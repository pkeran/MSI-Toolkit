use std::ffi::{c_char, c_void};
use std::time::Instant;

use serde::Serialize;
use windows::Win32::Foundation::{FARPROC, FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{PCSTR, PCWSTR};

use crate::executor::before_deadline;
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

type Device = *mut c_void;
type Status = u32;
type Shutdown = unsafe extern "C" fn() -> Status;
const SUCCESS: Status = 0;
const NOT_SUPPORTED: Status = 3;
const NO_ATTACH: u32 = 2;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmlReport {
    pub driver_version: Option<String>,
    pub devices: Vec<GpuSample>,
    pub completed: bool,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuSample {
    pub index: u32,
    pub name: Option<String>,
    pub temperature_c: Option<u32>,
    pub power_mw: Option<u32>,
    pub gpu_utilization_percent: Option<u32>,
    pub memory_controller_utilization_percent: Option<u32>,
}

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

struct Library(Option<HMODULE>);

impl Library {
    fn open() -> Result<Self, Diagnostic> {
        let mut dir = vec![0; 512];
        let used = unsafe { GetSystemDirectoryW(Some(&mut dir)) } as usize;
        if used == 0 {
            return Err(Diagnostic::last_win32("GetSystemDirectoryW"));
        }
        if used >= dir.len() || dir[used] != 0 {
            return Err(Diagnostic::invalid(
                "GetSystemDirectoryW",
                "unexpected directory length",
            ));
        }
        dir.truncate(used);
        dir.extend(r"\nvml.dll".encode_utf16().chain([0]));
        // The absolute path and search flags exclude the working directory and PATH for dependencies.
        unsafe {
            LoadLibraryExW(
                PCWSTR(dir.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            )
        }
        .map(|module| Self(Some(module)))
        .map_err(|e| Diagnostic::windows("LoadLibraryExW(System32 nvml.dll)", e))
    }

    fn get(&self, name: &'static [u8]) -> FARPROC {
        unsafe { GetProcAddress(self.0.expect("loaded NVML library"), PCSTR(name.as_ptr())) }
    }

    fn close(&mut self) -> Result<(), Diagnostic> {
        if let Some(module) = self.0.take() {
            unsafe { FreeLibrary(module) }
                .map_err(|e| Diagnostic::windows("FreeLibrary(nvml.dll)", e))?;
        }
        Ok(())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct Session(Option<Shutdown>);

impl Session {
    fn close(&mut self) -> Result<(), Diagnostic> {
        if let Some(shutdown) = self.0.take() {
            check("nvmlShutdown", unsafe { shutdown() })?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

// Resolve only public NVML C ABI exports. Keep every typed pointer local to this library's scope.
macro_rules! symbol {
    ($lib:expr, $name:literal, $ty:ty) => {
        $lib.get(concat!($name, "\0").as_bytes())
            .map(|f| unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f) })
    };
}

fn missing(name: &'static str) -> Diagnostic {
    Diagnostic::new(name, DiagnosticKind::Unsupported).detail("NVML export is absent")
}

pub(crate) fn inspect(deadline: Instant) -> NvmlReport {
    let mut report = NvmlReport::default();
    let mut library = match Library::open() {
        Ok(library) => library,
        Err(error) => {
            report.diagnostics.push(error);
            return report;
        }
    };
    let mut session = Session(None);
    let result = (|| {
        before_deadline(deadline, "nvmlInitWithFlags")?;
        let shutdown =
            symbol!(library, "nvmlShutdown", Shutdown).ok_or_else(|| missing("nvmlShutdown"))?;
        type InitFlags = unsafe extern "C" fn(u32) -> Status;
        type Init = unsafe extern "C" fn() -> Status;
        let mut fallback = true;
        if let Some(init) = symbol!(library, "nvmlInitWithFlags", InitFlags) {
            let status = unsafe { init(NO_ATTACH) };
            if status != NOT_SUPPORTED {
                check("nvmlInitWithFlags", status)?;
                fallback = false;
            }
        }
        if fallback {
            before_deadline(deadline, "nvmlInit_v2")?;
            let init =
                symbol!(library, "nvmlInit_v2", Init).ok_or_else(|| missing("nvmlInit_v2"))?;
            check("nvmlInit_v2", unsafe { init() })?;
        }
        session.0 = Some(shutdown);
        sample(&library, &mut report, deadline)
    })();
    report.completed = result.is_ok();
    if let Err(error) = result {
        report.diagnostics.push(error);
    }
    for result in [session.close(), library.close()] {
        if let Err(error) = result {
            report.completed = false;
            report.diagnostics.push(error);
        }
    }
    report
}

fn sample(library: &Library, report: &mut NvmlReport, deadline: Instant) -> Result<(), Diagnostic> {
    type GetVersion = unsafe extern "C" fn(*mut c_char, u32) -> Status;
    type GetCount = unsafe extern "C" fn(*mut u32) -> Status;
    type GetHandle = unsafe extern "C" fn(u32, *mut Device) -> Status;
    type GetName = unsafe extern "C" fn(Device, *mut c_char, u32) -> Status;
    type GetTemp = unsafe extern "C" fn(Device, u32, *mut u32) -> Status;
    type GetPower = unsafe extern "C" fn(Device, *mut u32) -> Status;
    type GetUtil = unsafe extern "C" fn(Device, *mut Utilization) -> Status;
    let count = symbol!(library, "nvmlDeviceGetCount_v2", GetCount)
        .ok_or_else(|| missing("nvmlDeviceGetCount_v2"))?;
    let handle = symbol!(library, "nvmlDeviceGetHandleByIndex_v2", GetHandle)
        .ok_or_else(|| missing("nvmlDeviceGetHandleByIndex_v2"))?;
    let name = symbol!(library, "nvmlDeviceGetName", GetName);
    let temp = symbol!(library, "nvmlDeviceGetTemperature", GetTemp);
    let power = symbol!(library, "nvmlDeviceGetPowerUsage", GetPower);
    let util = symbol!(library, "nvmlDeviceGetUtilizationRates", GetUtil);
    let mut devices = 0;
    before_deadline(deadline, "nvmlDeviceGetCount_v2")?;
    check("nvmlDeviceGetCount_v2", unsafe { count(&mut devices) })?;
    if devices > 32 {
        return Err(Diagnostic::invalid(
            "nvmlDeviceGetCount_v2",
            "GPU count exceeds bound",
        ));
    }
    if devices == 0 {
        return Err(
            Diagnostic::new("nvmlDeviceGetCount_v2", DiagnosticKind::Unavailable)
                .detail("no NVML devices"),
        );
    }
    before_deadline(deadline, "nvmlSystemGetDriverVersion")?;
    report.driver_version = collect(
        symbol!(library, "nvmlSystemGetDriverVersion", GetVersion)
            .ok_or_else(|| missing("nvmlSystemGetDriverVersion"))
            .and_then(|f| read_text("nvmlSystemGetDriverVersion", |b, n| unsafe { f(b, n) })),
        &mut report.diagnostics,
    );
    for index in 0..devices {
        before_deadline(deadline, "nvmlDeviceGetHandleByIndex_v2")?;
        let mut device = std::ptr::null_mut();
        if let Err(error) = check("nvmlDeviceGetHandleByIndex_v2", unsafe {
            handle(index, &mut device)
        }) {
            report
                .diagnostics
                .push(error.detail(format!("GPU index {index}")));
            continue;
        }
        if device.is_null() {
            return Err(Diagnostic::invalid(
                "nvmlDeviceGetHandleByIndex_v2",
                "null GPU handle",
            ));
        }
        before_deadline(deadline, "nvmlDeviceGetName")?;
        let label = collect(
            name.ok_or_else(|| missing("nvmlDeviceGetName"))
                .and_then(|f| read_text("nvmlDeviceGetName", |b, n| unsafe { f(device, b, n) })),
            &mut report.diagnostics,
        );
        before_deadline(deadline, "nvmlDeviceGetTemperature")?;
        let temperature_c = collect(
            temp.ok_or_else(|| missing("nvmlDeviceGetTemperature"))
                .and_then(|f| {
                    let mut t = 0;
                    check("nvmlDeviceGetTemperature", unsafe { f(device, 0, &mut t) })?;
                    if t > 150 {
                        return Err(Diagnostic::invalid(
                            "nvmlDeviceGetTemperature",
                            "GPU temperature outside range",
                        ));
                    }
                    Ok(t)
                }),
            &mut report.diagnostics,
        );
        before_deadline(deadline, "nvmlDeviceGetPowerUsage")?;
        let power_mw = collect(
            power
                .ok_or_else(|| missing("nvmlDeviceGetPowerUsage"))
                .and_then(|f| {
                    let mut value = 0;
                    check("nvmlDeviceGetPowerUsage", unsafe { f(device, &mut value) })?;
                    Ok(value)
                }),
            &mut report.diagnostics,
        );
        before_deadline(deadline, "nvmlDeviceGetUtilizationRates")?;
        let utilization = collect(
            util.ok_or_else(|| missing("nvmlDeviceGetUtilizationRates"))
                .and_then(|f| {
                    let mut value = Utilization::default();
                    check("nvmlDeviceGetUtilizationRates", unsafe {
                        f(device, &mut value)
                    })?;
                    if value.gpu > 100 || value.memory > 100 {
                        return Err(Diagnostic::invalid(
                            "nvmlDeviceGetUtilizationRates",
                            "utilization outside 0..=100",
                        ));
                    }
                    Ok(value)
                }),
            &mut report.diagnostics,
        );
        report.devices.push(GpuSample {
            index,
            name: label,
            temperature_c,
            power_mw,
            gpu_utilization_percent: utilization.as_ref().map(|u| u.gpu),
            memory_controller_utilization_percent: utilization.map(|u| u.memory),
        });
    }
    if report.devices.is_empty() {
        return Err(Diagnostic::new("NVML devices", DiagnosticKind::Unavailable));
    }
    Ok(())
}

fn collect<T>(result: Result<T, Diagnostic>, diagnostics: &mut Vec<Diagnostic>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(error);
            None
        }
    }
}

fn read_text(
    operation: &'static str,
    f: impl FnOnce(*mut c_char, u32) -> Status,
) -> Result<String, Diagnostic> {
    let mut buffer = [0xffu8; 128];
    check(
        operation,
        f(buffer.as_mut_ptr().cast(), buffer.len() as u32),
    )?;
    let end = buffer
        .iter()
        .position(|&b| b == 0)
        .filter(|&end| end > 0)
        .ok_or_else(|| Diagnostic::invalid(operation, "missing NVML string terminator"))?;
    if !buffer[..end]
        .iter()
        .all(|b| b.is_ascii_graphic() || *b == b' ')
    {
        return Err(Diagnostic::invalid(
            operation,
            "unexpected NVML string characters",
        ));
    }
    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

fn check(operation: &'static str, status: Status) -> Result<(), Diagnostic> {
    let kind = match status {
        SUCCESS => return Ok(()),
        NOT_SUPPORTED | 13 => DiagnosticKind::Unsupported,
        4 => DiagnosticKind::AccessDenied,
        6 | 9 | 12 | 15 => DiagnosticKind::Unavailable,
        10 => DiagnosticKind::TimedOut,
        _ => DiagnosticKind::Io,
    };
    Err(Diagnostic::native(
        operation,
        kind,
        NativeDomain::Nvml,
        status,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_driver_failure_classification_are_bounded() {
        assert_eq!(
            check("nvml test", 4).unwrap_err().kind,
            DiagnosticKind::AccessDenied
        );
        assert_eq!(
            check("nvml test", 10).unwrap_err().kind,
            DiagnosticKind::TimedOut
        );
        assert!(read_text("nvml test", |_, _| SUCCESS).is_err());
        assert_eq!(
            read_text("nvml test", |ptr, len| {
                assert_eq!(len, 128);
                unsafe { std::ptr::copy_nonoverlapping(c"589.1".as_ptr(), ptr, 6) };
                SUCCESS
            })
            .unwrap(),
            "589.1"
        );
    }
}
