//! Native Windows install, upgrade, and uninstall operations for MSI-Toolkit.
//!
//! Zero PowerShell subprocesses, zero GPU DLLs, 100% consistent with
//! `msilt-daemon`'s `ProtectedInstall` and `msilt-hal`'s `\MSI-Toolkit\Daemon`
//! scheduled task hardening.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE, HLOCAL, HWND,
    LPARAM, LocalFree, VARIANT_FALSE, VARIANT_TRUE, WPARAM,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, GetDriveTypeW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    MoveFileExW,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WRITE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteKeyW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, IRegisteredTask, ITaskFolder, ITaskService, TASK_ACTION_EXEC,
    TASK_CREATE_OR_UPDATE, TASK_DONT_ADD_PRINCIPAL_ACE, TASK_LOGON_INTERACTIVE_TOKEN,
    TASK_RUNLEVEL_HIGHEST, TASK_TRIGGER_LOGON, TaskScheduler,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VariantClear,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    IShellLinkW, KNOWN_FOLDER_FLAG, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHGetKnownFolderPath, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, SW_SHOWNORMAL, WM_CLOSE,
};
use windows::core::{BSTR, GUID, Interface, PCWSTR, PWSTR, w};

use crate::archive::{self, BUNDLE_FILES};

pub const APP_NAME: &str = "MSI-Toolkit";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO_URL: &str = "https://github.com/pkeran/MSI-Toolkit";
const TASK_FOLDER: &str = r"\MSI-Toolkit";
const TASK_NAME: &str = "Daemon";
const DAEMON_ARGS: &str = "--acpi --allow-platform-control --enable-ec-writes";
const APP_REG_KEY: PCWSTR = w!(r"SOFTWARE\MSI-Toolkit");
const UNINSTALL_REG_KEY: PCWSTR =
    w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit");
const INNO_UNINSTALL_REG_KEY: PCWSTR =
    w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit_is1");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("MSIToolkit");
const HARDENED_INSTALL_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
const DRIVE_FIXED: u32 = 3;

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub auto_start: bool,
    pub start_menu: bool,
    pub desktop_shortcut: bool,
    pub launch_ui: bool,
    pub source_dir: Option<PathBuf>,
    pub from_update: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct UninstallOptions {
    pub purge_config: bool,
}

#[derive(Debug, Clone)]
pub struct SystemProbe {
    pub install_dir: PathBuf,
    pub installed: bool,
    pub installed_version: Option<String>,
    pub auto_start_enabled: bool,
    pub start_menu_exists: bool,
    pub desktop_shortcut_exists: bool,
    pub has_payload: bool,
}

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elev).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

pub fn install_dir() -> PathBuf {
    for (key, value) in [
        (APP_REG_KEY, w!("InstallDir")),
        (INNO_UNINSTALL_REG_KEY, w!("InstallLocation")),
        (UNINSTALL_REG_KEY, w!("InstallLocation")),
    ] {
        if let Some(raw) = read_hklm_sz(key, value) {
            let trimmed = raw.trim().trim_end_matches(['\\', '/']);
            let candidate = PathBuf::from(trimmed);
            if validate_local_fixed_dir(&candidate).is_ok() {
                return candidate;
            }
        }
    }
    known_folder(&FOLDERID_ProgramFiles)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files"))
        .join(APP_NAME)
}

fn read_hklm_sz(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
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
            subkey,
            value,
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

fn write_install_dir_reg(dir: &Path) -> Result<(), String> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            APP_REG_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("无法写入安装路径注册表（错误码 {}）", status.0));
    }
    let _guard = RegKeyGuard(key);
    let clean = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
    set_reg_sz(key, w!("InstallDir"), &clean)
}

fn delete_install_dir_reg() {
    unsafe {
        let _ = RegDeleteKeyW(HKEY_LOCAL_MACHINE, APP_REG_KEY);
    }
}

pub fn validate_local_fixed_dir(dir: &Path) -> Result<(), String> {
    use std::path::{Component, Prefix};
    let mut comps = dir.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return Err("安装路径必须是本地磁盘的绝对路径。".into());
    };
    if !matches!(prefix.kind(), Prefix::Disk(_)) || comps.next() != Some(Component::RootDir) {
        return Err("安装路径必须是本地固定磁盘上的目录。".into());
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(r"\");
    let root_w = wide(&root);
    if unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) } != DRIVE_FIXED {
        return Err("仅支持安装到本地固定磁盘。".into());
    }
    let mut depth = 0usize;
    for part in comps {
        match part {
            Component::Normal(name) => {
                let s = name.to_string_lossy();
                if s.is_empty() || s.ends_with([' ', '.']) || s.contains([':', '"', '/', '\\']) {
                    return Err("安装路径包含无效字符。".into());
                }
                depth += 1;
            }
            _ => return Err("安装路径不能包含相对段。".into()),
        }
    }
    if depth == 0 {
        return Err("不能直接安装到磁盘根目录，请选择专用子目录。".into());
    }
    Ok(())
}

fn verify_no_reparse_chain(dir: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    use std::path::Component;
    let mut comps = dir.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return Err("安装路径无效。".into());
    };
    if comps.next() != Some(Component::RootDir) {
        return Err("安装路径缺少根目录。".into());
    }
    let mut current = PathBuf::from(prefix.as_os_str());
    current.push(r"\");
    for part in comps {
        let Component::Normal(name) = part else {
            return Err("安装路径包含非法段。".into());
        };
        current.push(name);
        let meta = fs::symlink_metadata(&current)
            .map_err(|e| format!("无法校验目录 {}：{e}", current.display()))?;
        if !meta.is_dir() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
            return Err(format!(
                "安全检查失败：{} 是符号链接或联接点。",
                current.display()
            ));
        }
    }
    Ok(())
}

/// Applies a protected admin-only NTFS ACL (`SYSTEM`/`Administrators` Full,
/// `Users` Read & Execute only) to the installation directory and installed
/// executables so custom locations (e.g. `D:\Apps\MSI-Toolkit`) match the
/// security boundary of `C:\Program Files\MSI-Toolkit`.
fn harden_install_dir(dir: &Path) -> Result<(), String> {
    validate_local_fixed_dir(dir)?;
    verify_no_reparse_chain(dir)?;

    let sddl_w = wide(HARDENED_INSTALL_SDDL);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .map_err(|e| format!("无法构建目录安全描述符：{e}"))?;
    }
    struct SdGuard(PSECURITY_DESCRIPTOR);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.0)));
                }
            }
        }
    }
    let _sd_guard = SdGuard(sd);

    let mut owner = PSID::default();
    let mut owner_defaulted = windows::core::BOOL::default();
    let mut dacl_present = windows::core::BOOL::default();
    let mut dacl_defaulted = windows::core::BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorOwner(sd, &mut owner, &mut owner_defaulted)
            .map_err(|e| format!("读取安全描述符所有者失败：{e}"))?;
        GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted)
            .map_err(|e| format!("读取安全描述符 ACL 失败：{e}"))?;
    }
    if !dacl_present.as_bool() || dacl.is_null() || owner.0.is_null() {
        return Err("目录安全描述符无效。".into());
    }

    let apply_one = |target: &Path| -> Result<(), String> {
        let target_w = wide(target);
        let status = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(target_w.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(owner),
                None,
                Some(dacl),
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "无法设置 {} 的安全权限（错误码 {}）",
                target.display(),
                status.0
            ));
        }
        Ok(())
    };

    apply_one(dir)?;
    for name in [
        "msilt-daemon.exe",
        "msilt-ui.exe",
        "msilt-cli.exe",
        "msilt-probe.exe",
        "msilt-maintenance.exe",
        "uninstall.exe",
    ] {
        let p = dir.join(name);
        if p.is_file() {
            apply_one(&p)?;
        }
    }
    Ok(())
}

pub fn start_menu_shortcut_path() -> PathBuf {
    known_folder(&FOLDERID_ProgramData)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
        .join(r"Microsoft\Windows\Start Menu\Programs\MSI-Toolkit.lnk")
}

pub fn desktop_shortcut_path() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
        .ok()
        .map(|d| d.join("MSI-Toolkit.lnk"))
}

pub fn local_data_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData)
        .ok()
        .map(|d| d.join(APP_NAME))
}

pub fn probe_system(source_dir: Option<&Path>) -> SystemProbe {
    let dir = install_dir();
    let installed = dir.join("msilt-daemon.exe").is_file();
    let installed_version = read_installed_version();
    let task_state = query_task_enabled().unwrap_or(None);
    let auto_start_enabled = if installed {
        task_state.unwrap_or(true)
    } else {
        true
    };
    let start_menu_exists = if installed {
        start_menu_shortcut_path().is_file()
    } else {
        true
    };
    let desktop_shortcut_exists = desktop_shortcut_path().is_some_and(|p| p.is_file());
    let has_payload = source_dir
        .is_some_and(|d| d.join("msilt-daemon.exe").is_file())
        || std::env::current_exe()
            .ok()
            .and_then(|exe| archive::inspect_overlay(&exe).ok().flatten())
            .is_some();

    SystemProbe {
        install_dir: dir,
        installed,
        installed_version,
        auto_start_enabled,
        start_menu_exists,
        desktop_shortcut_exists,
        has_payload,
    }
}

/// Launches the current executable elevated via UAC without waiting (so a
/// non-elevated uninstaller can exit immediately and release its image lock).
pub fn elevate_detached(args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径：{e}"))?;
    let exe_w = wide(&exe);
    let verb_w = wide("runas");
    let params = join_windows_args(args);
    let params_w = wide(&params);

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    unsafe {
        if ShellExecuteExW(&mut sei).is_err() {
            return Err("已取消管理员授权，操作未执行。".into());
        }
    }
    Ok(())
}

/// Launches the current executable elevated via UAC and waits for its exit code.
pub fn elevate_and_wait(args: &[String]) -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径：{e}"))?;
    let exe_w = wide(&exe);
    let verb_w = wide("runas");
    let params = join_windows_args(args);
    let params_w = wide(&params);

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    unsafe {
        if ShellExecuteExW(&mut sei).is_err() {
            return Err("已取消管理员授权，操作未执行。".into());
        }
        if !sei.hProcess.is_invalid() {
            WaitForSingleObject(sei.hProcess, u32::MAX);
            let mut code: u32 = 0;
            let _ = GetExitCodeProcess(sei.hProcess, &mut code);
            let _ = CloseHandle(sei.hProcess);
            Ok(code as i32)
        } else {
            Ok(0)
        }
    }
}

pub fn run_install(opts: &InstallOptions, progress: impl Fn(u32, &str)) -> Result<(), String> {
    if !is_elevated() {
        return Err("安装到 Program Files 需要管理员权限。".into());
    }
    let target_dir = install_dir();
    let current_exe = std::env::current_exe().map_err(|e| format!("无法读取安装包路径：{e}"))?;

    progress(10, "正在停止运行中的 MSI-Toolkit 进程…");
    stop_running_processes();

    progress(22, "正在准备受保护安装目录…");
    fs::create_dir_all(&target_dir)
        .map_err(|e| format!("无法创建安装目录 {}：{e}", target_dir.display()))?;

    let mut total_bytes: u64 = 0;
    if let Some(src_dir) = &opts.source_dir {
        for (idx, &name) in BUNDLE_FILES.iter().enumerate() {
            let pct = 25 + (idx as u32) * 11;
            progress(pct, &format!("正在写入 {name}…"));
            let bytes = fs::read(src_dir.join(name))
                .map_err(|e| format!("读取 {name} 失败：{e}"))?;
            total_bytes += bytes.len() as u64;
            write_file_atomic(&target_dir.join(name), &bytes)?;
        }
    } else {
        let meta = archive::inspect_overlay(&current_exe)
            .map_err(|e| format!("读取安装包载荷失败：{e}"))?
            .ok_or_else(|| "当前安装包不包含内置程序文件，请使用完整 MSI-Toolkit-Setup.exe。".to_string())?;
        let entries = archive::list_entries(&current_exe, meta)
            .map_err(|e| format!("解析安装包目录失败：{e}"))?;
        for (idx, entry) in entries.iter().enumerate() {
            let pct = 25 + (idx as u32) * 11;
            progress(pct, &format!("正在解压并校验 {}…", entry.name));
            let bytes = archive::read_entry_bytes(&current_exe, entry)
                .map_err(|e| format!("解压 {} 失败：{e}", entry.name))?;
            total_bytes += bytes.len() as u64;
            write_file_atomic(&target_dir.join(&entry.name), &bytes)?;
        }
    }

    progress(72, "正在安装卸载与维护组件…");
    let uninst_tmp = target_dir.join("uninstall.exe.new");
    let uninst_dst = target_dir.join("uninstall.exe");
    let stub_bytes = archive::write_stub_copy(&current_exe, &uninst_tmp)
        .map_err(|e| format!("写入卸载组件失败：{e}"))?;
    move_replace(&uninst_tmp, &uninst_dst)?;
    total_bytes += stub_bytes;

    // Clean up legacy script uninstallers if upgrading from an older script install.
    for legacy in ["install.ps1", "install.cmd", "uninstall.ps1", "uninstall.cmd", "README.txt"] {
        let p = target_dir.join(legacy);
        if p.is_file() {
            let _ = fs::remove_file(p);
        }
    }

    progress(80, "正在注册 Windows 应用信息与快捷方式…");
    harden_install_dir(&target_dir)?;
    write_install_dir_reg(&target_dir)?;
    register_uninstall_entry(&target_dir, total_bytes)?;
    let ui_exe = target_dir.join("msilt-ui.exe");
    let start_lnk = start_menu_shortcut_path();
    if opts.start_menu {
        create_shortcut(
            &start_lnk,
            &ui_exe,
            &target_dir,
            "MSI-Toolkit - 轻量原生笔记本控制中心",
        )?;
    } else if start_lnk.is_file() {
        let _ = fs::remove_file(&start_lnk);
    }

    if let Some(desk_lnk) = desktop_shortcut_path() {
        if opts.desktop_shortcut {
            create_shortcut(
                &desk_lnk,
                &ui_exe,
                &target_dir,
                "MSI-Toolkit - 轻量原生笔记本控制中心",
            )?;
        } else if !opts.from_update && desk_lnk.is_file() {
            let _ = fs::remove_file(&desk_lnk);
        }
    }

    progress(88, "正在配置后台守护任务（免 UAC）…");
    let daemon_exe = target_dir.join("msilt-daemon.exe");
    register_daemon_task(&daemon_exe, opts.auto_start)?;
    remove_user_run_key();

    progress(94, "正在启动后台守护进程…");
    start_daemon(&daemon_exe, &target_dir, opts.auto_start)?;
    progress(96, "正在确认后台已连接…");
    wait_for_daemon(&target_dir)?;

    if opts.launch_ui {
        launch_ui_medium(&ui_exe);
    }

    if opts.from_update {
        schedule_temp_cleanup(&current_exe);
    }

    progress(100, if opts.from_update { "升级完成" } else { "安装完成" });
    Ok(())
}

pub fn run_uninstall(opts: &UninstallOptions, progress: impl Fn(u32, &str)) -> Result<(), String> {
    if !is_elevated() {
        return Err("卸载程序需要管理员权限。".into());
    }
    let target_dir = install_dir();

    progress(15, "正在停止 MSI-Toolkit 后台服务与界面…");
    stop_running_processes();

    progress(38, "正在移除开机任务、快捷方式与系统应用项…");
    remove_daemon_task();
    remove_user_run_key();
    let _ = fs::remove_file(start_menu_shortcut_path());
    if let Some(desk) = desktop_shortcut_path() {
        let _ = fs::remove_file(desk);
    }
    unregister_uninstall_entry();
    delete_install_dir_reg();

    progress(65, "正在删除程序文件…");
    if target_dir.is_dir() {
        for name in [
            "msilt-daemon.exe",
            "msilt-ui.exe",
            "msilt-cli.exe",
            "msilt-probe.exe",
            "install.ps1",
            "install.cmd",
            "uninstall.ps1",
            "uninstall.cmd",
            "README.txt",
        ] {
            let _ = fs::remove_file(target_dir.join(name));
        }
    }

    if opts.purge_config {
        progress(82, "正在清除本地用户配置与自动化规则…");
        if let Some(data_dir) = local_data_dir() {
            let _ = fs::remove_dir_all(data_dir);
        }
    }

    progress(95, "正在清理安装目录（系统底层驱动未受影响）…");
    schedule_dir_self_delete(&target_dir);

    progress(100, "卸载完成，系统底层驱动均保持完好");
    Ok(())
}

// ── Process management ──────────────────────────────────────────────────────

fn stop_running_processes() {
    let targets = find_pids_by_name(&["msilt-ui.exe", "msilt-daemon.exe"]);
    if targets.is_empty() {
        return;
    }
    // Ask windows to close cleanly first so tray icon & IPC pipe tear down gracefully.
    for &pid in &targets {
        close_windows_for_pid(pid);
    }
    let deadline = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < deadline {
        if find_pids_by_name(&["msilt-ui.exe", "msilt-daemon.exe"]).is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(80));
    }
    for pid in find_pids_by_name(&["msilt-ui.exe", "msilt-daemon.exe"]) {
        unsafe {
            if let Ok(proc) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) {
                let _ = TerminateProcess(proc, 0);
                let _ = WaitForSingleObject(proc, 1_000);
                let _ = CloseHandle(proc);
            }
        }
    }
    thread::sleep(Duration::from_millis(120));
}

fn find_pids_by_name(names: &[&str]) -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if names.iter().any(|n| exe.eq_ignore_ascii_case(n)) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    pids
}

fn close_windows_for_pid(target_pid: u32) {
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let want = lparam.0 as u32;
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == want {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(target_pid as isize));
    }
}

// ── File I/O helpers ────────────────────────────────────────────────────────

fn write_file_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = dest.with_extension("exe.new");
    fs::write(&tmp, bytes).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    move_replace(&tmp, dest)
}

fn move_replace(src: &Path, dst: &Path) -> Result<(), String> {
    let src_w = wide(src);
    let dst_w = wide(dst);
    for attempt in 0..8 {
        let res = unsafe {
            MoveFileExW(
                PCWSTR(src_w.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if res.is_ok() {
            return Ok(());
        }
        if attempt == 7 {
            let _ = fs::remove_file(src);
            return Err(format!(
                "替换 {} 失败（文件可能仍被占用，请重试）",
                dst.display()
            ));
        }
        thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}

// ── Registry: Windows Settings -> Apps & Features ───────────────────────────

fn read_installed_version() -> Option<String> {
    let mut buf = [0u16; 64];
    let mut bytes = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            UNINSTALL_REG_KEY,
            w!("DisplayVersion"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS || bytes < 2 {
        return None;
    }
    let len = ((bytes as usize) / 2).saturating_sub(1).min(buf.len());
    let ver = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    (!ver.is_empty()).then_some(ver)
}

fn register_uninstall_entry(install_dir: &Path, total_bytes: u64) -> Result<(), String> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            UNINSTALL_REG_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("无法写入系统应用注册表（错误码 {}）", status.0));
    }
    let _guard = RegKeyGuard(key);

    let dir_str = install_dir.to_string_lossy();
    let icon = format!("{}\\msilt-ui.exe,0", dir_str);
    let uninst = format!("\"{}\\uninstall.exe\" --uninstall", dir_str);
    let quiet = format!("\"{}\\uninstall.exe\" --uninstall --quiet", dir_str);
    let size_kib = (total_bytes / 1024).max(1) as u32;

    set_reg_sz(key, w!("DisplayName"), APP_NAME)?;
    set_reg_sz(key, w!("DisplayVersion"), APP_VERSION)?;
    set_reg_sz(key, w!("Publisher"), APP_NAME)?;
    set_reg_sz(key, w!("InstallLocation"), &dir_str)?;
    set_reg_sz(key, w!("DisplayIcon"), &icon)?;
    set_reg_sz(key, w!("UninstallString"), &uninst)?;
    set_reg_sz(key, w!("QuietUninstallString"), &quiet)?;
    set_reg_sz(key, w!("URLInfoAbout"), REPO_URL)?;
    set_reg_dword(key, w!("EstimatedSize"), size_kib)?;
    set_reg_dword(key, w!("NoModify"), 1)?;
    set_reg_dword(key, w!("NoRepair"), 1)?;
    Ok(())
}

fn unregister_uninstall_entry() {
    unsafe {
        let _ = RegDeleteKeyW(HKEY_LOCAL_MACHINE, UNINSTALL_REG_KEY);
    }
}

fn remove_user_run_key() {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        ) == ERROR_SUCCESS
        {
            let _guard = RegKeyGuard(key);
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
    }
}

fn set_reg_sz(key: HKEY, name: PCWSTR, value: &str) -> Result<(), String> {
    let w = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr().cast::<u8>(), w.len() * 2) };
    let status = unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)) };
    if status != ERROR_SUCCESS {
        return Err(format!("写入注册表键值失败（错误码 {}）", status.0));
    }
    Ok(())
}

fn set_reg_dword(key: HKEY, name: PCWSTR, value: u32) -> Result<(), String> {
    let bytes = value.to_le_bytes();
    let status = unsafe { RegSetValueExW(key, name, None, REG_DWORD, Some(&bytes)) };
    if status != ERROR_SUCCESS {
        return Err(format!("写入注册表数值失败（错误码 {}）", status.0));
    }
    Ok(())
}

struct RegKeyGuard(HKEY);
impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

// ── Shortcuts via COM IShellLinkW ───────────────────────────────────────────

fn create_shortcut(
    lnk_path: &Path,
    target: &Path,
    workdir: &Path,
    description: &str,
) -> Result<(), String> {
    if let Some(parent) = lnk_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    with_com(|| unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建快捷方式对象失败：{e}"))?;
        let target_w = wide(target);
        let workdir_w = wide(workdir);
        let desc_w = wide(description);
        link.SetPath(PCWSTR(target_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式路径失败：{e}"))?;
        link.SetWorkingDirectory(PCWSTR(workdir_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式目录失败：{e}"))?;
        link.SetDescription(PCWSTR(desc_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式描述失败：{e}"))?;
        link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)
            .map_err(|e| format!("设置快捷方式图标失败：{e}"))?;
        let persist: IPersistFile = link
            .cast()
            .map_err(|e| format!("获取快捷方式持久化接口失败：{e}"))?;
        let lnk_w = wide(lnk_path);
        persist
            .Save(PCWSTR(lnk_w.as_ptr()), true)
            .map_err(|e| format!("保存快捷方式失败：{e}"))?;
        Ok(())
    })
}

// ── Task Scheduler: \MSI-Toolkit\Daemon ─────────────────────────────────────

fn query_task_enabled() -> Result<Option<bool>, String> {
    with_scheduler(|service| {
        let Some(folder) = optional_task(unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) })?
        else {
            return Ok(None);
        };
        let Some(task) = optional_task(unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) })? else {
            return Ok(None);
        };
        let enabled = unsafe { task.Enabled() }
            .map_err(|e| format!("读取任务状态失败：{e}"))?
            .as_bool();
        Ok(Some(enabled))
    })
}

fn register_daemon_task(daemon_exe: &Path, enabled: bool) -> Result<(), String> {
    let sid = current_user_sid()?;
    let access = TextVariant::new(&format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{sid})"));
    let user_bstr = BSTR::from(sid.as_str());

    with_scheduler(|service| {
        let folder: ITaskFolder =
            match optional_task(unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) })? {
                Some(folder) => folder,
                None => unsafe {
                    let root = service
                        .GetFolder(&BSTR::from(r"\"))
                        .map_err(|e| format!("打开任务计划根目录失败：{e}"))?;
                    root.CreateFolder(&BSTR::from(&TASK_FOLDER[1..]), &access.0)
                        .map_err(|e| format!("创建任务计划目录失败：{e}"))?
                },
            };

        unsafe {
            let def = service
                .NewTask(0)
                .map_err(|e| format!("创建计划任务定义失败：{e}"))?;
            let principal = def
                .Principal()
                .map_err(|e| format!("获取任务主体失败：{e}"))?;
            principal
                .SetUserId(&user_bstr)
                .map_err(|e| format!("设置任务用户失败：{e}"))?;
            principal
                .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
                .map_err(|e| format!("设置任务登录类型失败：{e}"))?;
            principal
                .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
                .map_err(|e| format!("设置任务提权级别失败：{e}"))?;

            let logon: ILogonTrigger = def
                .Triggers()
                .and_then(|t| t.Create(TASK_TRIGGER_LOGON))
                .and_then(|t| t.cast())
                .map_err(|e| format!("创建登录触发器失败：{e}"))?;
            logon
                .SetUserId(&user_bstr)
                .map_err(|e| format!("绑定登录触发器用户失败：{e}"))?;

            let exec: IExecAction = def
                .Actions()
                .and_then(|a| a.Create(TASK_ACTION_EXEC))
                .and_then(|a| a.cast())
                .map_err(|e| format!("创建执行动作失败：{e}"))?;
            exec.SetPath(&BSTR::from(daemon_exe.to_string_lossy().as_ref()))
                .map_err(|e| format!("设置守护进程路径失败：{e}"))?;
            exec.SetArguments(&BSTR::from(DAEMON_ARGS))
                .map_err(|e| format!("设置守护进程参数失败：{e}"))?;
            if let Some(dir) = daemon_exe.parent() {
                exec.SetWorkingDirectory(&BSTR::from(dir.to_string_lossy().as_ref()))
                    .map_err(|e| format!("设置守护进程工作目录失败：{e}"))?;
            }

            let settings = def
                .Settings()
                .map_err(|e| format!("读取任务配置失败：{e}"))?;
            settings
                .SetEnabled(if enabled { VARIANT_TRUE } else { VARIANT_FALSE })
                .map_err(|e| format!("设置任务启停状态失败：{e}"))?;
            settings
                .SetExecutionTimeLimit(&BSTR::from("PT0S"))
                .map_err(|e| format!("设置任务无超时失败：{e}"))?;
            settings
                .SetDisallowStartIfOnBatteries(VARIANT_FALSE)
                .map_err(|e| format!("允许电池模式启动失败：{e}"))?;
            settings
                .SetStopIfGoingOnBatteries(VARIANT_FALSE)
                .map_err(|e| format!("允许电池模式保持运行失败：{e}"))?;

            folder
                .RegisterTaskDefinition(
                    &BSTR::from(TASK_NAME),
                    &def,
                    TASK_CREATE_OR_UPDATE.0 | TASK_DONT_ADD_PRINCIPAL_ACE.0,
                    &TextVariant::new(&sid).0,
                    &VARIANT::default(),
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &access.0,
                )
                .map_err(|e| format!("注册开机任务失败：{e}"))?;
        }
        Ok(())
    })
}

fn start_daemon(daemon_exe: &Path, workdir: &Path, via_task: bool) -> Result<(), String> {
    if via_task {
        let ran = with_scheduler(|service| {
            let Some(folder) =
                optional_task(unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) })?
            else {
                return Ok(false);
            };
            let Some(task): Option<IRegisteredTask> =
                optional_task(unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) })?
            else {
                return Ok(false);
            };
            unsafe { task.Run(&VARIANT::default()) }
                .map_err(|e| format!("启动计划任务失败：{e}"))?;
            Ok(true)
        });
        if matches!(ran, Ok(true)) {
            return Ok(());
        }
    }

    Command::new(daemon_exe)
        .args(["--acpi", "--allow-platform-control", "--enable-ec-writes"])
        .current_dir(workdir)
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|e| format!("启动守护进程失败：{e}"))?;
    Ok(())
}

pub fn wizard_action(args: &[String]) -> Result<(), String> {
    if !is_elevated() {
        return Err("需要管理员权限".into());
    }
    let Some(action) = args.first().map(String::as_str) else {
        return Err("缺少安装操作参数".into());
    };

    let mut explicit_dir: Option<PathBuf> = None;
    let mut autostart_mode: Option<String> = None;
    let mut purge = false;
    let mut idx = 1;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--dir" {
            if let Some(v) = args.get(idx + 1) {
                explicit_dir = Some(PathBuf::from(v.trim().trim_end_matches(['\\', '/'])));
                idx += 1;
            }
        } else if let Some(val) = arg.strip_prefix("--autostart=") {
            autostart_mode = Some(val.to_ascii_lowercase());
        } else if arg == "--purge" {
            purge = true;
        }
        idx += 1;
    }

    match action {
        "--wizard-stop" => stop_running_processes(),
        "--wizard-remove" => {
            stop_running_processes();
            remove_daemon_task();
            remove_user_run_key();
            unregister_uninstall_entry();
            delete_install_dir_reg();
            // Remove legacy pre-Inno direct Start Menu link if present.
            let legacy_lnk = start_menu_shortcut_path();
            if legacy_lnk.is_file() {
                let _ = fs::remove_file(&legacy_lnk);
            }
            if purge {
                if let Some(data_dir) = local_data_dir() {
                    let _ = fs::remove_dir_all(data_dir);
                }
            }
        }
        "--wizard-start" => {
            let dir = explicit_dir
                .or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(Path::to_path_buf))
                })
                .unwrap_or_else(install_dir);
            harden_install_dir(&dir)?;
            write_install_dir_reg(&dir)?;

            // Clean any legacy pre-Inno helper scripts/artifacts in the target folder.
            for legacy in [
                "uninstall.exe",
                "install.ps1",
                "install.cmd",
                "uninstall.ps1",
                "uninstall.cmd",
                "README.txt",
            ] {
                let p = dir.join(legacy);
                if p.is_file() {
                    let _ = fs::remove_file(p);
                }
            }
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.starts_with("startup-check-") && name.ends_with(".txt") {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }

            let exe = dir.join("msilt-daemon.exe");
            let enabled = match autostart_mode.as_deref() {
                Some("0" | "false") => false,
                Some("1" | "true") => true,
                _ => query_task_enabled()?.unwrap_or(true),
            };
            register_daemon_task(&exe, enabled)?;
            start_daemon(&exe, &dir, enabled)?;
            wait_for_daemon(&dir)?;
            unregister_uninstall_entry(); // Retire the pre-Inno entry only after a successful migration.
            remove_user_run_key();
            // Clean legacy pre-Inno flat Start Menu link so Start Menu doesn't show duplicates.
            let legacy_lnk = start_menu_shortcut_path();
            if legacy_lnk.is_file() {
                let _ = fs::remove_file(&legacy_lnk);
            }
        }
        _ => return Err("未知安装操作".into()),
    }
    Ok(())
}

fn wait_for_daemon(dir: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = Command::new(dir.join("msilt-cli.exe"))
            .args(["--json", "status"])
            .current_dir(dir)
            .creation_flags(CREATE_NO_WINDOW.0)
            .output().map_err(|e| format!("无法检查后台状态：{e}"))?;
        if output.status.success() { return Ok(()); }
        if Instant::now() >= deadline {
            return Err(format!("程序文件已安装，但后台没有响应。\n{}", String::from_utf8_lossy(&output.stderr).trim()));
        }
        thread::sleep(Duration::from_millis(300));
    }
}

fn remove_daemon_task() {
    let _ = with_scheduler(|service| {
        if let Some(folder) =
            optional_task(unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) })?
        {
            let _ = unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) };
        }
        if let Ok(root) = unsafe { service.GetFolder(&BSTR::from(r"\")) } {
            let _ = unsafe { root.DeleteFolder(&BSTR::from(&TASK_FOLDER[1..]), 0) };
        }
        Ok(())
    });
}

fn launch_ui_medium(ui_exe: &Path) {
    // Delegating to explorer.exe ensures msilt-ui.exe runs in the Medium-integrity
    // desktop session (never High integrity) so daemon IPC UI authentication succeeds.
    let _ = Command::new("explorer.exe").arg(ui_exe).spawn();
}

fn schedule_dir_self_delete(dir: &Path) {
    let dir_s = dir.to_string_lossy().replace('"', "");
    // Use raw_arg so Rust does not backslash-escape quotes (cmd.exe /C treats \" as literal).
    let raw = format!(
        "/C \"for /L %i in (1,1,10) do @(ping -n 2 127.0.0.1 >nul & rmdir /s /q \"{dir_s}\" 2>nul & if not exist \"{dir_s}\" exit 0)\""
    );
    let _ = Command::new("cmd.exe")
        .raw_arg(&raw)
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn();
}

fn schedule_temp_cleanup(current_exe: &Path) {
    let temp = std::env::temp_dir();
    if let Some(parent) = current_exe.parent() {
        let p_low = parent.to_string_lossy().to_ascii_lowercase();
        let t_low = temp.to_string_lossy().to_ascii_lowercase();
        if p_low.starts_with(&t_low) && p_low.contains("msi-toolkit-update-") {
            let dir_s = parent.to_string_lossy().replace('"', "");
            let raw = format!("/C \"ping -n 4 127.0.0.1 >nul & rmdir /s /q \"{dir_s}\" 2>nul\"");
            let _ = Command::new("cmd.exe")
                .raw_arg(&raw)
                .current_dir(&temp)
                .creation_flags(CREATE_NO_WINDOW.0)
                .spawn();
        }
    }
}

// ── Low-level Win32 helpers ─────────────────────────────────────────────────

fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    let text = unsafe { SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None) }
        .map_err(|e| format!("无法获取系统目录：{e}"))?;
    struct CoPath(PWSTR);
    impl Drop for CoPath {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
        }
    }
    let text = CoPath(text);
    Ok(PathBuf::from(OsString::from_wide(unsafe {
        text.0.as_wide()
    })))
}

fn current_user_sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| format!("读取当前进程令牌失败：{e}"))?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; (len as usize).max(64)];
        let res = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        res.map_err(|e| format!("读取用户 SID 失败：{e}"))?;
        let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut raw_sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut raw_sid)
            .map_err(|e| format!("转换用户 SID 失败：{e}"))?;
        let text = raw_sid
            .to_string()
            .map_err(|e| format!("解码 SID 字符串失败：{e}"))?;
        let _ = LocalFree(Some(HLOCAL(raw_sid.0.cast())));
        Ok(text)
    }
}

fn with_com<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let uninit = hr.is_ok();
    let result = f();
    if uninit {
        unsafe { CoUninitialize() };
    }
    result
}

fn with_scheduler<T>(f: impl FnOnce(&ITaskService) -> Result<T, String>) -> Result<T, String> {
    with_com(|| unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_ALL)
            .map_err(|e| format!("初始化任务计划程序失败：{e}"))?;
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .map_err(|e| format!("连接任务计划程序失败：{e}"))?;
        f(&service)
    })
}

fn optional_task<T>(res: windows::core::Result<T>) -> Result<Option<T>, String> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e)
            if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
                .iter()
                .any(|c| e.code() == c.to_hresult()) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("访问任务计划程序失败：{e}")),
    }
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

pub fn join_windows_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_arg_quoting_preserves_spaces() {
        let joined = join_windows_args(&[
            "--apply".into(),
            "--source".into(),
            r"C:\My Folder\MSI Toolkit".into(),
        ]);
        assert_eq!(joined, r#"--apply --source "C:\My Folder\MSI Toolkit""#);
    }

    #[test]
    fn probe_system_runs_safely_without_side_effects() {
        let probe = probe_system(None);
        assert!(probe.install_dir.is_absolute());
        assert!(validate_local_fixed_dir(Path::new(r"C:\Program Files\MSI-Toolkit")).is_ok());
        assert!(validate_local_fixed_dir(Path::new(r"C:\")).is_err());
        assert!(validate_local_fixed_dir(Path::new(r"\\server\share\MSI-Toolkit")).is_err());
    }
}
