//! Embeds the MSI-Toolkit Windows icon and VERSIONINFO into `msilt-daemon.exe`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/msilt.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join("..").join("..").join("assets").join("msilt.ico");
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let (maj, min, pat) = parse_ver(&version);

    let ico_line = if ico_path.is_file() {
        format!("1 ICON \"{}\"\n", escape_rc(&ico_path))
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
            VALUE "FileDescription", "MSI-Toolkit Hardware Daemon\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "msilt-daemon\0"
            VALUE "OriginalFilename", "msilt-daemon.exe\0"
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

    let rc_path = out_dir.join("msilt-daemon.rc");
    let res_path = out_dir.join("msilt-daemon.res");
    fs::write(&rc_path, rc_content).expect("write rc");

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

fn parse_ver(v: &str) -> (u16, u16, u16) {
    let mut parts = v.split('.');
    let a = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let b = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let c = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (a, b, c)
}

fn escape_rc(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
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
