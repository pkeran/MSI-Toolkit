//! Build script: compiles the modular Slint front-end with the mature Fluent 2
//! widget style from `std-widgets.slint`.
//!
//! Style choice is a *compile-time* decision (`fluent`); the dark/light colour
//! scheme is a *runtime* decision driven by `Palette.color-scheme`, so a single
//! binary follows the Windows 11 app theme without a second build.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let ui_dir = Path::new("ui");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/msilt.ico");

    // Watch every nested Slint file so incremental builds pick up edits to
    // components/pages without a `cargo clean`.
    rerun_on_slint_sources(ui_dir);

    let config = slint_build::CompilerConfiguration::new()
        // Mature Fluent 2 widget set. `fluent-dark` is the documented alias for
        // "fluent + dark colour scheme"; `Palette.color-scheme` still overrides
        // it at runtime, which is what the theme follower uses.
        .with_style("fluent".into())
        // Keep `@image-url` and `import` resolution relative to `ui/`.
        .with_include_paths(vec![ui_dir.to_path_buf()]);

    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile ui/app.slint");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

fn embed_windows_resources() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join("..").join("..").join("assets").join("msilt.ico");
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let mut parts = version.split('.');
    let maj: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let pat: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    let ico_line = if ico_path.is_file() {
        format!("1 ICON \"{}\"\n", ico_path.to_string_lossy().replace('\\', "\\\\"))
    } else {
        String::new()
    };
    let rc_content = format!(
        r#"#pragma code_page(65001)
{ico_line}1 VERSIONINFO
FILEVERSION {maj},{min},{pat},0
PRODUCTVERSION {maj},{min},{pat},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "MSI-Toolkit\0"
            VALUE "FileDescription", "MSI-Toolkit Control Center\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "msilt-ui\0"
            VALUE "OriginalFilename", "msilt-ui.exe\0"
            VALUE "ProductName", "MSI-Toolkit\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    );
    let rc_path = out_dir.join("msilt-ui.rc");
    let res_path = out_dir.join("msilt-ui.res");
    if fs::write(&rc_path, rc_content).is_err() {
        return;
    }
    if let Some(rc_exe) = find_rc_exe() {
        let status = Command::new(rc_exe)
            .args(["/nologo", "/fo"])
            .arg(&res_path)
            .arg(&rc_path)
            .status();
        if status.is_ok_and(|s| s.success()) && res_path.is_file() {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path) = env::var("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(kits) {
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("x64").join("rc.exe"))
            .filter(|p| p.is_file())
            .collect();
        versions.sort();
        return versions.pop();
    }
    None
}

fn rerun_on_slint_sources(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rerun_on_slint_sources(&path);
        } else if path.extension().is_some_and(|ext| ext == "slint") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
