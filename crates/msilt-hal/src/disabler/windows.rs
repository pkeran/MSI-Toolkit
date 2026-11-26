use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Services::*;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, PCWSTR, PWSTR, w};

use crate::identity;
use crate::native::{NamedMutex, OwnedHandle};
use crate::{Diagnostic, DiagnosticKind};

use super::{
    MAX_RUN_BYTES, MsiObject, ObjectState, RegistryValue, RunState, ServiceActivity, ServiceStart,
    ServiceState, SystemChange, SystemControlOps, TaskState, backup, valid_state,
};

const RUN_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run");
const STARTUP_KEY: PCWSTR = w!(
    r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData\9426MICRO-STARINTERNATION.MSICenter_kzh8wxbdkxb8p\MSI_Center_UWP"
);
const SERVICE_BUDGET: Duration = Duration::from_secs(10);
const TASK_BUDGET: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The native adapter is obtained through `SoftwareDisabler::new()`.
pub struct WindowsSystemControlOps {
    _private: (),
}

pub struct WindowsUpdateLock {
    _lock: NamedMutex,
    _thread_bound: PhantomData<Rc<()>>,
}

impl WindowsSystemControlOps {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

impl SystemControlOps for WindowsSystemControlOps {
    type UpdateLock = WindowsUpdateLock;

    fn lock_update(&mut self) -> Result<Self::UpdateLock, Diagnostic> {
        const OP: &str = "MSI software update lock";
        if !identity::is_elevated()? {
            return Err(Diagnostic::new(OP, DiagnosticKind::AccessDenied)
                .detail("an elevated process is required"));
        }
        Ok(WindowsUpdateLock {
            _lock: NamedMutex::msi_software(OP)?,
            _thread_bound: PhantomData,
        })
    }

    fn backup_binding(&mut self) -> Result<String, Diagnostic> {
        let key = reg_open(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Cryptography"),
            KEY_QUERY_VALUE | native_view()?,
        )?
        .ok_or_else(|| Diagnostic::new("machine backup binding", DiagnosticKind::Unavailable))?;
        let guid = reg_read(&key, w!("MachineGuid"))?
            .filter(|value| {
                value.value_type == REG_SZ.0
                    && (72..=80).contains(&value.bytes.len())
                    && value.bytes.ends_with(&[0, 0])
            })
            .ok_or_else(|| {
                Diagnostic::invalid(
                    "machine backup binding",
                    "MachineGuid is missing or invalid",
                )
            })?;
        let mut hash = Sha256::new();
        hash.update(b"msilt/software-backup/v1\0");
        hash.update(guid.bytes);
        hash.update(user_sid()?);
        Ok(format!("{:x}", hash.finalize()))
    }

    fn read(&mut self, object: MsiObject) -> Result<ObjectState, Diagnostic> {
        if object.is_service() {
            let manager = scm()?;
            return match open_service(
                &manager,
                object,
                SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
            )? {
                Some(service) => service_state(&service).map(ObjectState::Service),
                None => Ok(ObjectState::Missing),
            };
        }
        if object.is_task() {
            return with_task(object, task_state)
                .map(|state| state.map_or(ObjectState::Missing, ObjectState::Task));
        }
        match object {
            MsiObject::TrueColorRun => read_run().map(ObjectState::Run),
            MsiObject::StoreStartup => read_startup()
                .map(|state| state.map_or(ObjectState::Missing, ObjectState::StoreStartup)),
            _ => unreachable!("fixed MSI object kinds"),
        }
    }

    fn preflight(&mut self, objects: &[MsiObject]) -> Result<(), Diagnostic> {
        for &object in objects {
            if object.is_service() {
                let manager = scm()?;
                let service = require_service(&manager, object, ALL_CHANGE_ACCESS)?;
                let _ = service_state(&service)?;
            } else if object.is_task() {
                require_task(object, task_state)?;
            } else if object == MsiObject::TrueColorRun {
                for slot in run_slots()? {
                    let _ = reg_open(
                        HKEY_LOCAL_MACHINE,
                        RUN_KEY,
                        KEY_QUERY_VALUE | KEY_SET_VALUE | slot.view,
                    )?;
                }
            } else {
                let _ =
                    startup_key(KEY_QUERY_VALUE | KEY_SET_VALUE)?.ok_or_else(|| absent(object))?;
            }
        }
        Ok(())
    }

    fn execute(&mut self, change: SystemChange) -> Result<(), Diagnostic> {
        match change {
            SystemChange::ServiceStart(object, start, delayed) => {
                let manager = scm()?;
                let service = require_service(
                    &manager,
                    object,
                    SERVICE_QUERY_CONFIG | SERVICE_CHANGE_CONFIG,
                )?;
                set_service_start(&service, start, delayed)
            }
            SystemChange::ServiceActivity(object, activity) => {
                let manager = scm()?;
                let service = require_service(&manager, object, ALL_CHANGE_ACCESS)?;
                set_service_activity(&manager, &service, activity)
            }
            SystemChange::StopTask(object) => require_task(object, stop_task),
            SystemChange::TaskEnabled(object, enabled) => require_task(object, |task| {
                unsafe { task.SetEnabled(if enabled { VARIANT_TRUE } else { VARIANT_FALSE }) }
                    .map_err(|e| Diagnostic::windows("IRegisteredTask::SetEnabled", e))
            }),
            SystemChange::Run { before, after } => change_run(&before, &after),
            SystemChange::StoreStartup { before, after } => change_startup(before, after),
        }
    }

    fn wait(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    fn read_backup(&mut self, path: &Path) -> Result<Option<Vec<u8>>, Diagnostic> {
        backup::read(path)
    }

    fn create_backup(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Diagnostic> {
        backup::create(path, bytes)
    }
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

const ALL_CHANGE_ACCESS: u32 = SERVICE_QUERY_CONFIG
    | SERVICE_QUERY_STATUS
    | SERVICE_CHANGE_CONFIG
    | SERVICE_START
    | SERVICE_STOP
    | SERVICE_PAUSE_CONTINUE;

fn scm() -> Result<ServiceHandle, Diagnostic> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .map(ServiceHandle)
        .map_err(|e| Diagnostic::windows("OpenSCManagerW(MSI)", e))
}

fn open_service(
    manager: &ServiceHandle,
    object: MsiObject,
    access: u32,
) -> Result<Option<ServiceHandle>, Diagnostic> {
    if !object.is_service() {
        return Err(Diagnostic::invalid(
            "MSI service allowlist",
            "object is not a managed service",
        ));
    }
    let name: Vec<u16> = object.name().encode_utf16().chain([0]).collect();
    match unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) } {
        Ok(handle) => Ok(Some(ServiceHandle(handle))),
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
        Err(e) => Err(Diagnostic::windows("OpenServiceW(MSI)", e).detail(object.name())),
    }
}

fn require_service(
    manager: &ServiceHandle,
    object: MsiObject,
    access: u32,
) -> Result<ServiceHandle, Diagnostic> {
    open_service(manager, object, access)?.ok_or_else(|| absent(object))
}

fn absent(object: MsiObject) -> Diagnostic {
    Diagnostic::new("MSI software object", DiagnosticKind::Unavailable).detail(object.name())
}

fn service_config(service: &ServiceHandle) -> Result<Vec<usize>, Diagnostic> {
    const OP: &str = "QueryServiceConfigW";
    let mut bytes = 0;
    match unsafe { QueryServiceConfigW(service.0, None, 0, &mut bytes) } {
        Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => (),
        Err(e) => return Err(Diagnostic::windows(OP, e)),
        Ok(()) => return Err(Diagnostic::invalid(OP, "missing service buffer size")),
    }
    if !(size_of::<QUERY_SERVICE_CONFIGW>() as u32..=8192).contains(&bytes) {
        return Err(Diagnostic::invalid(OP, "unexpected service config size"));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    unsafe {
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    let config = config_header(&buffer);
    if config.dwServiceType != SERVICE_WIN32_OWN_PROCESS {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
            .detail("managed name is not a Win32 own-process service"));
    }
    Ok(buffer)
}

fn config_header(buffer: &[usize]) -> &QUERY_SERVICE_CONFIGW {
    unsafe { &*buffer.as_ptr().cast() }
}

fn delayed(service: &ServiceHandle) -> Result<bool, Diagnostic> {
    const OP: &str = "QueryServiceConfig2W(delayed start)";
    let mut info = SERVICE_DELAYED_AUTO_START_INFO::default();
    let mut bytes = 0;
    let buffer = unsafe {
        std::slice::from_raw_parts_mut(std::ptr::from_mut(&mut info).cast(), size_of_val(&info))
    };
    unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            Some(buffer),
            &mut bytes,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    Ok(info.fDelayedAutostart.as_bool())
}

fn service_state(service: &ServiceHandle) -> Result<ServiceState, Diagnostic> {
    let buffer = service_config(service)?;
    let start = match config_header(&buffer).dwStartType {
        SERVICE_AUTO_START => ServiceStart::Automatic,
        SERVICE_DEMAND_START => ServiceStart::Manual,
        SERVICE_DISABLED => ServiceStart::Disabled,
        _ => {
            return Err(Diagnostic::invalid(
                "QueryServiceConfigW",
                "unsupported service start type",
            ));
        }
    };
    let status = service_status(service)?;
    let activity = stable_activity(status).ok_or_else(|| {
        Diagnostic::new("QueryServiceStatusEx", DiagnosticKind::Busy)
            .detail("managed service is transitioning")
    })?;
    Ok(ServiceState {
        start,
        activity,
        delayed_auto_start: delayed(service)?,
    })
}

fn service_status(service: &ServiceHandle) -> Result<SERVICE_STATUS_CURRENT_STATE, Diagnostic> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut bytes = 0;
    let buffer = unsafe {
        std::slice::from_raw_parts_mut(std::ptr::from_mut(&mut status).cast(), size_of_val(&status))
    };
    unsafe { QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(buffer), &mut bytes) }
        .map_err(|e| Diagnostic::windows("QueryServiceStatusEx", e))?;
    Ok(status.dwCurrentState)
}

fn stable_activity(status: SERVICE_STATUS_CURRENT_STATE) -> Option<ServiceActivity> {
    match status {
        SERVICE_STOPPED => Some(ServiceActivity::Stopped),
        SERVICE_RUNNING => Some(ServiceActivity::Running),
        SERVICE_PAUSED => Some(ServiceActivity::Paused),
        _ => None,
    }
}

fn set_service_start(
    service: &ServiceHandle,
    start: ServiceStart,
    delayed_auto: bool,
) -> Result<(), Diagnostic> {
    let config = service_config(service)?;
    let desired = match start {
        ServiceStart::Automatic => SERVICE_AUTO_START,
        ServiceStart::Manual => SERVICE_DEMAND_START,
        ServiceStart::Disabled => SERVICE_DISABLED,
    };
    if config_header(&config).dwStartType != desired {
        let nil = PCWSTR::null();
        unsafe {
            ChangeServiceConfigW(
                service.0,
                ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                desired,
                SERVICE_ERROR(SERVICE_NO_CHANGE),
                nil,
                nil,
                None,
                nil,
                nil,
                nil,
                nil,
            )
        }
        .map_err(|e| Diagnostic::windows("ChangeServiceConfigW(MSI)", e))?;
    }
    if delayed(service)? != delayed_auto {
        let value = SERVICE_DELAYED_AUTO_START_INFO {
            fDelayedAutostart: delayed_auto.into(),
        };
        unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(std::ptr::from_ref(&value).cast()),
            )
        }
        .map_err(|e| Diagnostic::windows("ChangeServiceConfig2W(delayed start)", e))?;
    }
    Ok(())
}

fn set_service_activity(
    manager: &ServiceHandle,
    service: &ServiceHandle,
    desired: ServiceActivity,
) -> Result<(), Diagnostic> {
    let config = service_config(service)?;
    let until = Instant::now() + SERVICE_BUDGET;
    let current =
        wait_service(service, until, |activity| activity.is_some())?.expect("settled SCM state");
    if current == desired {
        return Ok(());
    }
    if desired == ServiceActivity::Stopped {
        service_control(
            service,
            SERVICE_CONTROL_STOP,
            ERROR_SERVICE_NOT_ACTIVE.to_hresult(),
        )?;
    } else {
        if current == ServiceActivity::Stopped {
            verify_dependencies_running(manager, &config)?;
            if let Err(e) = unsafe { StartServiceW(service.0, None) } {
                if e.code() != ERROR_SERVICE_ALREADY_RUNNING.to_hresult() {
                    return Err(Diagnostic::windows("StartServiceW(MSI)", e));
                }
            }
            wait_service(service, until, |activity| {
                matches!(
                    activity,
                    Some(ServiceActivity::Running | ServiceActivity::Paused)
                )
            })?;
        }
        let current = stable_activity(service_status(service)?);
        match (current, desired) {
            (Some(ServiceActivity::Running), ServiceActivity::Paused) => {
                service_control(service, SERVICE_CONTROL_PAUSE, S_OK)?
            }
            (Some(ServiceActivity::Paused), ServiceActivity::Running) => {
                service_control(service, SERVICE_CONTROL_CONTINUE, S_OK)?
            }
            _ => (),
        }
    }
    wait_service(service, until, |activity| activity == Some(desired))?;
    Ok(())
}

fn verify_dependencies_running(
    manager: &ServiceHandle,
    buffer: &[usize],
) -> Result<(), Diagnostic> {
    const OP: &str = "StartServiceW(MSI dependency preflight)";
    let address = config_header(buffer).lpDependencies.0;
    if address.is_null() {
        return Ok(());
    }
    let offset = (address as usize)
        .checked_sub(buffer.as_ptr() as usize)
        .filter(|offset| *offset % 2 == 0 && *offset < size_of_val(buffer))
        .ok_or_else(|| Diagnostic::invalid(OP, "invalid dependencies pointer"))?;
    let words = unsafe { std::slice::from_raw_parts(address, (size_of_val(buffer) - offset) / 2) };
    for name in words.split_inclusive(|&word| word == 0) {
        if name == [0] {
            return Ok(());
        }
        if name.first() == Some(&u16::from(b'+')) || name.last() != Some(&0) {
            return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
                .detail("service group or malformed dependency cannot be safely auto-started"));
        }
        // Query dependencies only; letting SCM start one would bypass the MSI allowlist.
        let dependency = ServiceHandle(
            unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS) }
                .map_err(|e| Diagnostic::windows(OP, e))?,
        );
        if service_status(&dependency)? != SERVICE_RUNNING {
            return Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
                .detail("a dependency is not already running"));
        }
    }
    Err(Diagnostic::invalid(OP, "unterminated dependency list"))
}

fn service_control(
    service: &ServiceHandle,
    control: u32,
    race_status: windows::core::HRESULT,
) -> Result<(), Diagnostic> {
    let mut status = SERVICE_STATUS::default();
    match unsafe { ControlService(service.0, control, &mut status) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == race_status => Ok(()),
        Err(e) => Err(Diagnostic::windows("ControlService(MSI)", e)),
    }
}

fn wait_service(
    service: &ServiceHandle,
    until: Instant,
    matches: impl Fn(Option<ServiceActivity>) -> bool,
) -> Result<Option<ServiceActivity>, Diagnostic> {
    loop {
        let state = stable_activity(service_status(service)?);
        if matches(state) {
            return Ok(state);
        }
        if !poll_wait(until) {
            return Err(Diagnostic::new(
                "SCM managed service transition",
                DiagnosticKind::TimedOut,
            ));
        }
    }
}

fn poll_wait(until: Instant) -> bool {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return false;
    }
    thread::sleep(left.min(POLL_INTERVAL));
    true
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self, Diagnostic> {
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if status == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        status
            .ok()
            .map_err(|e| Diagnostic::windows("CoInitializeEx(Task Scheduler)", e))?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn with_task<T>(
    object: MsiObject,
    call: impl FnOnce(&IRegisteredTask) -> Result<T, Diagnostic>,
) -> Result<Option<T>, Diagnostic> {
    if !object.is_task() {
        return Err(Diagnostic::invalid(
            "MSI task allowlist",
            "object is not a managed task",
        ));
    }
    with_scheduler(|service| {
        let root = unsafe { service.GetFolder(&BSTR::from(r"\")) }
            .map_err(|e| Diagnostic::windows("ITaskService::GetFolder", e))?;
        match unsafe { root.GetTask(&BSTR::from(&object.name()[1..])) } {
            Ok(task) => call(&task).map(Some),
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
            Err(e) => {
                Err(Diagnostic::windows("ITaskFolder::GetTask(MSI)", e).detail(object.name()))
            }
        }
    })
}

pub(crate) fn with_scheduler<T>(
    call: impl FnOnce(&ITaskService) -> Result<T, Diagnostic>,
) -> Result<T, Diagnostic> {
    let _apartment = ComApartment::enter()?;
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| Diagnostic::windows("CoCreateInstance(TaskScheduler)", e))?;
    let local = VARIANT::default();
    unsafe { service.Connect(&local, &local, &local, &local) }
        .map_err(|e| Diagnostic::windows("ITaskService::Connect", e))?;
    call(&service)
}

fn require_task<T>(
    object: MsiObject,
    call: impl FnOnce(&IRegisteredTask) -> Result<T, Diagnostic>,
) -> Result<T, Diagnostic> {
    with_task(object, call)?.ok_or_else(|| absent(object))
}

fn task_state(task: &IRegisteredTask) -> Result<TaskState, Diagnostic> {
    let enabled = unsafe { task.Enabled() }
        .map_err(|e| Diagnostic::windows("IRegisteredTask::Enabled", e))?;
    if !matches!(enabled.0, -1 | 0) {
        return Err(Diagnostic::invalid(
            "IRegisteredTask::Enabled",
            "invalid automation boolean",
        ));
    }
    Ok(TaskState {
        enabled: enabled.0 == -1,
        active: task_active(task)?,
    })
}

fn task_active(task: &IRegisteredTask) -> Result<bool, Diagnostic> {
    let state =
        unsafe { task.State() }.map_err(|e| Diagnostic::windows("IRegisteredTask::State", e))?;
    match state {
        TASK_STATE_QUEUED | TASK_STATE_RUNNING => Ok(true),
        TASK_STATE_DISABLED | TASK_STATE_READY => {
            let instances = unsafe { task.GetInstances(0) }
                .map_err(|e| Diagnostic::windows("IRegisteredTask::GetInstances", e))?;
            let count = unsafe { instances.Count() }
                .map_err(|e| Diagnostic::windows("IRunningTaskCollection::Count", e))?;
            if count < 0 {
                return Err(Diagnostic::invalid(
                    "IRegisteredTask::GetInstances",
                    "invalid instance count",
                ));
            }
            Ok(count > 0)
        }
        _ => Err(
            Diagnostic::new("IRegisteredTask::State", DiagnosticKind::Unavailable)
                .detail("task state is unknown"),
        ),
    }
}

fn stop_task(task: &IRegisteredTask) -> Result<(), Diagnostic> {
    const OP: &str = "IRegisteredTask::Stop";
    match unsafe { task.Stop(0) } {
        Ok(()) => (),
        Err(e) if e.code() == SCHED_E_TASK_NOT_RUNNING && !task_active(task)? => return Ok(()),
        Err(e) => return Err(Diagnostic::windows(OP, e)),
    }
    let until = Instant::now() + TASK_BUDGET;
    while task_active(task)? {
        if !poll_wait(until) {
            return Err(Diagnostic::new(OP, DiagnosticKind::TimedOut));
        }
    }
    Ok(())
}

pub(crate) struct RegKey(pub(crate) HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

pub(crate) fn reg_open(
    hive: HKEY,
    path: PCWSTR,
    access: REG_SAM_FLAGS,
) -> Result<Option<RegKey>, Diagnostic> {
    let mut handle = HKEY::default();
    match unsafe { RegOpenKeyExW(hive, path, None, access, &mut handle) } {
        ERROR_SUCCESS => Ok(Some(RegKey(handle))),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
        status => Err(Diagnostic::win32("RegOpenKeyExW(MSI)", status.0)),
    }
}

pub(crate) fn reg_read(key: &RegKey, name: PCWSTR) -> Result<Option<RegistryValue>, Diagnostic> {
    const OP: &str = "RegQueryValueExW(MSI)";
    let mut len = 0;
    let mut ty = REG_VALUE_TYPE::default();
    match unsafe { RegQueryValueExW(key.0, name, None, Some(&mut ty), None, Some(&mut len)) } {
        ERROR_FILE_NOT_FOUND => return Ok(None),
        ERROR_SUCCESS => (),
        status => return Err(Diagnostic::win32(OP, status.0)),
    }
    if len as usize > MAX_RUN_BYTES {
        return Err(Diagnostic::invalid(OP, "registry value exceeds limit"));
    }
    let (original_type, expected) = (ty, len);
    let mut bytes = vec![0; (len as usize).max(1)];
    match unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut ty),
            Some(bytes.as_mut_ptr()),
            Some(&mut len),
        )
    } {
        ERROR_SUCCESS if len == expected && ty == original_type => {
            bytes.truncate(len as usize);
            Ok(Some(RegistryValue {
                value_type: ty.0,
                bytes,
            }))
        }
        ERROR_SUCCESS | ERROR_MORE_DATA | ERROR_FILE_NOT_FOUND => Err(Diagnostic::conflict(
            OP,
            "registry value changed during read",
        )),
        status => Err(Diagnostic::win32(OP, status.0)),
    }
}

#[derive(Clone, Copy)]
struct RunSlot {
    index: usize,
    view: REG_SAM_FLAGS,
    name: PCWSTR,
}

fn run_slots() -> Result<Vec<RunSlot>, Diagnostic> {
    let mut slots = Vec::new();
    if has_64bit_registry()? {
        slots.extend([
            RunSlot {
                index: 0,
                view: KEY_WOW64_64KEY,
                name: w!("MsiTrueColor"),
            },
            RunSlot {
                index: 1,
                view: KEY_WOW64_64KEY,
                name: w!("MSI True Color"),
            },
        ]);
    }
    slots.extend([
        RunSlot {
            index: 2,
            view: KEY_WOW64_32KEY,
            name: w!("MsiTrueColor"),
        },
        RunSlot {
            index: 3,
            view: KEY_WOW64_32KEY,
            name: w!("MSI True Color"),
        },
    ]);
    Ok(slots)
}

fn native_view() -> Result<REG_SAM_FLAGS, Diagnostic> {
    Ok(if has_64bit_registry()? {
        KEY_WOW64_64KEY
    } else {
        REG_SAM_FLAGS(0)
    })
}

#[cfg(target_pointer_width = "64")]
fn has_64bit_registry() -> Result<bool, Diagnostic> {
    Ok(true)
}

#[cfg(target_pointer_width = "32")]
fn has_64bit_registry() -> Result<bool, Diagnostic> {
    let mut wow64 = windows::core::BOOL(0);
    unsafe { windows::Win32::System::Threading::IsWow64Process(GetCurrentProcess(), &mut wow64) }
        .map_err(|e| Diagnostic::windows("IsWow64Process(registry views)", e))?;
    Ok(wow64.as_bool())
}

fn read_run() -> Result<RunState, Diagnostic> {
    let mut values = [None, None, None, None];
    for slot in run_slots()? {
        if let Some(key) = reg_open(HKEY_LOCAL_MACHINE, RUN_KEY, KEY_QUERY_VALUE | slot.view)? {
            values[slot.index] = reg_read(&key, slot.name)?;
        }
    }
    let [name_64, spaced_name_64, name_32, spaced_name_32] = values;
    Ok(RunState {
        name_64,
        spaced_name_64,
        name_32,
        spaced_name_32,
    })
}

fn change_run(before: &RunState, after: &RunState) -> Result<(), Diagnostic> {
    const OP: &str = "HKLM Run(MsiTrueColor)";
    if !valid_state(MsiObject::TrueColorRun, &ObjectState::Run(after.clone())) {
        return Err(Diagnostic::invalid(OP, "invalid Run snapshot"));
    }
    let prior = before.values();
    let desired = after.values();
    for slot in run_slots()? {
        let i = slot.index;
        if prior[i] == desired[i] {
            continue;
        }
        let key = reg_open(
            HKEY_LOCAL_MACHINE,
            RUN_KEY,
            KEY_QUERY_VALUE | KEY_SET_VALUE | slot.view,
        )?
        .ok_or_else(|| absent(MsiObject::TrueColorRun))?;
        if &reg_read(&key, slot.name)? != prior[i] {
            return Err(Diagnostic::conflict(
                OP,
                "Run value changed after preflight",
            ));
        }
        let status = if let Some(value) = desired[i] {
            unsafe {
                RegSetValueExW(
                    key.0,
                    slot.name,
                    None,
                    REG_VALUE_TYPE(value.value_type),
                    Some(&value.bytes),
                )
            }
        } else {
            unsafe { RegDeleteValueW(key.0, slot.name) }
        };
        if status != ERROR_SUCCESS {
            return Err(Diagnostic::win32(OP, status.0));
        }
    }
    Ok(())
}

fn startup_key(access: REG_SAM_FLAGS) -> Result<Option<RegKey>, Diagnostic> {
    reg_open(current_user_key()?.0, STARTUP_KEY, access)
}

pub(crate) fn current_user_key() -> Result<RegKey, Diagnostic> {
    let mut current = HKEY::default();
    let status = unsafe { RegOpenCurrentUser(KEY_READ.0, &mut current) };
    if status != ERROR_SUCCESS {
        return Err(Diagnostic::win32(
            "RegOpenCurrentUser(MSI startup task)",
            status.0,
        ));
    }
    Ok(RegKey(current))
}

fn startup_state(key: &RegKey) -> Result<Option<u32>, Diagnostic> {
    reg_read(key, w!("State"))?
        .map(|value| {
            let bytes = value
                .bytes
                .as_slice()
                .try_into()
                .ok()
                .filter(|_| value.value_type == REG_DWORD.0)
                .ok_or_else(|| Diagnostic::invalid("MSI_Center_UWP State", "expected a DWORD"))?;
            Ok(u32::from_le_bytes(bytes))
        })
        .transpose()
}

fn read_startup() -> Result<Option<u32>, Diagnostic> {
    match startup_key(KEY_QUERY_VALUE)? {
        Some(key) => startup_state(&key),
        None => Ok(None),
    }
}

fn change_startup(before: u32, after: u32) -> Result<(), Diagnostic> {
    const OP: &str = "MSI_Center_UWP State";
    if !matches!((before, after), (0, 2) | (2, 0)) {
        return Err(Diagnostic::invalid(
            OP,
            "startup policy or user-disabled state cannot be overwritten",
        ));
    }
    let key = startup_key(KEY_QUERY_VALUE | KEY_SET_VALUE)?
        .ok_or_else(|| absent(MsiObject::StoreStartup))?;
    if startup_state(&key)? != Some(before) {
        return Err(Diagnostic::conflict(
            OP,
            "startup state changed after preflight",
        ));
    }
    let status = unsafe {
        RegSetValueExW(
            key.0,
            w!("State"),
            None,
            REG_DWORD,
            Some(&after.to_le_bytes()),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(Diagnostic::win32(OP, status.0))
    }
}

fn user_sid() -> Result<Vec<u8>, Diagnostic> {
    with_user_sid(|sid| {
        let len = unsafe { GetLengthSid(sid) } as usize;
        if !(8..=68).contains(&len) {
            return Err(Diagnostic::invalid(
                "GetTokenInformation(TokenUser)",
                "invalid SID length",
            ));
        }
        Ok(unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), len) }.to_vec())
    })
}

pub(crate) fn user_sid_string() -> Result<String, Diagnostic> {
    const OP: &str = "ConvertSidToStringSidW";
    with_user_sid(|sid| {
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut text) }
            .map_err(|e| Diagnostic::windows(OP, e))?;
        let result =
            unsafe { text.to_string() }.map_err(|_| Diagnostic::invalid(OP, "invalid SID text"));
        unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        result
    })
}

fn with_user_sid<T>(
    call: impl FnOnce(windows::Win32::Security::PSID) -> Result<T, Diagnostic>,
) -> Result<T, Diagnostic> {
    const OP: &str = "GetTokenInformation(TokenUser)";
    let mut token = HANDLE::default();
    match unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) } {
        Ok(()) => (),
        Err(error) if error.code() == ERROR_NO_TOKEN.to_hresult() => {
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
                .map_err(|e| Diagnostic::windows("OpenProcessToken", e))?;
        }
        Err(error) => return Err(Diagnostic::windows("OpenThreadToken", error)),
    }
    let token = OwnedHandle(token);
    let mut bytes = 0;
    if let Err(error) = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes) } {
        if error.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult() {
            return Err(Diagnostic::windows(OP, error));
        }
    }
    if !(size_of::<TOKEN_USER>() as u32..=1024).contains(&bytes) {
        return Err(Diagnostic::invalid(OP, "invalid token size"));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .map_err(|e| Diagnostic::windows(OP, e))?;
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if sid.0.is_null() || !unsafe { IsValidSid(sid) }.as_bool() {
        return Err(Diagnostic::invalid(OP, "invalid user SID"));
    }
    call(sid)
}
