//! MSI-Toolkit single-file native installer, updater, and uninstaller.

#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

#[cfg(windows)]
mod archive;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod ops;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::path::PathBuf;
    use std::process::ExitCode;

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.first().is_some_and(|a| a.starts_with("--wizard-")) {
        return match ops::wizard_action(&raw_args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("msilt-setup: {err}");
                ExitCode::FAILURE
            }
        };
    }

    // Build-time packer: msilt-setup.exe --pack <source_dir> <output_exe>
    if raw_args.first().map(String::as_str) == Some("--pack") {
        if raw_args.len() != 3 {
            eprintln!("usage: msilt-setup --pack <source_dir> <output_exe>");
            return ExitCode::from(2);
        }
        let stub = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("msilt-setup: cannot locate self: {e}");
                return ExitCode::FAILURE;
            }
        };
        let src = PathBuf::from(&raw_args[1]);
        let out = PathBuf::from(&raw_args[2]);
        match archive::pack_installer(&stub, &src, &out) {
            Ok((unpacked, final_len)) => {
                println!(
                    "Packed {} ({:.2} MiB unpacked -> {:.2} MiB single-file setup)",
                    out.display(),
                    unpacked as f64 / (1024.0 * 1024.0),
                    final_len as f64 / (1024.0 * 1024.0),
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("msilt-setup: pack failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Non-destructive verification probe for scripts/verify-all.ps1
    if raw_args.first().map(String::as_str) == Some("--self-test") {
        let probe = ops::probe_system(None);
        println!(
            "msilt-setup v{} ok (target={}, has_payload={})",
            ops::APP_VERSION,
            probe.install_dir.display(),
            probe.has_payload
        );
        return ExitCode::SUCCESS;
    }

    let mut uninstall = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .is_some_and(|n| n.contains("uninstall"));
    let mut update = false;
    let mut apply = false;
    let mut apply_uninstall = false;
    let mut quiet = false;
    let mut purge = false;
    let mut from_update = false;
    let mut auto_start = true;
    let mut start_menu = true;
    let mut desktop = false;
    let mut launch = true;
    let mut source_dir: Option<PathBuf> = None;

    let mut i = 0;
    while i < raw_args.len() {
        let a = raw_args[i].as_str();
        match a {
            "--uninstall" => uninstall = true,
            "--update" => update = true,
            "--apply" => apply = true,
            "--apply-uninstall" => apply_uninstall = true,
            "--quiet" | "/S" | "-q" => quiet = true,
            "--purge" => purge = true,
            "--from-update" => from_update = true,
            "--autostart=0" => auto_start = false,
            "--autostart=1" => auto_start = true,
            "--startmenu=0" => start_menu = false,
            "--startmenu=1" => start_menu = true,
            "--desktop=0" => desktop = false,
            "--desktop=1" => desktop = true,
            "--launch=0" => launch = false,
            "--launch=1" => launch = true,
            "--source" => {
                if let Some(val) = raw_args.get(i + 1) {
                    source_dir = Some(PathBuf::from(val));
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if uninstall && quiet {
        if !ops::is_elevated() {
            let mut forwarded = vec!["--uninstall".into(), "--quiet".into()];
            if purge {
                forwarded.push("--purge".into());
            }
            return match ops::elevate_and_wait(&forwarded) {
                Ok(0) => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            };
        }
        return match ops::run_uninstall(&ops::UninstallOptions { purge_config: purge }, |_, _| {})
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }

    let mode = if uninstall {
        gui::GuiMode::Uninstall {
            auto_apply: apply_uninstall.then_some(ops::UninstallOptions {
                purge_config: purge,
            }),
        }
    } else if update {
        gui::GuiMode::Update { source_dir }
    } else {
        gui::GuiMode::Install {
            source_dir: source_dir.clone(),
            auto_apply: apply.then_some(ops::InstallOptions {
                auto_start,
                start_menu,
                desktop_shortcut: desktop,
                launch_ui: launch,
                source_dir,
                from_update,
            }),
        }
    };

    let code = gui::run(mode);
    ExitCode::from(if code == 0 { 0 } else { 1 })
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("msilt-setup requires Windows");
    std::process::ExitCode::FAILURE
}
