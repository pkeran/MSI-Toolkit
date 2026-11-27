//! Startup registration for the daemon under the current interactive user.

use std::fs::{File, OpenOptions};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};

use msilt_core::{StartupMode, StartupStatus};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VariantClear,
};
use windows::core::{BSTR, Interface, PCWSTR, w};

use crate::disabler::{
    RegKey, RegistryValue, current_user_key, lock_parents, reg_open, reg_read, user_sid_string,
    with_scheduler,
};
use crate::{Diagnostic, DiagnosticKind, identity};

const OP: &str = "MSI-Toolkit startup";
const DAEMON: &str = "msilt-daemon.exe";
const DEFAULT_INSTALLED_DAEMON: &str = r"C:\Program Files\MSI-Toolkit\msilt-daemon.exe";
const TASK_FOLDER: &str = r"\MSI-Toolkit";
const TASK_NAME: &str = "Daemon";
const TASK_ARGS: &str = "--acpi --allow-platform-control --enable-ec-writes";
const USER_ARGS: &str = "--readonly --no-tray --pipe MSIToolkit-User";
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("MSIToolkit");
const TRUSTED_INSTALLER: PCWSTR =
    w!("S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464");

fn installed_daemon_path() -> PathBuf {
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
        if let Some(dir) = read_hklm_sz(subkey, value) {
            let trimmed = dir.trim().trim_end_matches(['\\', '/']);
            let candidate = Path::new(trimmed).join(DAEMON);
            if valid_local_path(&candidate, DAEMON) {
                return candidate;
            }
        }
    }
    std::env::var_os("ProgramFiles")
        .map(|pf| PathBuf::from(pf).join("MSI-Toolkit").join(DAEMON))
        .filter(|p| valid_local_path(p, DAEMON))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTALLED_DAEMON))
}

fn read_hklm_sz(subkey: &str, value: &str) -> Option<String> {
    let subkey_w: Vec<u16> = std::ffi::OsStr::new(subkey)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let value_w: Vec<u16> = std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS || !(2..=65_536).contains(&bytes) {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    (!text.is_empty()).then_some(text)
}

pub fn inspect_startup() -> StartupStatus {
    let task = with_scheduler(|service| {
        task_folder(service)?
            .as_ref()
            .map_or(Ok(None), task_enabled)
    });
    let run = read_run();
    let elevation = identity::is_elevated();
    let installed = current_is_installed();
    describe(task, run.map(|value| value.is_some()), elevation, installed)
}

pub fn set_startup(enabled: bool) -> Result<StartupStatus, Diagnostic> {
    let elevated = identity::is_elevated()?;
    let installed_path = installed_daemon_path();
    let installed = installed_path.as_path();
    let mut use_task = false;
    if elevated {
        match checked_local_file(installed, DAEMON, OP) {
            Ok(file) => {
                if enabled {
                    file.require_protected_install(OP)?;
                }
                use_task = true;
                set_task(enabled, installed)?;
                // Do not leave a second launch path when switching to the installed daemon.
                if let Err(error) = set_run(None) {
                    if enabled {
                        let _ = set_task(false, installed);
                    }
                    return Err(error);
                }
            }
            Err(error) if error.kind == DiagnosticKind::Unavailable => {
                if !enabled {
                    // A task can outlive its installation. Disable it without launching it.
                    set_task(false, installed)?;
                } else if with_scheduler(|service| {
                    task_folder(service)?
                        .as_ref()
                        .map_or(Ok(None), task_enabled)
                })? == Some(true)
                {
                    set_task(false, installed)?;
                }
            }
            Err(error) => return Err(error),
        }
    }
    if !use_task {
        if enabled {
            let target = std::env::current_exe()
                .map_err(|error| Diagnostic::file(OP, error))?
                .with_file_name(DAEMON);
            let _file = checked_local_file(&target, DAEMON, OP)?;
            set_run(Some(&run_command(&target)?))?;
        } else {
            set_run(None)?;
        }
    }
    let status = inspect_startup();
    let desired = match (enabled, use_task) {
        (true, true) => StartupMode::ScheduledTask,
        (true, false) => StartupMode::UserRunKey,
        (false, _) => StartupMode::Disabled,
    };
    if status.mode == Some(desired) && status.enabled == enabled {
        Ok(status)
    } else {
        Err(Diagnostic::new(OP, DiagnosticKind::Rejected).detail("启动设置回读与请求不符"))
    }
}

fn describe(
    task: Result<Option<bool>, Diagnostic>,
    run: Result<bool, Diagnostic>,
    elevated: Result<bool, Diagnostic>,
    installed_in_program_files: bool,
) -> StartupStatus {
    let mode = if task == Ok(Some(true)) {
        Some(StartupMode::ScheduledTask)
    } else if run == Ok(true) {
        Some(StartupMode::UserRunKey)
    } else if task.is_ok() && run.is_ok() {
        Some(StartupMode::Disabled)
    } else {
        None
    };
    let task_text = match task {
        Ok(Some(true)) => "已启用",
        Ok(Some(false)) => "已禁用",
        Ok(None) => "未注册",
        Err(_) => "无法读取",
    };
    let run_text = match run {
        Ok(true) => "已注册（只读端点 MSIToolkit-User）",
        Ok(false) => "未注册",
        Err(_) => "无法读取",
    };
    StartupStatus {
        enabled: matches!(
            mode,
            Some(StartupMode::ScheduledTask | StartupMode::UserRunKey)
        ),
        mode,
        elevated: elevated.unwrap_or(false),
        installed_in_program_files,
        detail: format!("计划任务：{task_text}；当前用户 Run 项：{run_text}"),
    }
}

fn current_is_installed() -> bool {
    let expected = installed_daemon_path();
    let Some(expected_str) = expected.to_str() else {
        return false;
    };
    std::env::current_exe().ok().is_some_and(|path| {
        path.to_str().is_some_and(|name| {
            name.strip_prefix(r"\\?\")
                .unwrap_or(name)
                .eq_ignore_ascii_case(expected_str)
        }) && checked_local_file(&expected, DAEMON, OP)
            .and_then(|file| file.require_protected_install(OP))
            .is_ok()
    })
}

fn valid_local_path(path: &Path, expected: &str) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        && components.next() == Some(Component::RootDir)
        && components.all(|part| matches!(part, Component::Normal(name) if name.to_str().is_some_and(|name|
            !name.ends_with([' ', '.']) && !name.contains([':', '"', '/', '\\']) && !name.chars().any(char::is_control))))
        && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

impl LocalFile {
    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn require_admin_path(&self, op: &'static str) -> Result<(), Diagnostic> {
        let handles: Vec<HANDLE> = self
            .parents
            .iter()
            .skip(1)
            .map(|handle| handle.0)
            .chain([HANDLE(self.file.as_raw_handle())])
            .collect();
        check_admin_handles(&handles, op)
    }

    /// Verifies that the installed directory itself (resolved from admin-only HKLM)
    /// and the daemon executable are owned by Administrators/SYSTEM/TrustedInstaller
    /// and disallow non-admin write/delete/modify ACEs. All ancestor directories are
    /// already verified non-reparse by `lock_parents`.
    pub(crate) fn require_protected_install(&self, op: &'static str) -> Result<(), Diagnostic> {
        let mut handles = Vec::with_capacity(2);
        if self.parents.len() > 1 {
            if let Some(install_dir) = self.parents.last() {
                handles.push(install_dir.0);
            }
        }
        handles.push(HANDLE(self.file.as_raw_handle()));
        check_admin_handles(&handles, op)
    }
}

fn check_admin_handles(handles: &[HANDLE], op: &'static str) -> Result<(), Diagnostic> {
    struct LocalAllocation(*mut std::ffi::c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe { LocalFree(Some(HLOCAL(self.0))) };
        }
    }
    let mut trusted = PSID::default();
    unsafe { ConvertStringSidToSidW(TRUSTED_INSTALLER, &mut trusted) }
        .map_err(|error| Diagnostic::windows(op, error))?;
    let _allocation = LocalAllocation(trusted.0);
    let trusted = |sid: PSID| {
        !sid.0.is_null()
            && unsafe {
                IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
                    || IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                    || EqualSid(sid, trusted).is_ok()
            }
    };
    let denied = || {
        Diagnostic::new(op, DiagnosticKind::AccessDenied)
            .detail("launch path permits changes by an untrusted principal")
    };
    for &handle in handles {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let mut owner = PSID::default();
        let mut acl = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut acl),
                None,
                Some(&mut descriptor),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(Diagnostic::win32(op, status.0));
        }
        let _allocation = LocalAllocation(descriptor.0);
        if acl.is_null() || !trusted(owner) {
            return Err(denied());
        }
        for i in 0..u32::from(unsafe { (*acl).AceCount }) {
            let mut raw = std::ptr::null_mut();
            unsafe { GetAce(acl, i, &mut raw) }
                .map_err(|error| Diagnostic::windows(op, error))?;
            let header = unsafe { *raw.cast::<ACE_HEADER>() };
            if header.AceFlags & INHERIT_ONLY_ACE.0 as u8 != 0
                || matches!(header.AceType, 1 | 6 | 10 | 12)
            {
                continue;
            }
            if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
                return Err(denied());
            }
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            let mutations = FILE_WRITE_DATA.0
                | FILE_APPEND_DATA.0
                | FILE_WRITE_EA.0
                | FILE_DELETE_CHILD.0
                | FILE_WRITE_ATTRIBUTES.0
                | DELETE.0
                | WRITE_DAC.0
                | WRITE_OWNER.0
                | GENERIC_ALL.0
                | GENERIC_WRITE.0;
            if ace.Mask & mutations != 0
                && (!matches!(header.AceType, 0 | 9)
                    || !trusted(PSID(std::ptr::from_ref(&ace.SidStart).cast_mut().cast())))
            {
                return Err(denied());
            }
        }
    }
    Ok(())
}

pub(crate) struct LocalFile {
    file: File,
    parents: Vec<crate::native::OwnedHandle>,
}

pub(crate) fn checked_local_file(
    path: &Path,
    expected: &str,
    op: &'static str,
) -> Result<LocalFile, Diagnostic> {
    if !valid_local_path(path, expected) {
        return Err(Diagnostic::invalid(
            op,
            "expected an allowlisted local file path",
        ));
    }
    let parents = lock_parents(path)?;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(|error| Diagnostic::file(op, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| Diagnostic::file(op, error))?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(Diagnostic::invalid(
            op,
            "expected a regular, non-reparse file",
        ));
    }
    Ok(LocalFile { file, parents })
}

fn run_command(path: &Path) -> Result<String, Diagnostic> {
    let path = path
        .to_str()
        .filter(|_| valid_local_path(path, DAEMON))
        .ok_or_else(|| Diagnostic::invalid(OP, "invalid daemon path"))?;
    let command = format!(
        r#""{}" {USER_ARGS}"#,
        path.strip_prefix(r"\\?\").unwrap_or(path)
    );
    if command.encode_utf16().count() >= 260 {
        return Err(Diagnostic::invalid(
            OP,
            "daemon command exceeds the Run key limit",
        ));
    }
    Ok(command)
}

fn read_run() -> Result<Option<RegistryValue>, Diagnostic> {
    let Some(key) = reg_open(current_user_key()?.0, RUN_KEY, KEY_QUERY_VALUE)? else {
        return Ok(None);
    };
    reg_read(&key, RUN_VALUE)
}

fn set_run(command: Option<&str>) -> Result<(), Diagnostic> {
    let user = current_user_key()?;
    let desired = command.map(|text| RegistryValue {
        value_type: REG_SZ.0,
        bytes: text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect(),
    });
    if read_run()? == desired {
        return Ok(());
    }
    let access = KEY_QUERY_VALUE | KEY_SET_VALUE;
    match &desired {
        Some(value) => {
            let mut handle = HKEY::default();
            let status = unsafe {
                RegCreateKeyExW(
                    user.0,
                    RUN_KEY,
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    access,
                    None,
                    &mut handle,
                    None,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(Diagnostic::win32(OP, status.0));
            }
            let key = RegKey(handle);
            let status =
                unsafe { RegSetValueExW(key.0, RUN_VALUE, None, REG_SZ, Some(&value.bytes)) };
            if status != ERROR_SUCCESS {
                return Err(Diagnostic::win32(OP, status.0));
            }
        }
        None => {
            if let Some(key) = reg_open(user.0, RUN_KEY, access)? {
                let status = unsafe { RegDeleteValueW(key.0, RUN_VALUE) };
                if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                    return Err(Diagnostic::win32(OP, status.0));
                }
            }
        }
    }
    if read_run()? != desired {
        return Err(Diagnostic::new(OP, DiagnosticKind::Rejected).detail("Run 项回读与请求不符"));
    }
    Ok(())
}

fn task_folder(service: &ITaskService) -> Result<Option<ITaskFolder>, Diagnostic> {
    optional_task(unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) })
}

fn registered_task(folder: &ITaskFolder) -> Result<Option<IRegisteredTask>, Diagnostic> {
    optional_task(unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) })
}

fn optional_task<T>(result: windows::core::Result<T>) -> Result<Option<T>, Diagnostic> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
                .iter()
                .any(|code| error.code() == code.to_hresult()) =>
        {
            Ok(None)
        }
        Err(error) => Err(Diagnostic::windows(OP, error)),
    }
}

fn task_enabled(folder: &ITaskFolder) -> Result<Option<bool>, Diagnostic> {
    registered_task(folder)?
        .map(|task| {
            unsafe { task.Enabled() }
                .map(|value| value.as_bool())
                .map_err(|error| Diagnostic::windows(OP, error))
        })
        .transpose()
}

fn set_task(enabled: bool, path: &Path) -> Result<(), Diagnostic> {
    with_scheduler(|service| {
        let folder = task_folder(service)?;
        if !enabled {
            if let Some(folder) = &folder {
                if let Some(task) = registered_task(folder)? {
                    unsafe { task.SetEnabled(VARIANT_FALSE) }
                        .map_err(|error| Diagnostic::windows(OP, error))?;
                }
                if task_enabled(folder)? == Some(true) {
                    return Err(
                        Diagnostic::new(OP, DiagnosticKind::Rejected).detail("计划任务仍已启用")
                    );
                }
            }
            return Ok(());
        }
        let sid = user_sid_string()?;
        let user = BSTR::from(sid.as_str());
        let access = TextVariant::new(&format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{sid})"));
        let folder = match folder {
            Some(folder) => folder,
            None => unsafe { service.GetFolder(&BSTR::from(r"\")) }
                .and_then(|root| unsafe {
                    root.CreateFolder(&BSTR::from(&TASK_FOLDER[1..]), &access.0)
                })
                .map_err(|error| Diagnostic::windows(OP, error))?,
        };
        let operation = || -> windows::core::Result<()> {
            unsafe {
                let definition = service.NewTask(0)?;
                let principal = definition.Principal()?;
                principal.SetUserId(&user)?;
                principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
                principal.SetRunLevel(TASK_RUNLEVEL_HIGHEST)?;
                let logon: ILogonTrigger =
                    definition.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
                logon.SetUserId(&user)?;
                logon.SetDelay(&BSTR::from("PT2S"))?;
                let exec: IExecAction = definition.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
                exec.SetPath(&BSTR::from(path.to_string_lossy().as_ref()))?;
                exec.SetArguments(&BSTR::from(TASK_ARGS))?;
                exec.SetWorkingDirectory(&BSTR::from(
                    path.parent()
                        .expect("fixed daemon path")
                        .to_string_lossy()
                        .as_ref(),
                ))?;
                let settings = definition.Settings()?;
                settings.SetEnabled(VARIANT_TRUE)?;
                settings.SetExecutionTimeLimit(&BSTR::from("PT0S"))?;
                settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
                settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
                settings.SetRestartInterval(&BSTR::from("PT1M"))?;
                settings.SetRestartCount(3)?;
                folder.RegisterTaskDefinition(
                    &BSTR::from(TASK_NAME),
                    &definition,
                    TASK_CREATE_OR_UPDATE.0 | TASK_DONT_ADD_PRINCIPAL_ACE.0,
                    &TextVariant::new(&sid).0,
                    &VARIANT::default(),
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &access.0,
                )?;
            }
            Ok(())
        };
        operation().map_err(|error| Diagnostic::windows(OP, error))?;
        if task_enabled(&folder)? != Some(true) {
            return Err(
                Diagnostic::new(OP, DiagnosticKind::Rejected).detail("计划任务未通过启用回读")
            );
        }
        Ok(())
    })
}

struct TextVariant(VARIANT);

impl TextVariant {
    fn new(text: &str) -> Self {
        Self(VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_BSTR,
                    Anonymous: VARIANT_0_0_0 {
                        bstrVal: ManuallyDrop::new(BSTR::from(text)),
                    },
                    ..Default::default()
                }),
            },
        })
    }
}

impl Drop for TextVariant {
    fn drop(&mut self) {
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_commands_require_a_local_daemon_and_quote_the_executable() {
        assert_eq!(
            run_command(Path::new(DEFAULT_INSTALLED_DAEMON)).unwrap(),
            format!(r#""{DEFAULT_INSTALLED_DAEMON}" {USER_ARGS}"#)
        );
        assert_eq!(
            run_command(Path::new(r"D:\Apps\MSI-Toolkit\msilt-daemon.exe")).unwrap(),
            format!(r#""D:\Apps\MSI-Toolkit\msilt-daemon.exe" {USER_ARGS}"#)
        );
        for path in [
            r"msilt-daemon.exe",
            r"C:msilt-daemon.exe",
            r"\\server\share\msilt-daemon.exe",
            r"C:\bad\..\msilt-daemon.exe",
            r"C:\good\evil.exe:msilt-daemon.exe",
            r"C:\good\msilt-daemon.exe ",
            r"C:\good\another.exe",
        ] {
            assert!(run_command(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn disabled_task_does_not_hide_a_run_entry_and_errors_remain_unknown() {
        let status = |task, run| describe(task, run, Ok(true), true);
        assert_eq!(
            status(Ok(Some(true)), Ok(true)).mode,
            Some(StartupMode::ScheduledTask)
        );
        assert_eq!(
            status(Ok(Some(false)), Ok(true)).mode,
            Some(StartupMode::UserRunKey)
        );
        assert_eq!(
            status(Ok(Some(false)), Ok(false)).mode,
            Some(StartupMode::Disabled)
        );
        assert_eq!(
            status(
                Err(Diagnostic::new(OP, DiagnosticKind::AccessDenied)),
                Ok(false)
            )
            .mode,
            None
        );
    }
}
