use std::mem::{offset_of, size_of};
use std::slice;
use std::time::Instant;

use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, HANDLE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::{GUID, PCWSTR, w};

use crate::{Diagnostic, DiagnosticKind};

pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl OwnedHandle {
    pub(crate) fn open_device(path: &[u16], access: u32) -> windows::core::Result<Self> {
        // Paths are NUL-terminated buffers returned by the private SetupDi iterator.
        debug_assert_eq!(path.last(), Some(&0));
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map(Self)
    }
}

struct DeviceSet(HDEVINFO);

pub(crate) struct NamedMutex(OwnedHandle);

impl NamedMutex {
    pub(crate) fn msi_software(operation: &'static str) -> Result<Self, Diagnostic> {
        Self::try_acquire(w!(r"Global\MSILT.MsiSoftware"), operation)
    }

    pub(crate) fn try_acquire(name: PCWSTR, operation: &'static str) -> Result<Self, Diagnostic> {
        let handle = OwnedHandle(
            unsafe { CreateMutexW(None, false, name) }
                .map_err(|e| Diagnostic::windows(operation, e))?,
        );
        match unsafe { WaitForSingleObject(handle.0, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            WAIT_TIMEOUT => Err(Diagnostic::new(operation, DiagnosticKind::Busy)),
            _ => Err(Diagnostic::last_win32(operation)),
        }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0.0) };
    }
}

impl Drop for DeviceSet {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

const MAX_INTERFACES: u32 = 1_024;
const MAX_DETAIL_BYTES: u32 = 64 * 1024;

pub(crate) fn device_interfaces(
    class: &GUID,
    deadline: Instant,
    mut visit: impl FnMut(Result<Vec<u16>, Diagnostic>),
) -> Result<(), Diagnostic> {
    let set = DeviceSet(
        unsafe {
            SetupDiGetClassDevsW(
                Some(class),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(|e| Diagnostic::windows("SetupDiGetClassDevsW", e))?,
    );

    for index in 0..=MAX_INTERFACES {
        crate::executor::before_deadline(deadline, "SetupDiEnumDeviceInterfaces")?;
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        match unsafe { SetupDiEnumDeviceInterfaces(set.0, None, class, index, &mut interface) } {
            Ok(()) if index < MAX_INTERFACES => visit(device_path(&set, &interface)),
            Ok(()) => {
                return Err(Diagnostic::invalid(
                    "SetupDiEnumDeviceInterfaces",
                    "too many interfaces",
                ));
            }
            Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => return Ok(()),
            Err(e) => return Err(Diagnostic::windows("SetupDiEnumDeviceInterfaces", e)),
        }
    }
    unreachable!()
}

fn device_path(
    set: &DeviceSet,
    interface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<Vec<u16>, Diagnostic> {
    const OP: &str = "SetupDiGetDeviceInterfaceDetailW";
    const OFFSET: usize = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
    let mut required = 0;
    match unsafe {
        SetupDiGetDeviceInterfaceDetailW(set.0, interface, None, 0, Some(&mut required), None)
    } {
        Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => (),
        Err(e) => return Err(Diagnostic::windows(OP, e)),
        Ok(()) => return Err(Diagnostic::invalid(OP, "missing detail buffer size")),
    }
    if !(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32..=MAX_DETAIL_BYTES)
        .contains(&required)
        || required % 2 != 0
    {
        return Err(Diagnostic::invalid(OP, "invalid interface detail size"));
    }
    // DevicePath starts at offset 4 even when cbSize includes tail padding on x64.
    let mut buffer = vec![0u32; (required as usize).div_ceil(size_of::<u32>())];
    buffer[0] = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    let mut written = 0;
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set.0,
            interface,
            Some(detail),
            required,
            Some(&mut written),
            None,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    if written > required || written as usize <= OFFSET || written % 2 != 0 {
        return Err(Diagnostic::invalid(OP, "invalid returned detail size"));
    }
    let path = unsafe {
        slice::from_raw_parts(
            detail.cast::<u8>().add(OFFSET).cast::<u16>(),
            (written as usize - OFFSET) / 2,
        )
    };
    let end = path
        .iter()
        .position(|&c| c == 0)
        .filter(|&i| i > 0)
        .ok_or_else(|| {
            Diagnostic::new(OP, DiagnosticKind::InvalidData).detail("missing interface path")
        })?;
    Ok(path[..=end].to_vec())
}

pub(crate) fn wide_str(words: &[u16]) -> Option<String> {
    let end = words.iter().position(|&word| word == 0)?;
    String::from_utf16(&words[..end]).ok()
}
