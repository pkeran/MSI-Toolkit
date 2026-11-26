//! Win32 transport; access and peer checks mirror msilt-cli/src/pipe.rs.
use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, File};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use msilt_core::{IpcCommand, IpcMessage, IpcRequest, decode_frame, encode_frame};
use super::Endpoint;

const PIPE_ACCESS: u32 = 0x0010_0083;
const PIPE_FLAGS: u32 = 0x4000_0000 | 0x0010_0000 | 0x0001_0000;
const WAIT_TIMEOUT: u32 = 258;
const INFINITE: u32 = u32::MAX;
const IO_PENDING: i32 = 997;
const REPARSE_POINT: u32 = 0x400;

pub struct Signals {
    stop: OwnedHandle,
    command: OwnedHandle,
    stopped: AtomicBool,
    pipe: Mutex<Option<Arc<File>>>,
}

impl Signals {
    pub fn new() -> io::Result<Self> {
        Ok(Self { stop: event(true)?, command: event(false)?, stopped: AtomicBool::new(false), pipe: Mutex::new(None) })
    }
    pub fn stopped(&self) -> bool { self.stopped.load(Ordering::Acquire) }
    pub fn wake(&self) { unsafe { SetEvent(self.command.as_raw_handle()); } }
    pub fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        unsafe { SetEvent(self.stop.as_raw_handle()); }
        if let Ok(pipe) = self.pipe.lock() {
            if let Some(pipe) = pipe.as_ref() { unsafe { CancelIoEx(pipe.as_raw_handle(), null_mut()); } }
        }
    }
}

pub fn run(endpoint: &Endpoint, signals: &Signals, requests: mpsc::Receiver<IpcRequest>, emit: impl FnMut(IpcMessage)) -> io::Result<()> {
    let pipe = Arc::new(open_pipe(&endpoint.name, signals)?);
    verify_peer(pipe.as_raw_handle(), endpoint.diagnostic)?;
    *signals.pipe.lock().map_err(|_| io::Error::other("IPC lock poisoned"))? = Some(pipe.clone());
    let result = drive(pipe, signals, requests, emit);
    if let Ok(mut slot) = signals.pipe.lock() { *slot = None; }
    result
}

fn open_pipe(name: &str, signals: &Signals) -> io::Result<File> {
    let name = wide_z(OsStr::new(name));
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if signals.stopped() { return Err(cancelled()); }
        // GENERIC_WRITE would also grant FILE_CREATE_PIPE_INSTANCE. Never request it.
        let handle = unsafe { CreateFileW(name.as_ptr(), PIPE_ACCESS, 0, null_mut(), 3, PIPE_FLAGS, null_mut()) };
        if handle != (-1isize as RawHandle) { return Ok(File::from(owned(handle)?)); }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(231) { return Err(error); }
        if Instant::now() >= deadline { return Err(timeout("管道繁忙")); }
        // Busy-connect backoff is cancellable; never block the event-loop thread.
        if unsafe { WaitForSingleObject(signals.stop.as_raw_handle(), 10) } == 0 { return Err(cancelled()); }
    }
}

fn drive(pipe: Arc<File>, signals: &Signals, requests: mpsc::Receiver<IpcRequest>, mut emit: impl FnMut(IpcMessage)) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    let mut read = Operation::new(pipe.clone())?;
    let mut pending = Vec::with_capacity(8192);
    let mut partial_since: Option<Instant> = None;
    let mut flight: Option<(u64, Instant)> = None;
    loop {
        if signals.stopped() { return Ok(()); }
        if flight.is_none() {
            if let Ok(request) = requests.try_recv() {
                let frame = encode_frame(&IpcMessage::Request(request.clone())).map_err(io::Error::other)?;
                write_frame(pipe.clone(), signals, &frame)?;
                let seconds = if matches!(request.command, IpcCommand::Hello { .. } | IpcCommand::SetSubscriptions { .. }) { 5 } else { 20 };
                flight = Some((request.id, Instant::now() + Duration::from_secs(seconds)));
            }
        }
        if !read.pending { read.start_read(&mut chunk)?; }
        let now = Instant::now();
        let deadline = [flight.map(|(_, at)| at), partial_since.map(|at| at + Duration::from_secs(5))].into_iter().flatten().min();
        if deadline.is_some_and(|at| now >= at) { return Err(timeout("IPC 应答或分片接收超时")); }
        let millis = deadline.map_or(INFINITE, |at| at.saturating_duration_since(now).as_millis().max(1).min(u128::from(INFINITE - 1)) as u32);
        let handles = [signals.stop.as_raw_handle(), signals.command.as_raw_handle(), read.event.as_raw_handle()];
        match unsafe { WaitForMultipleObjects(3, handles.as_ptr(), 0, millis) } {
            0 => return Ok(()),
            1 => continue,
            2 => {
                let count = read.finish()?;
                if count == 0 { return Ok(()); }
                pending.extend_from_slice(&chunk[..count]);
                loop {
                    let Some((message, used)) = decode_frame(&pending).map_err(io::Error::other)? else { break; };
                    match &message {
                        IpcMessage::Response(response) if flight.is_some_and(|(id, _)| id == response.id) => { flight = None; }
                        IpcMessage::Event(_) => {}
                        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC 方向或应答 ID 不匹配")),
                    }
                    pending.drain(..used);
                    emit(message);
                }
                if pending.is_empty() { partial_since = None; }
                else { partial_since.get_or_insert(Instant::now()); }
            }
            WAIT_TIMEOUT => return Err(timeout("IPC 应答或分片接收超时")),
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

fn write_frame(pipe: Arc<File>, signals: &Signals, bytes: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut offset = 0;
    while offset < bytes.len() {
        if signals.stopped() { return Err(cancelled()); }
        let mut op = Operation::new(pipe.clone())?;
        op.start_write(&bytes[offset..])?;
        let handles = [signals.stop.as_raw_handle(), op.event.as_raw_handle()];
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err(timeout("IPC 发送超时")); }
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, remaining.as_millis().max(1) as u32) } {
            0 => return Err(cancelled()),
            1 => {
                let count = op.finish()?;
                if count == 0 { return Err(io::Error::new(io::ErrorKind::WriteZero, "IPC 写入为零")); }
                offset += count;
            }
            WAIT_TIMEOUT => return Err(timeout("IPC 发送超时")),
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(())
}

struct Operation {
    pipe: Arc<File>,
    event: OwnedHandle,
    op: Box<Overlapped>,
    pending: bool,
}

impl Operation {
    fn new(pipe: Arc<File>) -> io::Result<Self> {
        let event = event(true)?;
        let op = Box::new(Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: event.as_raw_handle() });
        Ok(Self { pipe, event, op, pending: false })
    }
    fn start_read(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        check(unsafe { ResetEvent(self.event.as_raw_handle()) })?;
        let result = unsafe { ReadFile(self.pipe.as_raw_handle(), bytes.as_mut_ptr().cast(), bytes.len() as u32, null_mut(), &mut *self.op) };
        self.started(result)
    }
    fn start_write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let result = unsafe { WriteFile(self.pipe.as_raw_handle(), bytes.as_ptr().cast(), bytes.len() as u32, null_mut(), &mut *self.op) };
        self.started(result)
    }
    fn started(&mut self, result: i32) -> io::Result<()> {
        if result == 0 && io::Error::last_os_error().raw_os_error() != Some(IO_PENDING) { return Err(io::Error::last_os_error()); }
        self.pending = true;
        Ok(())
    }
    fn finish(&mut self) -> io::Result<usize> {
        let mut count = 0;
        let result = unsafe { GetOverlappedResult(self.pipe.as_raw_handle(), &mut *self.op, &mut count, 1) };
        self.pending = false;
        check(result)?;
        Ok(count as usize)
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if self.pending {
            // Both OVERLAPPED and buffer must survive until the cancelled I/O completes.
            let mut count = 0;
            unsafe {
                CancelIoEx(self.pipe.as_raw_handle(), &mut *self.op);
                GetOverlappedResult(self.pipe.as_raw_handle(), &mut *self.op, &mut count, 1);
            }
        }
    }
}

fn verify_peer(pipe: RawHandle, diagnostic: bool) -> io::Result<()> {
    let mut pid = 0;
    check(unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) })?;
    let process = owned(unsafe { OpenProcess(0x0010_1000, 0, pid) })?;
    let server = process_token(process.as_raw_handle())?;
    let client = process_token(unsafe { GetCurrentProcess() })?;
    let session = token_dword(&client, 12)?;
    let mut pipe_session = 0;
    check(unsafe { GetNamedPipeServerSessionId(pipe, &mut pipe_session) })?;
    if session == 0 || pipe_session != session || token_dword(&server, 12)? != session || !same_user(&client, &server)? {
        return Err(untrusted("daemon 必须属于当前用户与交互登录会话"));
    }
    let image = if diagnostic {
        // Explicit local diagnostics accept only the daemon next to this UI, and are read-only.
        std::env::current_exe()?.with_file_name("msilt-daemon.exe")
    } else {
        if token_dword(&server, 20)? == 0 { return Err(untrusted("生产 daemon 必须已提升")); }
        program_files()?.join("MSI-Toolkit").join("msilt-daemon.exe")
    };
    verify_image(process.as_raw_handle(), &image)?;
    let mut current_pid = 0;
    check(unsafe { GetNamedPipeServerProcessId(pipe, &mut current_pid) })?;
    if current_pid != pid || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } != WAIT_TIMEOUT {
        return Err(untrusted("连接期间 daemon 进程已更换或退出"));
    }
    Ok(())
}

fn process_token(process: RawHandle) -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    check(unsafe { OpenProcessToken(process, 8, &mut token) })?;
    owned(token)
}

fn token_dword(token: &OwnedHandle, class: i32) -> io::Result<u32> {
    let mut value = 0u32;
    let mut returned = 0;
    check(unsafe { GetTokenInformation(token.as_raw_handle(), class, (&mut value as *mut u32).cast(), 4, &mut returned) })?;
    Ok(value)
}

fn same_user(client: &OwnedHandle, server: &OwnedHandle) -> io::Result<bool> {
    let mut users = [[0usize; 24]; 2];
    for (token, buffer) in [client, server].into_iter().zip(&mut users) {
        let mut returned = 0;
        check(unsafe { GetTokenInformation(token.as_raw_handle(), 1, buffer.as_mut_ptr().cast(), size_of_val(buffer) as u32, &mut returned) })?;
    }
    Ok(unsafe { EqualSid(users[0][0] as *const c_void, users[1][0] as *const c_void) } != 0)
}

fn verify_image(process: RawHandle, image: &Path) -> io::Result<()> {
    for path in [image.parent().ok_or_else(|| untrusted("daemon 缺少安装目录"))?, image] {
        if fs::symlink_metadata(path)?.file_attributes() & REPARSE_POINT != 0 { return Err(untrusted("daemon 安装路径不能是重解析点")); }
    }
    if !image.is_file() { return Err(untrusted("daemon 镜像不是文件")); }
    verify_process_image(process, &fs::canonicalize(image)?)
}

fn verify_process_image(process: RawHandle, expected: &Path) -> io::Result<()> {
    let expected: Vec<_> = expected.as_os_str().encode_wide().collect();
    let expected = expected.strip_prefix(&[92, 92, 63, 92]).unwrap_or(&expected);
    let mut actual = vec![0u16; 32768];
    let mut len = actual.len() as u32;
    check(unsafe { QueryFullProcessImageNameW(process, 0, actual.as_mut_ptr(), &mut len) })?;
    if unsafe { CompareStringOrdinal(actual.as_ptr(), len as i32, expected.as_ptr(), expected.len() as i32, 1) } != 2 {
        return Err(untrusted("daemon 镜像路径不匹配，握手已阻止"));
    }
    Ok(())
}

fn program_files() -> io::Result<PathBuf> {
    let mut raw = null_mut();
    let result = unsafe { SHGetKnownFolderPath(&FOLDERID_PROGRAM_FILES, 0, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() { unsafe { CoTaskMemFree(raw.cast()); } return Err(untrusted("无法解析受保护的 Program Files")); }
    let folder = unsafe {
        let mut len = 0;
        while *raw.add(len) != 0 { len += 1; }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)));
        CoTaskMemFree(raw.cast());
        path
    };
    match folder.components().next() {
        Some(std::path::Component::Prefix(part)) if matches!(part.kind(), std::path::Prefix::Disk(_)) && folder.is_absolute() => Ok(folder),
        _ => Err(untrusted("Program Files 必须是本地驱动器路径")),
    }
}

fn cancelled() -> io::Error { io::Error::new(io::ErrorKind::Interrupted, "IPC 已取消") }
fn timeout(text: &'static str) -> io::Error { io::Error::new(io::ErrorKind::TimedOut, text) }
fn untrusted(text: &'static str) -> io::Error { io::Error::new(io::ErrorKind::PermissionDenied, text) }
fn wide_z(text: &OsStr) -> Vec<u16> { text.encode_wide().chain(Some(0)).collect() }
fn check(result: i32) -> io::Result<()> { if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) } }
fn event(manual: bool) -> io::Result<OwnedHandle> { owned(unsafe { CreateEventW(null_mut(), i32::from(manual), 0, null()) }) }
fn owned(handle: RawHandle) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == (-1isize as RawHandle) { Err(io::Error::last_os_error()) }
    else { Ok(unsafe { OwnedHandle::from_raw_handle(handle) }) }
}

#[repr(C)]
struct Overlapped { internal: usize, internal_high: usize, offset: u32, offset_high: u32, event: RawHandle }
#[repr(C)]
struct Guid { data1: u32, data2: u16, data3: u16, data4: [u8; 8] }
const FOLDERID_PROGRAM_FILES: Guid = Guid { data1: 0x905e63b6, data2: 0xc1bf, data3: 0x494e, data4: [0xb2, 0x9c, 0x65, 0xb7, 0x32, 0xd3, 0xd2, 0x1a] };

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, security: *mut c_void, disposition: u32, flags: u32, template: RawHandle) -> RawHandle;
    fn CreateEventW(attributes: *mut c_void, manual: i32, initial: i32, name: *const u16) -> RawHandle;
    fn SetEvent(event: RawHandle) -> i32;
    fn ResetEvent(event: RawHandle) -> i32;
    fn ReadFile(file: RawHandle, buffer: *mut c_void, len: u32, read: *mut u32, op: *mut Overlapped) -> i32;
    fn WriteFile(file: RawHandle, buffer: *const c_void, len: u32, written: *mut u32, op: *mut Overlapped) -> i32;
    fn GetOverlappedResult(file: RawHandle, op: *mut Overlapped, transferred: *mut u32, wait: i32) -> i32;
    fn CancelIoEx(file: RawHandle, op: *mut Overlapped) -> i32;
    fn WaitForSingleObject(handle: RawHandle, timeout: u32) -> u32;
    fn WaitForMultipleObjects(count: u32, handles: *const RawHandle, all: i32, timeout: u32) -> u32;
    fn GetNamedPipeServerProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn GetNamedPipeServerSessionId(pipe: RawHandle, session: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> RawHandle;
    fn GetCurrentProcess() -> RawHandle;
    fn QueryFullProcessImageNameW(process: RawHandle, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    fn CompareStringOrdinal(left: *const u16, lsize: i32, right: *const u16, rsize: i32, ignore_case: i32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: RawHandle, access: u32, token: *mut RawHandle) -> i32;
    fn GetTokenInformation(token: RawHandle, class: i32, buffer: *mut c_void, length: u32, actual: *mut u32) -> i32;
    fn EqualSid(left: *const c_void, right: *const c_void) -> i32;
}
#[link(name = "shell32")]
unsafe extern "system" { fn SHGetKnownFolderPath(folder: *const Guid, flags: u32, token: RawHandle, path: *mut *mut u16) -> i32; }
#[link(name = "ole32")]
unsafe extern "system" { fn CoTaskMemFree(ptr: *mut c_void); }
