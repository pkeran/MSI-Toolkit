use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use msilt_core::ipc::PIPE_NAME;

use crate::client::Transport;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER: i32 = 1;
const TOKEN_SESSION_ID: i32 = 12;
const TOKEN_ELEVATION: i32 = 20;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SEM_TIMEOUT: i32 = 121;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_IO_PENDING: i32 = 997;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const CSTR_EQUAL: i32 = 2;

pub struct Pipe {
    file: File,
    event: OwnedHandle,
    deadline: Instant,
}

impl Pipe {
    pub fn connect() -> io::Result<Self> {
        let file = open_pipe(PIPE_NAME).map_err(connect_error)?;
        verify_peer(file.as_raw_handle()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("daemon identity check failed: {error}"),
            )
        })?;
        Self::from_file(file)
    }

    fn from_file(file: File) -> io::Result<Self> {
        let event = owned(unsafe { CreateEventW(null_mut(), 1, 0, null()) })?;
        Ok(Self {
            file,
            event,
            deadline: Instant::now(),
        })
    }

    fn transfer(&mut self, buffer: Transfer<'_>) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        wait_millis(self.deadline)?;
        let file = self.file.as_raw_handle();
        let event = self.event.as_raw_handle();
        let mut op = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event,
        };
        let op = &raw mut op;
        // `op` and the buffer stay valid until the kernel operation has been reaped.
        let complete = unsafe {
            check(ResetEvent(event))?;
            match buffer {
                Transfer::Read(bytes) => ReadFile(
                    file,
                    bytes.as_mut_ptr().cast(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    op,
                ),
                Transfer::Write(bytes) => WriteFile(
                    file,
                    bytes.as_ptr().cast(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    op,
                ),
            }
        };
        let mut count = 0;
        if complete == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(error);
            }
            let wait = wait_millis(self.deadline).and_then(|ms| {
                match unsafe { WaitForSingleObject(event, ms) } {
                    WAIT_OBJECT_0 => Ok(()),
                    WAIT_TIMEOUT => Err(timeout()),
                    _ => Err(io::Error::last_os_error()),
                }
            });
            if let Err(error) = wait {
                // CancelIoEx requests cancellation; it does not complete the original operation.
                unsafe {
                    CancelIoEx(file, op);
                    GetOverlappedResult(file, op, &mut count, 1);
                }
                return Err(error);
            }
        }
        check(unsafe { GetOverlappedResult(file, op, &mut count, 1) })?;
        Ok(count as usize)
    }
}

enum Transfer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl Transfer<'_> {
    fn is_empty(&self) -> bool {
        match self {
            Self::Read(bytes) => bytes.is_empty(),
            Self::Write(bytes) => bytes.is_empty(),
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.transfer(Transfer::Read(bytes))
    }
}

impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.transfer(Transfer::Write(bytes))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Transport for Pipe {
    fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
}

fn open_pipe(name: &str) -> io::Result<File> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION);
    loop {
        match options.open(name) {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                let remaining = wait_millis(deadline)?;
                let wide = wide_z(OsStr::new(name));
                check(unsafe { WaitNamedPipeW(wide.as_ptr(), remaining) })?;
            }
            result => return result,
        }
    }
}

fn connect_error(error: io::Error) -> io::Error {
    let message = match error.raw_os_error() {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
            "MSI-Toolkit daemon is not running (pipe not found); start it in this login session"
        }
        Some(ERROR_ACCESS_DENIED) => {
            "daemon pipe access denied; use the authorized Windows login session"
        }
        Some(ERROR_SEM_TIMEOUT | ERROR_PIPE_BUSY) => "daemon pipe is busy; no request was sent",
        _ if error.kind() == io::ErrorKind::TimedOut => "daemon pipe is busy; no request was sent",
        _ => {
            return io::Error::new(
                error.kind(),
                format!("cannot open the daemon pipe: {error}"),
            );
        }
    };
    io::Error::new(error.kind(), message)
}

fn verify_peer(pipe: RawHandle) -> io::Result<()> {
    let mut pid = 0;
    check(unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) })?;
    let process =
        owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) })?;
    let server = process_token(process.as_raw_handle())?;
    let client = process_token(unsafe { GetCurrentProcess() })?;
    let mut pipe_session = 0;
    check(unsafe { GetNamedPipeServerSessionId(pipe, &mut pipe_session) })?;
    let session = token_dword(&client, TOKEN_SESSION_ID)?;
    if session == 0 || pipe_session != session || token_dword(&server, TOKEN_SESSION_ID)? != session
    {
        return Err(untrusted(
            "the daemon must run in the current interactive session",
        ));
    }
    if token_dword(&server, TOKEN_ELEVATION)? == 0 || !same_user(&client, &server)? {
        return Err(untrusted(
            "the daemon must be elevated and run as the current user",
        ));
    }
    verify_image(process.as_raw_handle())?;
    let mut current_pid = 0;
    check(unsafe { GetNamedPipeServerProcessId(pipe, &mut current_pid) })?;
    if current_pid != pid
        || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } != WAIT_TIMEOUT
    {
        return Err(untrusted("the daemon process changed while connecting"));
    }
    Ok(())
}

fn process_token(process: RawHandle) -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) })?;
    owned(token)
}

fn token_dword(token: &OwnedHandle, class: i32) -> io::Result<u32> {
    let mut value = 0u32;
    let mut returned = 0;
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            (&mut value as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut returned,
        )
    })?;
    Ok(value)
}

fn same_user(client: &OwnedHandle, server: &OwnedHandle) -> io::Result<bool> {
    // TOKEN_USER starts with a SID pointer into the aligned caller-owned buffer.
    let mut users = [[0usize; 24]; 2];
    for (token, buffer) in [client, server].into_iter().zip(&mut users) {
        let mut returned = 0;
        check(unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TOKEN_USER,
                buffer.as_mut_ptr().cast(),
                size_of_val(buffer) as u32,
                &mut returned,
            )
        })?;
    }
    Ok(unsafe { EqualSid(users[0][0] as *const c_void, users[1][0] as *const c_void) } != 0)
}

fn verify_image(process: RawHandle) -> io::Result<()> {
    let install = configured_install_dir()?;
    verify_no_reparse_ancestors(&install)?;
    let image = install.join("msilt-daemon.exe");
    let dir_info = fs::symlink_metadata(&install)?;
    let image_info = fs::symlink_metadata(&image)?;
    if !dir_info.is_dir()
        || !image_info.is_file()
        || (dir_info.file_attributes() | image_info.file_attributes())
            & FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(untrusted(
            "the daemon installation must not use reparse points",
        ));
    }
    verify_process_image(process, &fs::canonicalize(&image)?)
}

fn configured_install_dir() -> io::Result<PathBuf> {
    for (subkey, value) in [
        (r"SOFTWARE\MSI-Toolkit", "InstallDir"),
        (
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit_is1",
            "InstallLocation",
        ),
        (
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit",
            "InstallLocation",
        ),
    ] {
        if let Some(raw) = read_hklm_string(subkey, value) {
            let trimmed = raw.trim().trim_end_matches(['\\', '/']);
            let path = PathBuf::from(trimmed);
            if is_valid_local_dir(&path) {
                return Ok(path);
            }
        }
    }
    Ok(program_files()?.join("MSI-Toolkit"))
}

fn is_valid_local_dir(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    matches!(comps.next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)))
        && comps.next() == Some(Component::RootDir)
        && comps.clone().next().is_some()
        && comps.all(|c| matches!(c, Component::Normal(_)))
}

fn verify_no_reparse_ancestors(install: &Path) -> io::Result<()> {
    use std::path::Component;
    let mut comps = install.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return Err(untrusted("install path is not a local drive path"));
    };
    if comps.next() != Some(Component::RootDir) {
        return Err(untrusted("install path has no root directory"));
    }
    let mut cur = PathBuf::from(prefix.as_os_str());
    cur.push(r"\");
    for part in comps {
        let Component::Normal(name) = part else {
            return Err(untrusted("install path contains a relative component"));
        };
        cur.push(name);
        let meta = fs::symlink_metadata(&cur)?;
        if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(untrusted(
                "the daemon installation must not use reparse points",
            ));
        }
    }
    Ok(())
}

fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
    const HKEY_LOCAL_MACHINE: isize = -2147483646; // 0x80000002
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    let subkey_w = wide_z(OsStr::new(subkey));
    let value_w = wide_z(OsStr::new(value));
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut bytes,
        )
    };
    if status != 0 || !(2..=65_536).contains(&bytes) {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    (!text.is_empty()).then_some(text)
}

fn verify_process_image(process: RawHandle, expected: &Path) -> io::Result<()> {
    let expected: Vec<_> = expected.as_os_str().encode_wide().collect();
    let expected = expected
        .strip_prefix(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
        .unwrap_or(&expected);
    let mut actual = vec![0u16; 32_768];
    let mut len = actual.len() as u32;
    check(unsafe { QueryFullProcessImageNameW(process, 0, actual.as_mut_ptr(), &mut len) })?;
    if unsafe {
        CompareStringOrdinal(
            actual.as_ptr(),
            len as i32,
            expected.as_ptr(),
            expected.len() as i32,
            1,
        )
    } != CSTR_EQUAL
    {
        return Err(untrusted(
            "the daemon image must be the installed Program Files executable",
        ));
    }
    Ok(())
}

fn program_files() -> io::Result<PathBuf> {
    let mut raw = null_mut();
    let result = unsafe { SHGetKnownFolderPath(&FOLDERID_PROGRAM_FILES, 0, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        unsafe { CoTaskMemFree(raw.cast()) };
        return Err(io::Error::other(format!(
            "cannot resolve Program Files (HRESULT 0x{result:08X})"
        )));
    }
    // SHGetKnownFolderPath returns a NUL-terminated UTF-16 buffer owned by the caller.
    let folder = unsafe {
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)));
        CoTaskMemFree(raw.cast());
        path
    };
    match folder.components().next() {
        Some(std::path::Component::Prefix(part))
            if matches!(part.kind(), std::path::Prefix::Disk(_)) && folder.is_absolute() =>
        {
            Ok(folder)
        }
        _ => Err(untrusted("Program Files is not a local drive path")),
    }
}

fn untrusted(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

fn wide_z(name: &OsStr) -> Vec<u16> {
    name.encode_wide().chain(std::iter::once(0)).collect()
}

fn wait_millis(deadline: Instant) -> io::Result<u32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout());
    }
    Ok(remaining
        .as_nanos()
        .div_ceil(1_000_000)
        .min(u128::from(u32::MAX - 1)) as u32)
}

fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "daemon pipe I/O deadline elapsed")
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn owned(handle: RawHandle) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == (-1isize as RawHandle) {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: RawHandle,
}

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const FOLDERID_PROGRAM_FILES: Guid = Guid {
    data1: 0x905e_63b6,
    data2: 0xc1bf,
    data3: 0x494e,
    data4: [0xb2, 0x9c, 0x65, 0xb7, 0x32, 0xd3, 0xd2, 0x1a],
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
    fn CreateEventW(
        attributes: *mut c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> RawHandle;
    fn ResetEvent(event: RawHandle) -> i32;
    fn ReadFile(
        file: RawHandle,
        buffer: *mut c_void,
        len: u32,
        read: *mut u32,
        op: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        file: RawHandle,
        buffer: *const c_void,
        len: u32,
        written: *mut u32,
        op: *mut Overlapped,
    ) -> i32;
    fn GetOverlappedResult(
        file: RawHandle,
        op: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn CancelIoEx(file: RawHandle, op: *mut Overlapped) -> i32;
    fn WaitForSingleObject(handle: RawHandle, timeout: u32) -> u32;
    fn GetNamedPipeServerProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn GetNamedPipeServerSessionId(pipe: RawHandle, session: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> RawHandle;
    fn GetCurrentProcess() -> RawHandle;
    fn QueryFullProcessImageNameW(
        process: RawHandle,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CompareStringOrdinal(
        left: *const u16,
        lsize: i32,
        right: *const u16,
        rsize: i32,
        ignore_case: i32,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: RawHandle, access: u32, token: *mut RawHandle) -> i32;
    fn GetTokenInformation(
        token: RawHandle,
        class: i32,
        buffer: *mut c_void,
        length: u32,
        actual: *mut u32,
    ) -> i32;
    fn EqualSid(left: *const c_void, right: *const c_void) -> i32;
    fn RegGetValueW(
        hkey: isize,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        reg_type: *mut u32,
        data: *mut c_void,
        data_len: *mut u32,
    ) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetKnownFolderPath(
        folder: *const Guid,
        flags: u32,
        token: RawHandle,
        path: *mut *mut u16,
    ) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(ptr: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    fn pipe_pair() -> (File, Pipe) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\MSIToolkit-CLI-test-{pid}-{seq}");
        let wide = wide_z(OsStr::new(&name));
        let server = File::from(
            owned(unsafe {
                CreateNamedPipeW(wide.as_ptr(), 0x0008_0003, 8, 1, 1024, 1024, 0, null_mut())
            })
            .unwrap(),
        );
        let client = Pipe::from_file(open_pipe(&name).unwrap()).unwrap();
        let result = unsafe { ConnectNamedPipe(server.as_raw_handle(), null_mut()) };
        assert!(result != 0 || io::Error::last_os_error().raw_os_error() == Some(535)); // ERROR_PIPE_CONNECTED
        (server, client)
    }

    #[test]
    fn overlapped_local_pipe_reads_writes_and_reaps_a_timed_out_read() {
        let (mut server, mut client) = pipe_pair();
        let mut bytes = [0; 4];
        client.set_deadline(Instant::now() + Duration::from_millis(50));
        assert_eq!(
            client.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );

        client.set_deadline(Instant::now() + Duration::from_secs(2));
        client.write_all(b"ping").unwrap();
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
        check(unsafe { ImpersonateNamedPipeClient(server.as_raw_handle()) }).unwrap();
        let level = (|| {
            let mut token = null_mut();
            check(unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) })?;
            token_dword(&owned(token)?, 9) // TokenImpersonationLevel
        })();
        check(unsafe { RevertToSelf() }).unwrap();
        assert_eq!(level.unwrap(), 1); // SecurityIdentification
        server.write_all(b"pong").unwrap();
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"pong");
    }

    #[test]
    fn untrusted_pipe_peer_is_rejected() {
        let (_server, client) = pipe_pair();
        let mut pid = 0;
        check(unsafe { GetNamedPipeServerProcessId(client.file.as_raw_handle(), &mut pid) })
            .unwrap();
        assert_eq!(pid, std::process::id());
        let token = process_token(unsafe { GetCurrentProcess() }).unwrap();
        assert!(same_user(&token, &token).unwrap());
        assert!(verify_peer(client.file.as_raw_handle()).is_err());
    }

    #[test]
    fn process_image_requires_the_exact_canonical_path() {
        let process = unsafe { GetCurrentProcess() };
        let actual = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        verify_process_image(process, &actual).unwrap();
        assert_eq!(
            verify_process_image(process, &actual.with_file_name("msilt-daemon.exe"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied,
        );
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *mut c_void,
        ) -> RawHandle;
        fn ConnectNamedPipe(pipe: RawHandle, op: *mut Overlapped) -> i32;
        fn GetCurrentThread() -> RawHandle;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ImpersonateNamedPipeClient(pipe: RawHandle) -> i32;
        fn OpenThreadToken(
            thread: RawHandle,
            access: u32,
            open_as_self: i32,
            token: *mut RawHandle,
        ) -> i32;
        fn RevertToSelf() -> i32;
    }
}
