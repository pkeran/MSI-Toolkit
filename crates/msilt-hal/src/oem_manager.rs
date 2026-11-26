//! Inventory and control for the fixed MSI software allowlists.

use std::io::{Read, Seek, SeekFrom};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use msilt_core::{
    OemAction, OemInventory, OemItemKind, OemItemStatus, OemPackageId, OemPackageStatus,
    OemProcessStatus, OemScope,
};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::*;
use windows::core::{PCWSTR, PWSTR, w};

use crate::disabler::{
    DisablePlan, DisableTier, DisablerError, MsiObject, ObjectState, ServiceActivity, ServiceStart,
    SoftwareDisabler, SystemControlOps, WindowsSystemControlOps, is_protected, reg_open, reg_read,
};
use crate::native::{OwnedHandle, wide_str};
use crate::startup::{LocalFile, checked_local_file};
use crate::{Diagnostic, DiagnosticKind, identity};

const PROTECTED_DLL: &str = r"C:\Windows\SysWOW64\msiapcfg.dll";
const COMPANION_DLLS: &[&str] = &[
    r"C:\Windows\SysWOW64\msiltcfg.dll",
    r"C:\Windows\System32\msiltcfg.dll",
];
const APPX_NAME: &str = "9426MICRO-STARINTERNATION.MSICenter";
const APPX_FAMILY: PCWSTR = w!("9426MICRO-STARINTERNATION.MSICenter_kzh8wxbdkxb8p");
const UNINSTALL_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall");
const WMI_ACPI_KEY: PCWSTR = w!(r"SYSTEM\CurrentControlSet\Services\WmiAcpi");
const OP: &str = "MSI official software";

const PROCESSES: &[(&str, OemScope)] = &[
    ("DCv2", OemScope::Core),
    ("MSI.CentralServer", OemScope::Core),
    ("MSI.TerminalServer", OemScope::Core),
    ("MSI_Central_Service", OemScope::Core),
    ("MSIService", OemScope::Core),
    ("MSIAPService", OemScope::Core),
    ("LEDKeeper2", OemScope::Lighting),
    ("Mystic_Light_Service", OemScope::Lighting),
    ("MsiTrueColor", OemScope::TrueColor),
    ("MsiTrueColorHelper", OemScope::TrueColor),
    ("MsiTrueColorService", OemScope::TrueColor),
];

struct DesktopPackage {
    id: OemPackageId,
    name: &'static str,
    publisher: &'static str,
    executable: &'static str,
    path: &'static str,
    args: &'static [&'static str],
}

const SDK: DesktopPackage = DesktopPackage {
    id: OemPackageId::MsiCenterSdk,
    name: "MSI Center SDK",
    publisher: "MSI",
    executable: "unins000.exe",
    path: r"C:\Program Files (x86)\MSI\MSI Center\unins000.exe",
    args: &["/VERYSILENT", "/NORESTART"],
};
const TRUE_COLOR: DesktopPackage = DesktopPackage {
    id: OemPackageId::MsiTrueColor,
    name: "MSI True Color",
    publisher: "Portrait Displays, Inc.",
    executable: "Uninstall.exe",
    path: r"C:\Program Files\Portrait Displays\MSI True Color\Uninstall.exe",
    args: &[],
};

pub fn inspect_oem(backup_path: &Path) -> OemInventory {
    let mut ops = WindowsSystemControlOps::new();
    let mut incomplete = false;
    let items = MsiObject::ALL
        .iter()
        .map(|&object| {
            let state = ops.read(object);
            incomplete |= state.is_err();
            item_status(object, state.ok().as_ref())
        })
        .collect::<Vec<_>>();
    let mut processes = Vec::new();
    incomplete |= for_each_process(|name, tier, pid| {
        let memory_kib = process_memory(pid).unwrap_or_else(|| {
            incomplete = true;
            0
        });
        processes.push(OemProcessStatus {
            name: name.into(),
            pid,
            memory_kib,
            tier,
        });
        Ok(())
    })
    .is_err();
    processes.sort_unstable_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    let elevated = identity::is_elevated().unwrap_or_default();
    let protected_intact = checked_provider(Path::new(PROTECTED_DLL), "msiapcfg.dll").is_ok();
    let uwp = appx_version();
    incomplete |= uwp.is_err();
    let version = uwp.unwrap_or_default();
    let mut packages = vec![OemPackageStatus {
        id: OemPackageId::MsiCenterUwp,
        display_name: "MSI Center（商店应用）".into(),
        installed: version.is_some(),
        can_uninstall: version.is_some(),
        version: version.unwrap_or_default(),
        note: "仅移除当前用户的商店应用".into(),
    }];
    let desktop = desktop_packages();
    incomplete |= desktop.is_err();
    let desktop = desktop.unwrap_or_default();
    packages.extend(
        [&SDK, &TRUE_COLOR]
            .into_iter()
            .zip(desktop)
            .map(|(package, entry)| {
                let installed = entry.is_some();
                let valid = entry.as_ref().is_some_and(|entry| entry.launchable)
                    && checked_local_file(Path::new(package.path), package.executable, OP)
                        .and_then(|file| file.require_admin_path(OP))
                        .is_ok();
                let protected = package.id != OemPackageId::MsiCenterSdk || protected_intact;
                let note = if !installed {
                    "未检测到已安装包"
                } else if !valid {
                    "卸载程序不在受信任的安装路径"
                } else if !protected {
                    "无法确认 MSI ACPI 提供程序完好"
                } else if !elevated {
                    "卸载需要管理员权限"
                } else {
                    "使用官方卸载程序"
                };
                OemPackageStatus {
                    id: package.id,
                    display_name: package.name.into(),
                    version: entry.map(|entry| entry.version).unwrap_or_default(),
                    installed,
                    can_uninstall: valid && protected && elevated,
                    note: note.into(),
                }
            }),
    );
    let present = items.iter().filter(|item| item.present).count();
    let summary = format!(
        "{present}/{} 个受管项目已安装，{} 个官软进程运行中{}",
        items.len(),
        processes.len(),
        if incomplete {
            "；部分状态无法读取"
        } else {
            ""
        }
    );
    OemInventory {
        elevated,
        has_backup: backup_path.is_file(),
        protected_intact,
        items,
        processes,
        packages,
        summary,
    }
}

pub fn execute_oem_action(
    action: OemAction,
    backup_path: &Path,
) -> Result<OemInventory, Diagnostic> {
    let mut disabler = SoftwareDisabler::new();
    match action {
        OemAction::DisableAutoStart { scope } => {
            disabler
                .disable_autostart(&plan(scope)?, backup_path)
                .map_err(disabler_error)?;
        }
        OemAction::StopNow { scope } => {
            disabler
                .stop_services(&plan(scope)?)
                .map_err(disabler_error)?;
            terminate_matching(|_, tier| in_scope(scope, tier))?;
        }
        OemAction::DisableAndStop { scope } => {
            disabler
                .disable(&plan(scope)?, backup_path)
                .map_err(disabler_error)?;
            terminate_matching(|_, tier| in_scope(scope, tier))?;
        }
        OemAction::RestoreBackup {} => {
            disabler
                .restore_from_backup(backup_path)
                .map_err(disabler_error)?;
        }
        OemAction::UninstallPackage {
            package: OemPackageId::MsiCenterUwp,
        } => uninstall_appx()?,
        OemAction::UninstallPackage { package } => {
            let (package, tiers) = if package == OemPackageId::MsiCenterSdk {
                (&SDK, &[DisableTier::Core, DisableTier::Lighting][..])
            } else {
                (&TRUE_COLOR, &[DisableTier::TrueColor][..])
            };
            uninstall_desktop(&mut disabler, package, tiers)?;
        }
    }
    Ok(inspect_oem(backup_path))
}

fn scope_of(tier: DisableTier) -> OemScope {
    match tier {
        DisableTier::Core => OemScope::Core,
        DisableTier::Lighting => OemScope::Lighting,
        DisableTier::TrueColor => OemScope::TrueColor,
    }
}

fn in_scope(scope: OemScope, tier: OemScope) -> bool {
    scope == OemScope::All || scope == tier
}

fn plan(scope: OemScope) -> Result<DisablePlan, Diagnostic> {
    DisablePlan::new(
        [
            DisableTier::Core,
            DisableTier::Lighting,
            DisableTier::TrueColor,
        ]
        .into_iter()
        .filter(|&tier| in_scope(scope, scope_of(tier))),
    )
    .map_err(disabler_error)
}

fn disabler_error(error: DisablerError) -> Diagnostic {
    match error {
        DisablerError::System(error) => error,
        DisablerError::ApplyFailed {
            cause,
            rollback_failures,
            ..
        } if rollback_failures.is_empty() => cause,
        error => {
            let kind = match error {
                DisablerError::Protected | DisablerError::MachineOrUserMismatch => {
                    DiagnosticKind::AccessDenied
                }
                DisablerError::NoBackup => DiagnosticKind::Unavailable,
                DisablerError::EmptyPlan
                | DisablerError::InvalidBackup(_)
                | DisablerError::NotManaged => DiagnosticKind::InvalidData,
                DisablerError::PlanMismatch => DiagnosticKind::ConflictDetected,
                _ => DiagnosticKind::Io,
            };
            Diagnostic::new(OP, kind).detail(error.to_string())
        }
    }
}

fn item_status(object: MsiObject, state: Option<&ObjectState>) -> OemItemStatus {
    use MsiObject::*;
    let (display_name, kind) = match object {
        Center => ("MSI Center 核心服务", OemItemKind::Service),
        Foundation => ("MSI 基础服务", OemItemKind::Service),
        Sendev => ("MSI 传感器服务", OemItemKind::Service),
        Scm => ("Micro Star SCM", OemItemKind::Service),
        MysticLight => ("Mystic Light 灯效服务", OemItemKind::Service),
        LightKeeper => ("LightKeeper 灯效守护", OemItemKind::Service),
        TrueColorService => ("MSI True Color 校色服务", OemItemKind::Service),
        BrokerTask => ("OmApSvcBroker 计划任务", OemItemKind::ScheduledTask),
        UpdaterTask => ("OneDC 更新任务", OemItemKind::ScheduledTask),
        TrueColorRun => ("True Color 登录启动项", OemItemKind::RegistryRun),
        StoreStartup => ("MSI Center 商店自启项", OemItemKind::StoreStartup),
    };
    let label = |enabled| if enabled { "已启用" } else { "已禁用" };
    let (present, running, auto_start, startup_label) = match state {
        Some(ObjectState::Service(state)) => (
            true,
            state.activity != ServiceActivity::Stopped,
            state.start == ServiceStart::Automatic,
            match state.start {
                ServiceStart::Automatic => "自动",
                ServiceStart::Manual => "手动",
                ServiceStart::Disabled => "已禁用",
            },
        ),
        Some(ObjectState::Task(state)) => (true, state.active, state.enabled, label(state.enabled)),
        Some(ObjectState::Run(state)) if state.is_present() => (true, false, true, "已启用"),
        Some(ObjectState::StoreStartup(state)) => (
            true,
            false,
            matches!(state, 2 | 4),
            label(matches!(state, 2 | 4)),
        ),
        Some(_) => (false, false, false, "未安装"),
        None => (false, false, false, "无法读取"),
    };
    OemItemStatus {
        id: object.name().into(),
        display_name: display_name.into(),
        kind,
        tier: scope_of(object.tier()),
        present,
        running,
        auto_start,
        startup_label: startup_label.into(),
    }
}

fn process_entry(image_name: &str) -> Option<(&'static str, OemScope)> {
    let suffix = image_name.get(image_name.len().checked_sub(4)?..)?;
    if !suffix.eq_ignore_ascii_case(".exe") {
        return None;
    }
    let stem = &image_name[..image_name.len() - 4];
    if is_protected(stem) || is_protected(image_name) {
        return None;
    }
    PROCESSES
        .iter()
        .copied()
        .find(|(name, _)| name.eq_ignore_ascii_case(stem))
}

fn for_each_process(
    mut visit: impl FnMut(&'static str, OemScope, u32) -> Result<(), Diagnostic>,
) -> Result<(), Diagnostic> {
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|error| Diagnostic::windows("MSI process snapshot", error))?,
    );
    let mut row = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut row) };
    for _ in 0..65_536 {
        match next {
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(()),
            Err(error) => return Err(Diagnostic::windows("MSI process enumeration", error)),
            Ok(()) => {}
        }
        if let Some((name, tier)) = wide_str(&row.szExeFile).as_deref().and_then(process_entry) {
            visit(name, tier, row.th32ProcessID)?;
        }
        next = unsafe { Process32NextW(snapshot.0, &mut row) };
    }
    Err(Diagnostic::invalid(
        OP,
        "process snapshot exceeded its limit",
    ))
}

fn process_memory(pid: u32) -> Option<u32> {
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut vm = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe { GetProcessMemoryInfo(process.0, &mut vm, vm.cb) }.ok()?;
    u32::try_from(vm.WorkingSetSize / 1024).ok()
}

fn terminate_matching(select: impl Fn(&str, OemScope) -> bool) -> Result<(), Diagnostic> {
    let mut failure = None;
    for_each_process(|name, tier, pid| {
        if select(name, tier) {
            if let Err(error) = terminate_process(name, pid) {
                failure.get_or_insert(error);
            }
        }
        Ok(())
    })?;
    failure.map_or(Ok(()), Err)
}

fn terminate_process(name: &str, pid: u32) -> Result<(), Diagnostic> {
    const OP: &str = "terminate allowlisted MSI process";
    let process = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            pid,
        )
    } {
        Ok(process) => OwnedHandle(process),
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(()),
        Err(error) => return Err(Diagnostic::windows(OP, error).detail(name)),
    };
    if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
        return Ok(());
    }
    let mut buffer = vec![0u16; 32_768];
    let mut len = buffer.len() as u32;
    if let Err(error) = unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    } {
        return if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(Diagnostic::windows(OP, error).detail(name))
        };
    }
    let verified = buffer
        .get(..len as usize)
        .and_then(|words| String::from_utf16(words).ok())
        .and_then(|image| {
            Path::new(&image)
                .file_name()?
                .to_str()
                .and_then(process_entry)
        });
    // Recheck the image on the same handle used to act; the snapshot's PID may have been reused.
    if !verified.is_some_and(|(actual, _)| actual == name) {
        return Ok(());
    }
    if let Err(error) = unsafe { TerminateProcess(process.0, 1) } {
        if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0 {
            return Err(Diagnostic::windows(OP, error).detail(name));
        }
    }
    match unsafe { WaitForSingleObject(process.0, 3_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(Diagnostic::new(OP, DiagnosticKind::TimedOut).detail(name)),
        _ => Err(Diagnostic::last_win32(OP).detail(name)),
    }
}

fn appx_version() -> Result<Option<String>, Diagnostic> {
    const OP: &str = "MSI Center package inventory";
    let (mut count, mut len) = (0, 0);
    match unsafe { GetPackagesByPackageFamily(APPX_FAMILY, &mut count, None, &mut len, None) } {
        ERROR_SUCCESS if count == 0 => return Ok(None),
        ERROR_INSUFFICIENT_BUFFER if (1..=64).contains(&count) && (1..=16_384).contains(&len) => (),
        error => return Err(Diagnostic::win32(OP, error.0)),
    }
    let mut names = vec![PWSTR::null(); count as usize];
    let mut buffer = vec![0u16; len as usize];
    let code = unsafe {
        GetPackagesByPackageFamily(
            APPX_FAMILY,
            &mut count,
            Some(names.as_mut_ptr()),
            &mut len,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(Diagnostic::win32(OP, code.0));
    }
    let mut best = None;
    for name in names.into_iter().take(count as usize) {
        let Some(offset) = (name.0 as usize)
            .checked_sub(buffer.as_ptr() as usize)
            .filter(|n| n % 2 == 0)
        else {
            continue;
        };
        if let Some(version) = buffer
            .get(offset / 2..)
            .and_then(wide_str)
            .as_deref()
            .and_then(package_version)
        {
            best = Some(best.map_or(version, |old: [u16; 4]| old.max(version)));
        }
    }
    if count != 0 && best.is_none() {
        return Err(Diagnostic::invalid(
            OP,
            "no valid MSI Center full package name",
        ));
    }
    Ok(best.map(|[a, b, c, d]| format!("{a}.{b}.{c}.{d}")))
}

fn package_version(full_name: &str) -> Option<[u16; 4]> {
    let mut fields = full_name.split('_');
    if !fields.next()?.eq_ignore_ascii_case(APPX_NAME) {
        return None;
    }
    let mut version = fields.next()?.split('.');
    let mut number = [0; 4];
    for piece in &mut number {
        *piece = version.next()?.parse().ok()?;
    }
    if version.next().is_some() {
        return None;
    }
    let _arch = fields.next()?;
    if !fields.next()?.is_empty()
        || !fields.next()?.eq_ignore_ascii_case("kzh8wxbdkxb8p")
        || fields.next().is_some()
    {
        return None;
    }
    Some(number)
}

#[derive(Default)]
struct Installed {
    version: String,
    launchable: bool,
}

fn desktop_packages() -> Result<[Option<Installed>; 2], Diagnostic> {
    let mut results: [Option<Installed>; 2] = Default::default();
    let views = if cfg!(target_pointer_width = "64") {
        &[KEY_WOW64_64KEY, KEY_WOW64_32KEY][..]
    } else {
        &[KEY_WOW64_32KEY, KEY_WOW64_64KEY][..]
    };
    for &view in views {
        let Some(parent) = reg_open(HKEY_LOCAL_MACHINE, UNINSTALL_KEY, KEY_READ | view)? else {
            continue;
        };
        for index in 0..8192 {
            let mut name = [0u16; 256];
            let mut length = name.len() as u32;
            let code = unsafe {
                RegEnumKeyExW(
                    parent.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            match code {
                ERROR_NO_MORE_ITEMS => break,
                ERROR_SUCCESS if (length as usize) < name.len() => name[length as usize] = 0,
                _ => return Err(Diagnostic::win32("MSI uninstall inventory", code.0)),
            }
            let Some(key) = reg_open(parent.0, PCWSTR(name.as_ptr()), KEY_QUERY_VALUE | view)?
            else {
                continue;
            };
            let value = |name| reg_read(&key, name).ok().flatten().and_then(reg_text);
            let Some(name) = value(w!("DisplayName")) else {
                continue;
            };
            for (slot, package) in results.iter_mut().zip([&SDK, &TRUE_COLOR]) {
                if !name.eq_ignore_ascii_case(package.name)
                    || value(w!("Publisher"))
                        .is_none_or(|label| !label.eq_ignore_ascii_case(package.publisher))
                {
                    continue;
                }
                let launchable = value(w!("UninstallString"))
                    .is_some_and(|command| expected_uninstall(package, &command));
                if slot
                    .as_ref()
                    .is_none_or(|entry| !entry.launchable && launchable)
                {
                    *slot = Some(Installed {
                        version: value(w!("DisplayVersion"))
                            .filter(|v| v.len() <= 128)
                            .unwrap_or_default(),
                        launchable,
                    });
                }
            }
        }
    }
    Ok(results)
}

fn reg_text(value: crate::disabler::RegistryValue) -> Option<String> {
    if value.value_type != REG_SZ.0 || value.bytes.len() % 2 != 0 {
        return None;
    }
    let words = value
        .bytes
        .chunks_exact(2)
        .map(|n| u16::from_le_bytes([n[0], n[1]]))
        .collect::<Vec<_>>();
    let text = String::from_utf16(words.strip_suffix(&[0])?).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

fn expected_uninstall(package: &DesktopPackage, command: &str) -> bool {
    let path = command
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(command);
    path.eq_ignore_ascii_case(package.path) && !is_protected(package.executable)
}

fn uninstall_appx() -> Result<(), Diagnostic> {
    if appx_version()?.is_none() {
        return Ok(());
    }
    terminate_matching(|name, _| name == "DCv2")?;
    let mut system = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut system)) } as usize;
    if len == 0 || len >= system.len() {
        return Err(Diagnostic::last_win32(OP));
    }
    let system = String::from_utf16(&system[..len])
        .map_err(|_| Diagnostic::invalid(OP, "invalid system directory"))?;
    let path = PathBuf::from(system).join(r"WindowsPowerShell\v1.0\powershell.exe");
    let file = checked_local_file(&path, "powershell.exe", OP)?;
    let script = "$ErrorActionPreference = 'Stop'; Import-Module (Join-Path $PSHOME 'Modules\\Appx\\Appx.psd1'); Appx\\Get-AppxPackage -Name '9426MICRO-STARINTERNATION.MSICenter' | Where-Object PackageFamilyName -EQ '9426MICRO-STARINTERNATION.MSICenter_kzh8wxbdkxb8p' | Appx\\Remove-AppxPackage";
    run_uninstaller(
        &path,
        &[
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script,
        ],
        file,
    )?;
    if appx_version()?.is_some() {
        return Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
            .detail("MSI Center AppX remains installed"));
    }
    Ok(())
}

fn uninstall_desktop(
    disabler: &mut SoftwareDisabler,
    package: &DesktopPackage,
    tiers: &[DisableTier],
) -> Result<(), Diagnostic> {
    if !identity::is_elevated()? {
        return Err(Diagnostic::new(OP, DiagnosticKind::AccessDenied).detail("卸载需要管理员权限"));
    }
    let index = usize::from(package.id == OemPackageId::MsiTrueColor);
    let entries = desktop_packages()?;
    let Some(entry) = &entries[index] else {
        return Ok(());
    };
    if !entry.launchable {
        return Err(Diagnostic::invalid(
            OP,
            "uninstaller path does not match the installed MSI package",
        ));
    }

    let mut providers = Vec::new();
    let wmi_before = if package.id == OemPackageId::MsiCenterSdk {
        Some(wmi_binding()?)
    } else {
        None
    };
    if package.id == OemPackageId::MsiCenterSdk {
        // A read-only handle denies other processes write/delete access throughout SDK removal.
        providers.push(checked_provider(Path::new(PROTECTED_DLL), "msiapcfg.dll")?);
    }
    for &path in COMPANION_DLLS {
        match checked_provider(Path::new(path), "msiltcfg.dll") {
            Ok(held) => providers.push(held),
            Err(error) if error.kind == DiagnosticKind::Unavailable => (),
            Err(error) => return Err(error),
        }
    }
    let path = Path::new(package.path);
    let uninstaller = checked_local_file(path, package.executable, OP)?;
    uninstaller.require_admin_path(OP)?;
    let plan = DisablePlan::new(tiers.iter().copied()).map_err(disabler_error)?;
    disabler.stop_services(&plan).map_err(disabler_error)?;
    terminate_matching(|_, tier| tiers.iter().any(|&scope| scope_of(scope) == tier))?;
    let status = run_uninstaller(path, package.args, uninstaller);
    for (held, expected) in &mut providers {
        if fingerprint(held)? != *expected {
            return Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
                .detail("protected MSI provider changed"));
        }
    }
    if package.id == OemPackageId::MsiCenterSdk {
        let (_, current) = checked_provider(Path::new(PROTECTED_DLL), "msiapcfg.dll")?;
        if current != providers[0].1 {
            return Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
                .detail("protected MSI provider changed"));
        }
        if Some(wmi_binding()?) != wmi_before {
            return Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
                .detail("protected WmiAcpi service configuration changed"));
        }
    }
    status?;
    if desktop_packages()?[index].is_some() {
        return Err(
            Diagnostic::new(OP, DiagnosticKind::Rejected).detail("MSI package remains registered")
        );
    }
    Ok(())
}

fn wmi_binding() -> Result<
    (
        crate::disabler::RegistryValue,
        crate::disabler::RegistryValue,
    ),
    Diagnostic,
> {
    let invalid = || {
        Diagnostic::new(OP, DiagnosticKind::Unavailable)
            .detail("protected WmiAcpi MOF registration is missing or disabled")
    };
    let key = reg_open(HKEY_LOCAL_MACHINE, WMI_ACPI_KEY, KEY_QUERY_VALUE)?.ok_or_else(invalid)?;
    let start = reg_read(&key, w!("Start"))?
        .filter(|value| {
            value.value_type == REG_DWORD.0 && matches!(value.bytes.as_slice(), [2 | 3, 0, 0, 0])
        })
        .ok_or_else(invalid)?;
    let mof = reg_read(&key, w!("MofImagePath"))?
        .filter(|value| matches!(value.value_type, 1 | 2) && !value.bytes.is_empty())
        .ok_or_else(invalid)?;
    Ok((start, mof))
}

fn run_uninstaller(path: &Path, args: &[&str], executable: LocalFile) -> Result<(), Diagnostic> {
    let mut child = Command::new(path)
        .args(args)
        .current_dir(path.parent().expect("validated absolute file"))
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Diagnostic::file(OP, error))?;
    drop(executable);
    let status = child.wait().map_err(|error| Diagnostic::file(OP, error))?;
    if status.success() {
        Ok(())
    } else {
        Err(Diagnostic::new(OP, DiagnosticKind::Rejected)
            .detail(format!("uninstaller exited with {status}")))
    }
}

fn checked_provider(path: &Path, name: &str) -> Result<(LocalFile, [u8; 32]), Diagnostic> {
    let mut held = checked_local_file(path, name, OP)?;
    let hash = fingerprint(&mut held)?;
    Ok((held, hash))
}

fn fingerprint(held: &mut LocalFile) -> Result<[u8; 32], Diagnostic> {
    let mut bytes = Vec::new();
    let file = held.file();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|error| Diagnostic::file(OP, error))?;
    if !valid_dll(&bytes) {
        return Err(Diagnostic::invalid(
            OP,
            "invalid protected MSI provider DLL",
        ));
    }
    Ok(Sha256::digest(&bytes).into())
}

fn valid_dll(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"MZ") || !(64..=4 * 1024 * 1024).contains(&bytes.len()) {
        return false;
    }
    let offset = u32::from_le_bytes(bytes[60..64].try_into().expect("DOS header")) as usize;
    offset >= 64
        && bytes
            .get(offset..)
            .and_then(|bytes| bytes.get(..24))
            .is_some_and(|header| {
                header.starts_with(b"PE\0\0")
                    && u16::from_le_bytes([header[22], header[23]]) & 0x2000 != 0
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disabler::{ServiceState, TaskState};

    #[test]
    fn process_matching_uses_only_exact_exe_names_and_scopes() {
        assert_eq!(PROCESSES.len(), 11);
        assert_eq!(
            process_entry("msi.centralserver.EXE"),
            Some(("MSI.CentralServer", OemScope::Core))
        );
        assert_eq!(
            process_entry("ledkeeper2.exe"),
            Some(("LEDKeeper2", OemScope::Lighting))
        );
        assert_eq!(
            process_entry("MsiTrueColorHelper.exe").unwrap().1,
            OemScope::TrueColor
        );
        for name in [
            "DCv2",
            "DCv2.exe.bak",
            "notDCv2.exe",
            r"C:\DCv2.exe",
            "DCv2.exe ",
            "NahimicService.exe",
            "KillerLab.exe",
            "msiapcfg.dll",
            "WmiAcpi.exe",
        ] {
            assert_eq!(process_entry(name), None, "{name}");
        }
        assert!(is_protected("nAHIMIC-anything"));
        assert!(is_protected("KillerCustom"));
    }

    #[test]
    fn inventory_preserves_activity_and_configuration_independently() {
        let service = ObjectState::Service(ServiceState {
            start: ServiceStart::Disabled,
            delayed_auto_start: false,
            activity: ServiceActivity::Running,
        });
        let row = item_status(MsiObject::Center, Some(&service));
        assert!(row.present && row.running && !row.auto_start);
        assert_eq!(row.startup_label, "已禁用");
        assert_eq!(row.display_name, "MSI Center 核心服务");
        let task = ObjectState::Task(TaskState {
            enabled: true,
            active: false,
        });
        assert_eq!(
            item_status(MsiObject::BrokerTask, Some(&task)).startup_label,
            "已启用"
        );
        assert_eq!(
            item_status(MsiObject::StoreStartup, Some(&ObjectState::StoreStartup(4))).tier,
            OemScope::TrueColor
        );
        assert_eq!(
            item_status(MsiObject::StoreStartup, Some(&ObjectState::Missing)).startup_label,
            "未安装"
        );
        assert_eq!(MsiObject::ALL.len(), 11);
    }

    #[test]
    fn package_paths_and_appx_identity_are_not_prefix_matches_or_commands() {
        assert!(expected_uninstall(&SDK, &format!("\"{}\"", SDK.path)));
        for path in [
            r"C:\tmp\unins000.exe",
            SDK.path,
            r"\\other\share\unins000.exe",
        ] {
            assert!(!expected_uninstall(&SDK, &format!("\"{path}\" /S")));
        }
        assert_eq!(
            package_version("9426MICRO-STARINTERNATION.MSICenter_2.0.1.0_x64__kzh8wxbdkxb8p"),
            Some([2, 0, 1, 0])
        );
        assert_eq!(
            package_version("9426MICRO-STARINTERNATION.MSICenter_2.0.1.0_x64__other"),
            None
        );
    }

    #[test]
    fn a_nonempty_file_is_not_necessarily_an_intact_provider_dll() {
        let mut data = [0; 88];
        data[..2].copy_from_slice(b"MZ");
        data[60] = 64;
        data[64..68].copy_from_slice(b"PE\0\0");
        assert!(!valid_dll(&data));
        data[87] = 0x20;
        assert!(valid_dll(&data));
        assert!(!valid_dll(&data[..87]));
    }
}
