use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};

use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree, WIN32_ERROR};
use windows::core::Error;

pub(crate) fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

/// Takes an exclusively owned, closeable handle returned by a successful Win32 call.
pub(crate) unsafe fn own(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

pub(crate) fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

pub(crate) fn os_error(error: Error) -> io::Error {
    let code = error.code().0 as u32;
    if code >> 16 == 0x8007 {
        io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        io::Error::other(error.to_string())
    }
}

pub(crate) fn is_error(error: &Error, code: WIN32_ERROR) -> bool {
    error.code() == code.to_hresult()
}

pub(crate) fn denied(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

pub(crate) struct LocalAllocation(pub *mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}
