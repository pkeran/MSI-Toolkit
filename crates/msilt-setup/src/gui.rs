use std::path::PathBuf;

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Shell::{
    CLSID_ProgressDialog, IProgressDialog, PROGDLG_NOCANCEL, PROGDLG_NOTIME,
};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::{BOOL, PCWSTR, w};

use crate::ops::{self, InstallOptions, UninstallOptions, wide};

pub enum GuiMode {
    Install {
        source_dir: Option<PathBuf>,
        auto_apply: Option<InstallOptions>,
    },
    Update {
        source_dir: Option<PathBuf>,
    },
    Uninstall {
        auto_apply: Option<UninstallOptions>,
    },
}

fn confirm(
    title: &str,
    text: &str,
    action: &str,
    checkbox: &str,
    checked: bool,
) -> Result<Option<bool>, String> {
    let (title, text, action, checkbox) = (wide(title), wide(text), wide(action), wide(checkbox));
    let button = TASKDIALOG_BUTTON {
        nButtonID: 100,
        pszButtonText: PCWSTR(action.as_ptr()),
    };
    let mut config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("MSI-Toolkit"),
        pszMainInstruction: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        pszVerificationText: PCWSTR(checkbox.as_ptr()),
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: TD_SHIELD_ICON,
        },
        cButtons: 1,
        pButtons: &button,
        nDefaultButton: 100,
        ..Default::default()
    };
    if checked {
        config.dwFlags = config.dwFlags | TDF_VERIFICATION_FLAG_CHECKED;
    }
    let mut selected = 0;
    let mut value = BOOL::default();
    unsafe { TaskDialogIndirect(&config, Some(&mut selected), None, Some(&mut value)) }
        .map_err(|e| e.to_string())?;
    Ok((selected == 100).then(|| value.as_bool()))
}

fn with_progress(
    work: impl FnOnce(&dyn Fn(u32, &str)) -> Result<(), String>,
) -> Result<(), String> {
    let dialog: IProgressDialog =
        unsafe { CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| e.to_string())?;
    unsafe {
        dialog
            .SetTitle(w!("MSI-Toolkit"))
            .map_err(|e| e.to_string())?;
        dialog
            .StartProgressDialog(None, None, PROGDLG_NOCANCEL | PROGDLG_NOTIME, None)
            .map_err(|e| e.to_string())?;
    }
    let result = work(&|pct, text| unsafe {
        let _ = dialog.SetLine(1, PCWSTR(wide(text).as_ptr()), false, None);
        let _ = dialog.SetProgress(pct, 100);
    });
    unsafe {
        let _ = dialog.StopProgressDialog();
    }
    result
}

fn run_mode(mode: GuiMode) -> Result<i32, String> {
    let (source, given, update) = match mode {
        GuiMode::Install {
            source_dir,
            auto_apply,
        } => (source_dir, auto_apply, false),
        GuiMode::Update { source_dir } => (source_dir, None, true),
        GuiMode::Uninstall { auto_apply } => {
            let opts = if let Some(opts) = auto_apply {
                opts
            } else {
                let Some(purge_config) = confirm(
                    "卸载 MSI-Toolkit？",
                    "将移除应用、快捷方式和后台任务。",
                    "卸载",
                    "同时删除个人设置和自动化规则",
                    false,
                )?
                else {
                    return Ok(0);
                };
                UninstallOptions { purge_config }
            };
            if !ops::is_elevated() {
                let mut args = vec!["--uninstall".into(), "--apply-uninstall".into()];
                if opts.purge_config {
                    args.push("--purge".into());
                }
                ops::elevate_detached(&args)?;
                return Ok(0);
            }
            return with_progress(|report| ops::run_uninstall(&opts, report)).map(|_| 0);
        }
    };
    let opts = if let Some(opts) = given {
        opts
    } else {
        let probe = ops::probe_system(source.as_deref());
        if !probe.has_payload {
            return Err("请运行完整的 MSI-Toolkit-Setup.exe 安装包。".into());
        }
        let auto_start = if update {
            probe.auto_start_enabled
        } else {
            let action = if probe.installed { "更新" } else { "安装" };
            let mut details = format!("安装位置：{}", probe.install_dir.display());
            if let Some(version) = probe.installed_version {
                details.push_str(&format!("\n已安装版本：{version}"));
            }
            let Some(value) = confirm(
                &format!("{action} MSI-Toolkit {}", ops::APP_VERSION),
                &details,
                action,
                "登录 Windows 时启动后台",
                probe.auto_start_enabled,
            )?
            else {
                return Ok(0);
            };
            value
        };
        InstallOptions {
            auto_start,
            start_menu: probe.start_menu_exists,
            desktop_shortcut: probe.desktop_shortcut_exists,
            launch_ui: true,
            source_dir: source,
            from_update: update,
        }
    };
    if !ops::is_elevated() {
        let mut args = vec![
            "--apply".into(),
            format!("--autostart={}", u8::from(opts.auto_start)),
            format!("--startmenu={}", u8::from(opts.start_menu)),
            format!("--desktop={}", u8::from(opts.desktop_shortcut)),
            format!("--launch={}", u8::from(opts.launch_ui)),
        ];
        if opts.from_update {
            args.push("--from-update".into());
        }
        if let Some(source) = &opts.source_dir {
            args.extend(["--source".into(), source.to_string_lossy().into_owned()]);
        }
        return ops::elevate_and_wait(&args);
    }
    with_progress(|report| ops::run_install(&opts, report)).map(|_| 0)
}

pub fn run(mode: GuiMode) -> i32 {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let result = hr
        .ok()
        .map_err(|e| e.to_string())
        .and_then(|_| run_mode(mode));
    if hr.is_ok() {
        unsafe {
            CoUninitialize();
        }
    }
    result.unwrap_or_else(|msg| {
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(wide(msg).as_ptr()),
                w!("MSI-Toolkit"),
                MB_OK | MB_ICONERROR,
            );
        }
        1
    })
}
