use std::ffi::{c_char, c_void};
use std::time::Instant;

use msilt_core::GpuClockOffsetState;
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
const GRAPHICS: u32 = 0;
const MEMORY: u32 = 2;

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
    pub power_limit_mw: Option<u32>,
    pub graphics_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
    pub vram_used_mb: Option<u32>,
    pub vram_total_mb: Option<u32>,
    pub clock_offsets: Option<GpuClockOffsetState>,
}

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct NvmlClockOffsetV1 {
    version: u32,
    type_: u32,
    pstate: u32,
    clock_offset_mhz: i32,
    min_clock_offset_mhz: i32,
    max_clock_offset_mhz: i32,
}

impl NvmlClockOffsetV1 {
    fn new(type_: u32) -> Self {
        Self {
            version: (1 << 24) | std::mem::size_of::<Self>() as u32,
            type_,
            pstate: 0,
            ..Default::default()
        }
    }
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

/// Lives only on its owning HAL thread; shutdown precedes unloading the function pointers.
pub struct NvmlSession {
    session: Session,
    library: Library,
    devices: u32,
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

impl NvmlSession {
    pub fn open(deadline: Instant) -> Result<Self, Diagnostic> {
        before_deadline(deadline, "nvmlInit_v2")?;
        let library = Library::open()?;
        let shutdown = symbol!(library, "nvmlShutdown", Shutdown)
            .ok_or_else(|| missing("nvmlShutdown"))?;
        type Init = unsafe extern "C" fn() -> Status;
        let init = symbol!(library, "nvmlInit_v2", Init)
            .ok_or_else(|| missing("nvmlInit_v2"))?;
        before_deadline(deadline, "nvmlInit_v2")?;
        check("nvmlInit_v2", unsafe { init() })?;
        let session = Session(Some(shutdown));
        let devices = device_count(&library, deadline)?;
        Ok(Self { session, library, devices })
    }

    fn close(&mut self) -> Result<(), Diagnostic> {
        let shutdown = self.session.close();
        let unload = self.library.close();
        shutdown.and(unload)
    }

    fn device(&self, index: u32, deadline: Instant) -> Result<Device, Diagnostic> {
        type GetHandle = unsafe extern "C" fn(u32, *mut Device) -> Status;
        let handle = symbol!(self.library, "nvmlDeviceGetHandleByIndex_v2", GetHandle)
            .ok_or_else(|| missing("nvmlDeviceGetHandleByIndex_v2"))?;
        before_deadline(deadline, "nvmlDeviceGetHandleByIndex_v2")?;
        let mut device = std::ptr::null_mut();
        check("nvmlDeviceGetHandleByIndex_v2", unsafe { handle(index, &mut device) })?;
        if device.is_null() {
            return Err(Diagnostic::invalid("nvmlDeviceGetHandleByIndex_v2", "null GPU handle"));
        }
        Ok(device)
    }

    fn single_device(&self, deadline: Instant) -> Result<Device, Diagnostic> {
        if device_count(&self.library, deadline)? != 1 {
            return Err(Diagnostic::new("NVML GPU selection", DiagnosticKind::Unsupported)
                .detail("requires a single NVIDIA GPU"));
        }
        self.device(0, deadline)
    }

    pub fn sample(
        &self,
        deadline: Instant,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<GpuSample, Diagnostic> {
        self.single_device(deadline)?;
        self.sample_device(0, false, deadline, diagnostics)
    }

    fn sample_device(
        &self,
        index: u32,
        include_name: bool,
        deadline: Instant,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<GpuSample, Diagnostic> {
        type GetName = unsafe extern "C" fn(Device, *mut c_char, u32) -> Status;
        type GetClock = unsafe extern "C" fn(Device, u32, *mut u32) -> Status;
        type GetScalar = unsafe extern "C" fn(Device, *mut u32) -> Status;
        type GetUtil = unsafe extern "C" fn(Device, *mut Utilization) -> Status;
        type GetMemory = unsafe extern "C" fn(Device, *mut NvmlMemory) -> Status;
        let library = &self.library;
        let device = self.device(index, deadline)?;
        let label = if include_name {
            before_deadline(deadline, "nvmlDeviceGetName")?;
            collect(
                symbol!(library, "nvmlDeviceGetName", GetName)
                    .ok_or_else(|| missing("nvmlDeviceGetName"))
                    .and_then(|f| read_text("nvmlDeviceGetName", |b, n| unsafe { f(device, b, n) })),
                diagnostics,
            )
        } else {
            None
        };
        before_deadline(deadline, "nvmlDeviceGetTemperature")?;
        let temperature_c = collect(
            symbol!(library, "nvmlDeviceGetTemperature", GetClock)
                .ok_or_else(|| missing("nvmlDeviceGetTemperature"))
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
            diagnostics,
        );
        let scalar = |operation, f: Option<GetScalar>| {
            before_deadline(deadline, operation)?;
            let f = f.ok_or_else(|| missing(operation))?;
            let mut value = 0;
            check(operation, unsafe { f(device, &mut value) })?;
            Ok(value)
        };
        let power_mw = collect(
            scalar("nvmlDeviceGetPowerUsage", symbol!(library, "nvmlDeviceGetPowerUsage", GetScalar)),
            diagnostics,
        );
        let power_limit_mw = collect(
            scalar("nvmlDeviceGetEnforcedPowerLimit", symbol!(library, "nvmlDeviceGetEnforcedPowerLimit", GetScalar)),
            diagnostics,
        );
        before_deadline(deadline, "nvmlDeviceGetUtilizationRates")?;
        let utilization = collect(
            symbol!(library, "nvmlDeviceGetUtilizationRates", GetUtil)
                .ok_or_else(|| missing("nvmlDeviceGetUtilizationRates"))
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
            diagnostics,
        );
        let clock = |type_| {
            before_deadline(deadline, "nvmlDeviceGetClockInfo")?;
            let f = symbol!(library, "nvmlDeviceGetClockInfo", GetClock)
                .ok_or_else(|| missing("nvmlDeviceGetClockInfo"))?;
            let mut value = 0;
            check("nvmlDeviceGetClockInfo", unsafe { f(device, type_, &mut value) })?;
            Ok(value)
        };
        let graphics_clock_mhz = collect(clock(GRAPHICS), diagnostics);
        let memory_clock_mhz = collect(clock(MEMORY), diagnostics);
        let memory = collect((|| {
            before_deadline(deadline, "nvmlDeviceGetMemoryInfo")?;
            let f = symbol!(library, "nvmlDeviceGetMemoryInfo", GetMemory)
                .ok_or_else(|| missing("nvmlDeviceGetMemoryInfo"))?;
            let mut value = NvmlMemory::default();
            check("nvmlDeviceGetMemoryInfo", unsafe { f(device, &mut value) })?;
            memory_mb(value)
        })(), diagnostics);
        let clock_offsets = collect(self.read_offsets(device, deadline).map(bounded_offsets), diagnostics)
            .flatten();
        Ok(GpuSample {
            index,
            name: label,
            temperature_c,
            power_mw,
            gpu_utilization_percent: utilization.as_ref().map(|u| u.gpu),
            memory_controller_utilization_percent: utilization.map(|u| u.memory),
            power_limit_mw,
            graphics_clock_mhz,
            memory_clock_mhz,
            vram_used_mb: memory.map(|m| m.0),
            vram_total_mb: memory.map(|m| m.1),
            clock_offsets,
        })
    }

    fn read_offsets(&self, device: Device, deadline: Instant) -> Result<[NvmlClockOffsetV1; 2], Diagnostic> {
        type GetOffsets = unsafe extern "C" fn(Device, *mut NvmlClockOffsetV1) -> Status;
        let get = symbol!(self.library, "nvmlDeviceGetClockOffsets", GetOffsets)
            .ok_or_else(|| missing("nvmlDeviceGetClockOffsets"))?;
        let mut offsets = [NvmlClockOffsetV1::new(GRAPHICS), NvmlClockOffsetV1::new(MEMORY)];
        for offset in &mut offsets {
            before_deadline(deadline, "nvmlDeviceGetClockOffsets")?;
            check("nvmlDeviceGetClockOffsets", unsafe { get(device, offset) })?;
        }
        Ok(offsets)
    }

    pub fn clock_offsets(&self, deadline: Instant) -> Result<Option<GpuClockOffsetState>, Diagnostic> {
        self.read_offsets(self.single_device(deadline)?, deadline).map(bounded_offsets)
    }

    pub fn set_clock_offsets(&self, core_mhz: i16, mem_mhz: i16, deadline: Instant) -> Result<(i16, i16), Diagnostic> {
        validate_clock_offsets(core_mhz, mem_mhz)?;
        verify_clock_tools(core_mhz, mem_mhz, crate::identity::verify_gpu_clock_ownership)?;
        let device = self.single_device(deadline)?;
        let original = self.read_offsets(device, deadline)?;
        type SetOffsets = unsafe extern "C" fn(Device, *mut NvmlClockOffsetV1) -> Status;
        let set = symbol!(self.library, "nvmlDeviceSetClockOffsets", SetOffsets)
            .ok_or_else(|| missing("nvmlDeviceSetClockOffsets"))?;
        apply_offsets(original, core_mhz, mem_mhz, |mut offset, rollback| {
            if !rollback {
                before_deadline(deadline, "nvmlDeviceSetClockOffsets")?;
            }
            check("nvmlDeviceSetClockOffsets", unsafe { set(device, &mut offset) })
        }, || self.read_offsets(device, deadline))
    }

    #[cfg(test)]
    pub(crate) fn for_test(shutdown: Shutdown) -> Self {
        Self { session: Session(Some(shutdown)), library: Library(None), devices: 1 }
    }
}

impl Drop for NvmlSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn device_count(library: &Library, deadline: Instant) -> Result<u32, Diagnostic> {
    type GetCount = unsafe extern "C" fn(*mut u32) -> Status;
    let count = symbol!(library, "nvmlDeviceGetCount_v2", GetCount)
        .ok_or_else(|| missing("nvmlDeviceGetCount_v2"))?;
    let mut devices = 0;
    before_deadline(deadline, "nvmlDeviceGetCount_v2")?;
    check("nvmlDeviceGetCount_v2", unsafe { count(&mut devices) })?;
    if devices > 32 {
        return Err(Diagnostic::invalid("nvmlDeviceGetCount_v2", "GPU count exceeds bound"));
    }
    if devices == 0 {
        return Err(Diagnostic::new("nvmlDeviceGetCount_v2", DiagnosticKind::Unavailable)
            .detail("no NVML devices"));
    }
    Ok(devices)
}

pub(crate) fn inspect(deadline: Instant) -> NvmlReport {
    let mut report = NvmlReport::default();
    let mut session = match NvmlSession::open(deadline) {
        Ok(session) => session,
        Err(error) => {
            report.diagnostics.push(error);
            return report;
        }
    };
    type GetVersion = unsafe extern "C" fn(*mut c_char, u32) -> Status;
    report.driver_version = collect((|| {
        before_deadline(deadline, "nvmlSystemGetDriverVersion")?;
        let f = symbol!(session.library, "nvmlSystemGetDriverVersion", GetVersion)
            .ok_or_else(|| missing("nvmlSystemGetDriverVersion"))?;
        read_text("nvmlSystemGetDriverVersion", |b, n| unsafe { f(b, n) })
    })(), &mut report.diagnostics);
    for index in 0..session.devices {
        if let Some(gpu) = collect(session.sample_device(index, true, deadline, &mut report.diagnostics), &mut report.diagnostics) {
            report.devices.push(gpu);
        }
    }
    report.completed = !report.devices.is_empty();
    if let Err(error) = session.close() {
        report.completed = false;
        report.diagnostics.push(error);
    }
    report
}

pub fn set_clock_offsets(core_mhz: i16, mem_mhz: i16, deadline: Instant) -> Result<(i16, i16), Diagnostic> {
    validate_clock_offsets(core_mhz, mem_mhz)?;
    verify_clock_tools(core_mhz, mem_mhz, crate::identity::verify_gpu_clock_ownership)?;
    NvmlSession::open(deadline)?.set_clock_offsets(core_mhz, mem_mhz, deadline)
}

fn verify_clock_tools(core_mhz: i16, mem_mhz: i16, verify: impl FnOnce() -> Result<(), Diagnostic>) -> Result<(), Diagnostic> {
    if core_mhz != 0 || mem_mhz != 0 {
        verify()?;
    }
    Ok(())
}

pub(crate) fn validate_clock_offsets(core_mhz: i16, mem_mhz: i16) -> Result<(), Diagnostic> {
    if !(0..=200).contains(&core_mhz) || !(0..=200).contains(&mem_mhz) {
        return Err(Diagnostic::invalid("NVML clock offsets", "offset outside MSI envelope"));
    }
    Ok(())
}

fn memory_mb(memory: NvmlMemory) -> Result<(u32, u32), Diagnostic> {
    let mb = |bytes| u32::try_from(bytes / (1024 * 1024)).ok();
    match (mb(memory.used), mb(memory.total)) {
        (Some(used), Some(total)) if memory.used <= memory.total && memory.free <= memory.total => Ok((used, total)),
        _ => Err(Diagnostic::invalid("nvmlDeviceGetMemoryInfo", "invalid VRAM capacity")),
    }
}

fn bounded_offsets(offsets: [NvmlClockOffsetV1; 2]) -> Option<GpuClockOffsetState> {
    let bounds = |offset: NvmlClockOffsetV1| {
        let min = offset.min_clock_offset_mhz.max(0);
        let max = offset.max_clock_offset_mhz.min(200);
        (min <= max).then_some((min as i16, max as i16))
    };
    let (core_min_mhz, core_max_mhz) = bounds(offsets[0])?;
    let (mem_min_mhz, mem_max_mhz) = bounds(offsets[1])?;
    Some(GpuClockOffsetState {
        core_mhz: i16::try_from(offsets[0].clock_offset_mhz).ok()?,
        mem_mhz: i16::try_from(offsets[1].clock_offset_mhz).ok()?,
        core_min_mhz,
        core_max_mhz,
        mem_min_mhz,
        mem_max_mhz,
    })
}

fn apply_offsets(
    original: [NvmlClockOffsetV1; 2],
    core_mhz: i16,
    mem_mhz: i16,
    mut write: impl FnMut(NvmlClockOffsetV1, bool) -> Result<(), Diagnostic>,
    read: impl FnOnce() -> Result<[NvmlClockOffsetV1; 2], Diagnostic>,
) -> Result<(i16, i16), Diagnostic> {
    let bounds = bounded_offsets(original)
        .ok_or_else(|| Diagnostic::new("NVML clock offsets", DiagnosticKind::Unsupported))?;
    if !(bounds.core_min_mhz..=bounds.core_max_mhz).contains(&core_mhz)
        || !(bounds.mem_min_mhz..=bounds.mem_max_mhz).contains(&mem_mhz)
    {
        return Err(Diagnostic::invalid("NVML clock offsets", "offset outside MSI envelope"));
    }
    let mut requested = original;
    requested[0].clock_offset_mhz = i32::from(core_mhz);
    requested[1].clock_offset_mhz = i32::from(mem_mhz);
    write(requested[0], false)?;
    if let Err(error) = write(requested[1], false) {
        // Recovery must still run when the memory write consumed the caller's deadline.
        write(original[0], true).map_err(|error| error.detail("graphics offset rollback failed"))?;
        return Err(error);
    }
    let actual = read()?;
    if actual[0].clock_offset_mhz != i32::from(core_mhz)
        || actual[1].clock_offset_mhz != i32::from(mem_mhz)
    {
        return Err(Diagnostic::new("NVML clock offset readback", DiagnosticKind::Rejected));
    }
    Ok((core_mhz, mem_mhz))
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

    fn offsets(core: i32, mem: i32) -> [NvmlClockOffsetV1; 2] {
        [
            NvmlClockOffsetV1 { clock_offset_mhz: core, min_clock_offset_mhz: -1000, max_clock_offset_mhz: 1000, ..NvmlClockOffsetV1::new(GRAPHICS) },
            NvmlClockOffsetV1 { clock_offset_mhz: mem, min_clock_offset_mhz: -2000, max_clock_offset_mhz: 6000, ..NvmlClockOffsetV1::new(MEMORY) },
        ]
    }

    #[test]
    fn blackwell_offsets_use_v1_p0_and_intersect_the_msi_envelope() {
        let mut raw = offsets(25, 50);
        assert_eq!(std::mem::size_of::<NvmlClockOffsetV1>(), 24);
        assert_eq!(raw[0].version, (1 << 24) | 24);
        assert_eq!((raw[0].type_, raw[1].type_, raw[0].pstate, raw[1].pstate), (0, 2, 0, 0));
        let safe = bounded_offsets(raw).unwrap();
        assert_eq!((safe.core_mhz, safe.mem_mhz), (25, 50));
        assert_eq!((safe.core_min_mhz, safe.core_max_mhz, safe.mem_min_mhz, safe.mem_max_mhz), (0, 200, 0, 200));
        raw[0].min_clock_offset_mhz = 10;
        raw[1].max_clock_offset_mhz = 150;
        let safe = bounded_offsets(raw).unwrap();
        assert_eq!((safe.core_min_mhz, safe.mem_max_mhz), (10, 150));
        raw[1].min_clock_offset_mhz = 201;
        assert_eq!(bounded_offsets(raw), None);
        raw = offsets(-50, 600);
        assert_eq!((bounded_offsets(raw).unwrap().core_mhz, bounded_offsets(raw).unwrap().mem_mhz), (-50, 600));
        raw[0].max_clock_offset_mhz = -1;
        assert_eq!(bounded_offsets(raw), None);
    }

    #[test]
    fn offsets_reject_out_of_range_without_clamping_or_writing() {
        for (core, mem) in [(-1, 0), (201, 0), (0, -1), (0, 201)] {
            let error = apply_offsets(offsets(0, 0), core, mem,
                |_, _| panic!("invalid request must not write"),
                || panic!("invalid request must not read back"),
            ).unwrap_err();
            assert_eq!(error.kind, DiagnosticKind::InvalidData);
        }
    }

    #[test]
    fn both_offsets_are_applied_in_order_and_verified() {
        let mut writes = Vec::new();
        let result = apply_offsets(offsets(10, 20), 100, 200, |offset, rollback| {
            assert_eq!(offset.version, (1 << 24) | 24);
            assert_eq!(offset.pstate, 0);
            writes.push((offset.type_, offset.clock_offset_mhz, rollback));
            Ok(())
        }, || Ok(offsets(100, 200)));
        assert_eq!(result, Ok((100, 200)));
        assert_eq!(writes, [(GRAPHICS, 100, false), (MEMORY, 200, false)]);
        for actual in [offsets(99, 200), offsets(100, 199)] {
            assert_eq!(apply_offsets(offsets(0, 0), 100, 200, |_, _| Ok(()), || Ok(actual)).unwrap_err().kind, DiagnosticKind::Rejected);
        }
    }

    #[test]
    fn memory_failure_restores_original_graphics_even_after_deadline() {
        for kind in [DiagnosticKind::Io, DiagnosticKind::TimedOut] {
            let mut writes = Vec::new();
            let error = apply_offsets(offsets(35, 60), 100, 200, |offset, rollback| {
                writes.push((offset.type_, offset.clock_offset_mhz, rollback));
                if offset.type_ == MEMORY {
                    Err(Diagnostic::new("mock memory write", kind))
                } else {
                    Ok(())
                }
            }, || panic!("failed write must not report a successful readback")).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(writes, [(GRAPHICS, 100, false), (MEMORY, 200, false), (GRAPHICS, 35, true)]);
        }
    }

    #[test]
    fn failed_core_write_stops_and_failed_rollback_is_reported() {
        let mut calls = 0;
        assert_eq!(apply_offsets(offsets(0, 0), 100, 100, |offset, rollback| {
            calls += 1;
            assert_eq!(offset.type_, GRAPHICS);
            assert!(!rollback);
            Err(Diagnostic::new("mock core write", DiagnosticKind::AccessDenied))
        }, || panic!("core write failed")).unwrap_err().kind, DiagnosticKind::AccessDenied);
        assert_eq!(calls, 1);
        let error = apply_offsets(offsets(0, 0), 100, 100, |offset, rollback| {
            if offset.type_ == MEMORY || rollback {
                Err(Diagnostic::new("mock write", DiagnosticKind::Io))
            } else {
                Ok(())
            }
        }, || panic!("rollback failed")).unwrap_err();
        assert_eq!(error.detail.as_deref(), Some("graphics offset rollback failed"));
    }

    #[test]
    fn gpu_tool_conflicts_block_nonzero_but_never_a_reset() {
        assert_eq!(verify_clock_tools(0, 0, || panic!("reset bypasses GPU tool enumeration")), Ok(()));
        for (core, mem) in [(1, 0), (0, 1), (200, 200)] {
            assert_eq!(verify_clock_tools(core, mem, || Err(Diagnostic::conflict("mock GPU tools", "active"))).unwrap_err().kind, DiagnosticKind::ConflictDetected);
        }
        assert_eq!(verify_clock_tools(100, 100, || Ok(())), Ok(()));
        assert_eq!(apply_offsets(offsets(-50, 600), 0, 0, |_, _| Ok(()), || Ok(offsets(0, 0))), Ok((0, 0)));
    }

    #[test]
    fn vram_abi_and_units_are_bounded() {
        assert_eq!(std::mem::size_of::<NvmlMemory>(), 24);
        assert_eq!(memory_mb(NvmlMemory { total: 24 << 30, free: 22 << 30, used: 2 << 30 }).unwrap(), (2048, 24_576));
        assert!(memory_mb(NvmlMemory { total: 100, free: 1, used: 101 }).is_err());
        assert!(memory_mb(NvmlMemory { total: u64::MAX, free: 0, used: 0 }).is_err());
    }

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
