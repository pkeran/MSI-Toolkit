use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::CoCreateGuid;
use windows::core::{PCWSTR, w};

use crate::native::OwnedHandle;
use crate::{Diagnostic, DiagnosticKind};

use super::MAX_BACKUP_BYTES;

const OP: &str = "protected MSI software backup";
const DRIVE_FIXED: u32 = 3;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>, Diagnostic> {
    let _parents = lock_parents(path)?;
    let Some(file) = open(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_BACKUP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Diagnostic::file(OP, e))?;
    if bytes.len() > MAX_BACKUP_BYTES {
        return Err(Diagnostic::invalid(OP, "snapshot exceeds size limit"));
    }
    Ok(Some(bytes))
}

fn open(path: &Path) -> Result<Option<File>, Diagnostic> {
    let words = native_path(path)?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(words.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    };
    let file = match handle {
        Ok(handle) => unsafe { File::from_raw_handle(handle.0) },
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(None),
        Err(e) => return Err(Diagnostic::windows(OP, e)),
    };
    verify_file(HANDLE(file.as_raw_handle()))?;
    Ok(Some(file))
}

pub(super) fn create(path: &Path, bytes: &[u8]) -> Result<(), Diagnostic> {
    let _parents = lock_parents(path)?;
    let words = native_path(path)?;
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        ..Default::default()
    };
    let guid = unsafe { CoCreateGuid() }.map_err(|e| Diagnostic::windows(OP, e))?;
    let temporary = path.with_file_name(format!(".msilt-backup-{:032x}.tmp", guid.to_u128()));
    let temporary_name = native_path(&temporary)?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(temporary_name.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_MODE(0),
            Some(&attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    let result = (|| {
        let mut file = unsafe { File::from_raw_handle(handle.0) };
        verify_file(HANDLE(file.as_raw_handle()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| Diagnostic::file(OP, e))?;
        drop(file);
        // Publishing never replaces an original recovery snapshot.
        unsafe {
            MoveFileExW(
                PCWSTR(temporary_name.as_ptr()),
                PCWSTR(words.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| Diagnostic::windows(OP, e))?;
        let mut published =
            open(path)?.ok_or_else(|| Diagnostic::invalid(OP, "published snapshot is missing"))?;
        let mut check = Vec::new();
        (&mut published)
            .take(bytes.len() as u64 + 1)
            .read_to_end(&mut check)
            .map_err(|e| Diagnostic::file(OP, e))?;
        if check != bytes {
            return Err(Diagnostic::invalid(
                OP,
                "published snapshot does not match the original",
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn native_path(path: &Path) -> Result<Vec<u16>, Diagnostic> {
    let words: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    if words[..words.len() - 1].contains(&0) || words.len() > 32_767 {
        return Err(Diagnostic::invalid(
            OP,
            "invalid Windows path length or contents",
        ));
    }
    Ok(words)
}

pub(crate) fn lock_parents(path: &Path) -> Result<Vec<OwnedHandle>, Diagnostic> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(Diagnostic::invalid(
            OP,
            "backup requires an absolute local path",
        ));
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        || components.next() != Some(Component::RootDir)
    {
        return Err(Diagnostic::invalid(
            OP,
            "backup requires a local drive path",
        ));
    }
    let mut parent = PathBuf::from(prefix.as_os_str());
    parent.push(r"\");
    let root = native_path(&parent)?;
    if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != DRIVE_FIXED {
        return Err(Diagnostic::invalid(
            OP,
            "backup requires a fixed local drive",
        ));
    }
    let mut handles = vec![open_parent(&root)?];
    let mut components = components.peekable();
    if components.peek().is_none() {
        return Err(Diagnostic::invalid(OP, "backup file name missing"));
    }
    while let Some(Component::Normal(name)) = components.next() {
        let text = name.to_string_lossy();
        if text.contains([':', '\\', '/']) || text.ends_with([' ', '.']) {
            return Err(Diagnostic::invalid(
                OP,
                "backup path has an ambiguous component",
            ));
        }
        if components.peek().is_none() {
            return Ok(handles);
        }
        parent.push(name);
        handles.push(open_parent(&native_path(&parent)?)?);
    }
    Err(Diagnostic::invalid(
        OP,
        "backup path contains a relative component",
    ))
}

fn open_parent(path: &[u16]) -> Result<OwnedHandle, Diagnostic> {
    let handle = OwnedHandle(
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|e| Diagnostic::windows(OP, e))?,
    );
    let info = file_info(handle.0)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT).0
        != FILE_ATTRIBUTE_DIRECTORY.0
    {
        return Err(Diagnostic::invalid(
            OP,
            "backup parent is not a normal directory",
        ));
    }
    Ok(handle)
}

fn file_info(handle: HANDLE) -> Result<BY_HANDLE_FILE_INFORMATION, Diagnostic> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }
        .map_err(|e| Diagnostic::windows(OP, e))?;
    Ok(info)
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }
}

fn private_descriptor() -> Result<SecurityDescriptor, Diagnostic> {
    let mut descriptor = SecurityDescriptor(PSECURITY_DESCRIPTOR::default());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)"),
            1,
            &mut descriptor.0,
            None,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    Ok(descriptor)
}

fn verify_file(handle: HANDLE) -> Result<(), Diagnostic> {
    let info = file_info(handle)?;
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || info.nNumberOfLinks != 1
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT).0 != 0
    {
        return Err(Diagnostic::invalid(
            OP,
            "backup is not a regular single-link file",
        ));
    }
    let mut owner = PSID::default();
    let mut acl = ptr::null_mut();
    let mut descriptor = SecurityDescriptor(PSECURITY_DESCRIPTOR::default());
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut acl),
            None,
            Some(&mut descriptor.0),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(Diagnostic::win32(OP, status.0));
    }
    let denied = || {
        Diagnostic::new(OP, DiagnosticKind::AccessDenied)
            .detail("backup requires an administrator/SYSTEM owner and a protected private ACL")
    };
    let mut control = 0;
    let mut revision = 0;
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
        .map_err(|e| Diagnostic::windows(OP, e))?;
    if acl.is_null() || control & SE_DACL_PROTECTED.0 == 0 || !admin_or_system(owner) {
        return Err(denied());
    }
    let entries = unsafe { (*acl).AceCount };
    if entries != 2 {
        return Err(denied());
    }
    let mut principals = 0;
    for i in 0..entries {
        let mut raw = ptr::null_mut();
        unsafe { GetAce(acl, u32::from(i), &mut raw) }.map_err(|e| Diagnostic::windows(OP, e))?;
        let ace = raw.cast::<ACCESS_ALLOWED_ACE>();
        let header = unsafe { &(*ace).Header };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || header.AceFlags != 0
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(denied());
        }
        let sid = PSID(unsafe { ptr::addr_of_mut!((*ace).SidStart) }.cast());
        principals |= principal(sid);
    }
    if principals != 3 {
        Err(denied())
    } else {
        Ok(())
    }
}

fn admin_or_system(sid: PSID) -> bool {
    principal(sid) != 0
}

fn principal(sid: PSID) -> u8 {
    if sid.0.is_null() {
        return 0;
    }
    if unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) }.as_bool() {
        1
    } else if unsafe { IsWellKnownSid(sid, WinLocalSystemSid) }.as_bool() {
        2
    } else {
        0
    }
}
