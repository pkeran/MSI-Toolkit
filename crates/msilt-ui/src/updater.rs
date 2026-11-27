//! Lightweight, zero-bloat GitHub Releases update checker, SHA-256 verified
//! self-updater, and local app-maintenance helpers for MSI-Toolkit.
//!
//! Design invariants:
//!   * Runs strictly inside the Medium-integrity `msilt-ui` process — the
//!     elevated `msilt-daemon` stays 100% network-free.
//!   * Zero external HTTP/TLS/crypto crates: uses native Windows `winhttp.dll`
//!     (TLS 1.3 + automatic system proxy) and `bcrypt.dll` (CNG SHA-256).
//!   * Fail-closed security:
//!       - HTTPS only; redirects are manually verified against official GitHub
//!         hosts (`api.github.com`, `github.com`, `*.githubusercontent.com`).
//!       - `SHA256SUMS.txt` must be present on the same official release and the
//!         downloaded installer (`MSI-Toolkit-Setup.exe`, or fallback
//!         `MSI-Toolkit-win64.zip`) must match byte-for-byte before anything is
//!         written or launched.

#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};

pub const GITHUB_OWNER: &str = "pkeran";
pub const GITHUB_REPO: &str = "MSI-Toolkit";
pub const RELEASES_API_URL: &str = "https://api.github.com/repos/pkeran/MSI-Toolkit/releases/latest";
pub const RELEASES_PAGE_URL: &str = "https://github.com/pkeran/MSI-Toolkit/releases";

const SETUP_ASSET: &str = "MSI-Toolkit-Setup.exe";
const BUNDLE_ASSET: &str = "MSI-Toolkit-win64.zip";
const CHECKSUM_ASSET: &str = "SHA256SUMS.txt";
const MAX_META_BYTES: usize = 512 * 1024;
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    SetupExe,
    ZipBundle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub html_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_kind: AssetKind,
    pub sha256_url: String,
    pub published_at: String,
    pub notes_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate { current: String, latest: String },
    Available(ReleaseInfo),
    NoReleaseYet { current: String },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Launched { tag: String },
    Failed(String),
}

/// Parses `v1.2.3`, `1.2.3`, or `v1.2.3-rc1` into `(major, minor, patch)`.
pub fn parse_semver(raw: &str) -> Option<(u32, u32, u32)> {
    let trimmed = raw.trim().strip_prefix(['v', 'V']).unwrap_or(raw.trim());
    let core = trimmed.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

pub fn is_allowed_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    matches!(
        h.as_str(),
        "api.github.com"
            | "github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

pub fn is_allowed_release_url(url: &str) -> bool {
    if url.bytes().any(|b| b < 0x21 || matches!(b, b'"' | b'\'' | b'`' | b'$' | b'\\')) {
        return false;
    }
    let expected_repo_prefix = format!("https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/");
    url.starts_with(&expected_repo_prefix)
        || url.starts_with("https://objects.githubusercontent.com/")
        || url.starts_with("https://release-assets.githubusercontent.com/")
        || url.starts_with("https://github-releases.githubusercontent.com/")
}

fn summarize_notes(raw: &str) -> String {
    for line in raw.lines() {
        let cleaned = line
            .trim()
            .trim_start_matches(['#', '-', '*', '>', ' '])
            .trim();
        if !cleaned.is_empty() {
            let mut out: String = cleaned.chars().take(68).collect();
            if cleaned.chars().count() > 68 {
                out.push('…');
            }
            return out;
        }
    }
    String::new()
}

pub fn parse_release_json(current_version: &str, body: &[u8]) -> CheckOutcome {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return CheckOutcome::Failed(format!("发行版元数据解析失败：{e}")),
    };
    if value.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
        || value.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        return CheckOutcome::UpToDate {
            current: current_version.to_string(),
            latest: current_version.to_string(),
        };
    }
    let Some(tag) = value.get("tag_name").and_then(|v| v.as_str()).map(str::trim) else {
        return CheckOutcome::Failed("发行版缺少版本号字段".into());
    };
    let Some((maj, min, pat)) = parse_semver(tag) else {
        return CheckOutcome::Failed(format!("无法识别的版本号：{tag}"));
    };
    let normalized = format!("{maj}.{min}.{pat}");
    if !is_newer(&normalized, current_version) {
        return CheckOutcome::UpToDate {
            current: current_version.to_string(),
            latest: normalized,
        };
    }

    let html_url = value
        .get("html_url")
        .and_then(|v| v.as_str())
        .filter(|u| is_allowed_release_url(u))
        .unwrap_or(RELEASES_PAGE_URL)
        .to_string();
    let published_at = value
        .get("published_at")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let notes_summary = value
        .get("body")
        .and_then(|v| v.as_str())
        .map(summarize_notes)
        .unwrap_or_default();

    let mut setup_url = None;
    let mut zip_url = None;
    let mut sha256_url = None;
    if let Some(assets) = value.get("assets").and_then(|v| v.as_array()) {
        for asset in assets {
            let Some(name) = asset.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(url) = asset.get("browser_download_url").and_then(|v| v.as_str()) else {
                continue;
            };
            if !is_allowed_release_url(url) {
                continue;
            }
            if name.eq_ignore_ascii_case(SETUP_ASSET) {
                setup_url = Some(url.to_string());
            } else if name.eq_ignore_ascii_case(BUNDLE_ASSET) {
                zip_url = Some(url.to_string());
            } else if name.eq_ignore_ascii_case(CHECKSUM_ASSET) {
                sha256_url = Some(url.to_string());
            }
        }
    }

    let selected = if let Some(url) = setup_url {
        Some((SETUP_ASSET.to_string(), url, AssetKind::SetupExe))
    } else {
        zip_url.map(|url| (BUNDLE_ASSET.to_string(), url, AssetKind::ZipBundle))
    };

    match (selected, sha256_url) {
        (Some((asset_name, asset_url, asset_kind)), Some(sha256_url)) => {
            CheckOutcome::Available(ReleaseInfo {
                tag: tag.to_string(),
                version: normalized,
                html_url,
                asset_name,
                asset_url,
                asset_kind,
                sha256_url,
                published_at,
                notes_summary,
            })
        }
        _ => CheckOutcome::Failed(format!(
            "发现新版本 {tag}，但尚未上传完整的校验安装包，请稍后再试"
        )),
    }
}

pub fn parse_sha256_for_asset(text: &str, asset_name: &str) -> Option<String> {
    for raw_line in text.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut iter = line.split_whitespace();
        let Some(hash) = iter.next() else { continue };
        let Some(file) = iter.next() else { continue };
        let file = file.trim_start_matches('*');
        if file.eq_ignore_ascii_case(asset_name)
            && hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn check_latest(current_version: &str) -> CheckOutcome {
    match http_get_https(RELEASES_API_URL, MAX_META_BYTES, true, |_, _| {}) {
        Ok((200, body)) => parse_release_json(current_version, &body),
        Ok((404, _)) => CheckOutcome::NoReleaseYet {
            current: current_version.to_string(),
        },
        Ok((403 | 429, _)) => {
            let url = format!("{RELEASES_PAGE_URL}/latest/download/{CHECKSUM_ASSET}");
            match http_get_https(&url, MAX_META_BYTES, false, |_, _| {}) {
                Ok((200, sums)) => parse_release_checksums(current_version, &sums),
                Ok((status, _)) => CheckOutcome::Failed(format!("无法读取公开发行版校验清单（HTTP {status}）")),
                Err(err) => CheckOutcome::Failed(err),
            }
        }
        Ok((status, _)) => CheckOutcome::Failed(format!("GitHub 返回异常状态码 HTTP {status}")),
        Err(err) => CheckOutcome::Failed(err),
    }
}

fn parse_release_checksums(current: &str, body: &[u8]) -> CheckOutcome {
    let text = std::str::from_utf8(body).unwrap_or_default();
    let tag = text.lines().next().and_then(|s| s.strip_prefix("# MSI-Toolkit ")).unwrap_or_default();
    let Some((major, minor, patch)) = parse_semver(tag) else {
        return CheckOutcome::Failed("发行版校验清单缺少版本号".into());
    };
    let version = format!("{major}.{minor}.{patch}");
    if tag != format!("v{version}") || parse_sha256_for_asset(text, SETUP_ASSET).is_none() {
        return CheckOutcome::Failed("发行版校验清单无效".into());
    }
    if !is_newer(&version, current) {
        return CheckOutcome::UpToDate { current: current.into(), latest: version };
    }
    // Pin both downloads to this tag; latest may change between check and install.
    let base = format!("{RELEASES_PAGE_URL}/download/{tag}");
    CheckOutcome::Available(ReleaseInfo {
        tag: tag.into(), version, html_url: format!("{RELEASES_PAGE_URL}/tag/{tag}"),
        asset_name: SETUP_ASSET.into(), asset_url: format!("{base}/{SETUP_ASSET}"),
        asset_kind: AssetKind::SetupExe, sha256_url: format!("{base}/{CHECKSUM_ASSET}"),
        published_at: String::new(), notes_summary: String::new(),
    })
}

pub fn download_and_launch_update(
    info: &ReleaseInfo,
    mut on_progress: impl FnMut(&str),
) -> InstallOutcome {
    if !is_allowed_release_url(&info.asset_url) || !is_allowed_release_url(&info.sha256_url) {
        return InstallOutcome::Failed("更新地址未通过官方仓库域名校验".into());
    }

    on_progress(&format!("正在获取 {} 校验文件…", info.tag));
    let (sum_status, sum_bytes) =
        match http_get_https(&info.sha256_url, MAX_META_BYTES, false, |_, _| {}) {
            Ok(res) => res,
            Err(e) => return InstallOutcome::Failed(format!("下载校验文件失败：{e}")),
        };
    if sum_status != 200 {
        return InstallOutcome::Failed(format!("校验文件下载失败（HTTP {sum_status}）"));
    }
    let sum_text = String::from_utf8_lossy(&sum_bytes);
    let Some(expected_hash) = parse_sha256_for_asset(&sum_text, &info.asset_name) else {
        return InstallOutcome::Failed(format!(
            "SHA256SUMS.txt 中未找到 {} 的合法校验值",
            info.asset_name
        ));
    };

    on_progress(&format!("正在下载 {}…", info.tag));
    let mut last_pct: u32 = u32::MAX;
    let (dl_status, dl_bytes) = match http_get_https(
        &info.asset_url,
        MAX_DOWNLOAD_BYTES,
        false,
        |got, total| {
            if let Some(total) = total.filter(|&t| t > 0) {
                let pct = ((got.saturating_mul(100)) / total).min(100) as u32;
                if pct != last_pct && (pct == 100 || pct.saturating_sub(last_pct) >= 5 || last_pct == u32::MAX) {
                    last_pct = pct;
                    on_progress(&format!("正在下载 {}（{pct}%）…", info.tag));
                }
            }
        },
    ) {
        Ok(res) => res,
        Err(e) => return InstallOutcome::Failed(format!("下载更新包失败：{e}")),
    };
    if dl_status != 200 {
        return InstallOutcome::Failed(format!("更新包下载失败（HTTP {dl_status}）"));
    }

    match info.asset_kind {
        AssetKind::SetupExe => {
            if dl_bytes.len() < 4 || &dl_bytes[..2] != b"MZ" {
                return InstallOutcome::Failed("下载的安装程序不是有效的 Windows 可执行文件".into());
            }
        }
        AssetKind::ZipBundle => {
            if dl_bytes.len() < 4 || &dl_bytes[..2] != b"PK" {
                return InstallOutcome::Failed("下载的更新包不是有效的 ZIP 压缩包".into());
            }
        }
    }

    on_progress(&format!("正在校验 {} SHA-256 完整性…", info.tag));
    let actual_hash = match sha256_hex(&dl_bytes) {
        Ok(h) => h,
        Err(e) => return InstallOutcome::Failed(format!("SHA-256 计算失败：{e}")),
    };
    if actual_hash != expected_hash {
        return InstallOutcome::Failed("安装包 SHA-256 校验不匹配，已中止更新".into());
    }

    let safe_tag: String = info
        .tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let stage_dir: PathBuf = std::env::temp_dir().join(format!(
        "MSI-Toolkit-Update-{}",
        if safe_tag.is_empty() { "latest" } else { &safe_tag }
    ));
    let _ = fs::remove_dir_all(&stage_dir);
    if let Err(e) = fs::create_dir_all(&stage_dir) {
        return InstallOutcome::Failed(format!("无法创建临时升级目录：{e}"));
    }

    match info.asset_kind {
        AssetKind::SetupExe => {
            let setup_path = stage_dir.join(SETUP_ASSET);
            if let Err(e) = fs::write(&setup_path, &dl_bytes) {
                let _ = fs::remove_dir_all(&stage_dir);
                return InstallOutcome::Failed(format!("写入升级程序失败：{e}"));
            }
            on_progress(&format!("{} 校验通过，正在启动升级程序…", info.tag));
            match open_executable(&setup_path, "/SILENT /NORESTART /MSILTUPDATE --update") {
                Ok(_) => InstallOutcome::Launched {
                    tag: info.tag.clone(),
                },
                Err(e) => {
                    let _ = fs::remove_dir_all(&stage_dir);
                    InstallOutcome::Failed(format!("无法启动升级程序：{e}"))
                }
            }
        }
        AssetKind::ZipBundle => {
            let zip_path = stage_dir.join(BUNDLE_ASSET);
            if let Err(e) = fs::write(&zip_path, &dl_bytes) {
                let _ = fs::remove_dir_all(&stage_dir);
                return InstallOutcome::Failed(format!("写入更新包失败：{e}"));
            }
            let helper_script = stage_dir.join("apply-update.ps1");
            let script_body = r#"#Requires -Version 5.1
$ErrorActionPreference = 'Stop'
$stage = $PSScriptRoot
$zip = Join-Path $stage 'MSI-Toolkit-win64.zip'
$extract = Join-Path $stage 'extracted'
try {
    if (Test-Path -LiteralPath $extract) { Remove-Item -LiteralPath $extract -Recurse -Force }
    Expand-Archive -LiteralPath $zip -DestinationPath $extract -Force
    $installer = Join-Path $extract 'install.ps1'
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw 'Updated archive is missing install.ps1'
    }
    & $installer -SourceDir $extract -PreserveAutoStart
} finally {
    Start-Sleep -Seconds 2
    Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
}
"#;
            if let Err(e) = fs::write(&helper_script, script_body) {
                let _ = fs::remove_dir_all(&stage_dir);
                return InstallOutcome::Failed(format!("写入升级脚本失败：{e}"));
            }
            on_progress(&format!("{} 校验通过，正在启动升级脚本…", info.tag));
            match Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-WindowStyle",
                    "Hidden",
                    "-File",
                ])
                .arg(&helper_script)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
            {
                Ok(_) => InstallOutcome::Launched {
                    tag: info.tag.clone(),
                },
                Err(e) => {
                    let _ = fs::remove_dir_all(&stage_dir);
                    InstallOutcome::Failed(format!("无法启动升级程序：{e}"))
                }
            }
        }
    }
}

pub fn open_release_page(url: &str) {
    let target = if is_allowed_release_url(url) {
        url
    } else {
        RELEASES_PAGE_URL
    };
    shell_open(target);
}

// ── Local installation & maintenance helpers ────────────────────────────────

pub fn installed_dir() -> PathBuf {
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
        if let Some(raw) = read_hklm_string(subkey, value) {
            let trimmed = raw.trim().trim_end_matches(['\\', '/']);
            if !trimmed.is_empty() {
                let candidate = PathBuf::from(trimmed);
                if candidate.is_absolute() && candidate.join("msilt-daemon.exe").is_file() {
                    return candidate;
                }
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("msilt-daemon.exe").is_file()
                && (dir.join("unins000.exe").is_file() || dir.join("uninstall.exe").is_file())
            {
                return dir.to_path_buf();
            }
        }
    }
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("MSI-Toolkit")
}

fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
    const HKEY_LOCAL_MACHINE: isize = -2147483646; // 0x80000002
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    let subkey_w = wide_z(subkey);
    let value_w = wide_z(value);
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut bytes,
        )
    };
    if status != 0 || !(2..=65_536).contains(&bytes) {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    (!text.is_empty()).then_some(text)
}

pub fn is_installed() -> bool {
    installed_dir().join("msilt-daemon.exe").is_file()
}

pub fn open_config_dir() -> Result<(), String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "无法定位本地应用数据目录".to_string())?;
    let dir = base.join("MSI-Toolkit");
    let _ = fs::create_dir_all(&dir);
    shell_open_path(&dir);
    Ok(())
}

pub fn launch_uninstaller() -> Result<(), String> {
    let dir = installed_dir();
    let wizard = dir.join("unins000.exe");
    if wizard.is_file() { return open_executable(&wizard, ""); }
    let native = dir.join("uninstall.exe");
    if native.is_file() {
        Command::new(&native)
            .arg("--uninstall")
            .spawn()
            .map_err(|e| format!("无法启动卸载程序：{e}"))?;
        return Ok(());
    }
    let legacy = dir.join("uninstall.cmd");
    if legacy.is_file() {
        shell_open_path(&legacy);
        return Ok(());
    }
    Err("未找到已安装的卸载程序（便携版直接删除目录即可）。".into())
}

fn open_executable(path: &Path, params: &str) -> Result<(), String> {
    let result = unsafe { ShellExecuteW(null_mut(), null(), wide(path).as_ptr(), wide_z(params).as_ptr(), null(), 1) };
    if result > 32 { Ok(()) } else { Err(format!("Windows 无法启动安装程序（{result}）")) }
}

/// Triggers the pre-authorized `\MSI-Toolkit\Daemon` scheduled task silently
/// (zero UAC prompt, because the task DACL grants `GRGX` to the current user).
pub fn try_start_daemon_silently() -> bool {
    let schtasks = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join(r"System32\schtasks.exe"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\schtasks.exe"));
    Command::new(schtasks)
        .args(["/Run", "/TN", r"\MSI-Toolkit\Daemon"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

pub fn start_installed_daemon() -> Result<(), String> {
    let dir = installed_dir();
    let daemon = dir.join("msilt-daemon.exe");
    if !daemon.is_file() {
        return Err("尚未完成安装，请先运行 MSI-Toolkit-Setup.exe 安装。".into());
    }
    if try_start_daemon_silently() {
        return Ok(());
    }
    let verb = wide_z("runas");
    let file = wide(&daemon);
    let params = wide_z("--acpi --allow-platform-control --enable-ec-writes");
    let workdir = wide(&dir);
    let res = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            workdir.as_ptr(),
            0, // SW_HIDE
        )
    };
    if res > 32 {
        Ok(())
    } else {
        Err("管理员授权已取消或后台服务启动失败。".into())
    }
}

fn shell_open(target: &str) {
    let wide = wide_z(target);
    unsafe {
        ShellExecuteW(null_mut(), null(), wide.as_ptr(), null(), null(), 1);
    }
}

fn shell_open_path(path: &Path) {
    let wide = wide_os(path.as_os_str());
    unsafe {
        ShellExecuteW(null_mut(), null(), wide.as_ptr(), null(), null(), 1);
    }
}

// ── Native Windows CNG SHA-256 (`bcrypt.dll`) ───────────────────────────────

pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut alg: *mut c_void = null_mut();
    let alg_name = wide_z("SHA256");
    let st = unsafe { BCryptOpenAlgorithmProvider(&mut alg, alg_name.as_ptr(), null(), 0) };
    if st < 0 {
        return Err(format!("BCryptOpenAlgorithmProvider failed: 0x{:08X}", st as u32));
    }
    struct AlgGuard(*mut c_void);
    impl Drop for AlgGuard {
        fn drop(&mut self) {
            unsafe {
                BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
    let _alg_guard = AlgGuard(alg);

    let mut hash: *mut c_void = null_mut();
    let st = unsafe { BCryptCreateHash(alg, &mut hash, null_mut(), 0, null(), 0, 0) };
    if st < 0 {
        return Err(format!("BCryptCreateHash failed: 0x{:08X}", st as u32));
    }
    struct HashGuard(*mut c_void);
    impl Drop for HashGuard {
        fn drop(&mut self) {
            unsafe {
                BCryptDestroyHash(self.0);
            }
        }
    }
    let _hash_guard = HashGuard(hash);

    for chunk in data.chunks(1024 * 1024) {
        let st = unsafe { BCryptHashData(hash, chunk.as_ptr(), chunk.len() as u32, 0) };
        if st < 0 {
            return Err(format!("BCryptHashData failed: 0x{:08X}", st as u32));
        }
    }

    let mut digest = [0u8; 32];
    let st = unsafe { BCryptFinishHash(hash, digest.as_mut_ptr(), digest.len() as u32, 0) };
    if st < 0 {
        return Err(format!("BCryptFinishHash failed: 0x{:08X}", st as u32));
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    Ok(out)
}

// ── Native WinHTTP HTTPS client (`winhttp.dll`) ─────────────────────────────

struct WinHttpHandle(*mut c_void);
impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn parse_https_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "仅允许 HTTPS 安全连接".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err("无效的目标主机".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "无效的端口".to_string())?,
        ),
        None => (authority.to_string(), 443u16),
    };
    if !is_allowed_host(&host) {
        return Err(format!("已拦截非官方主机：{host}"));
    }
    Ok((host, port, path.to_string()))
}

fn http_get_https(
    start_url: &str,
    max_bytes: usize,
    github_api: bool,
    mut on_chunk: impl FnMut(u64, Option<u64>),
) -> Result<(u32, Vec<u8>), String> {
    let ua = wide_z(&format!("MSI-Toolkit/{}", env!("CARGO_PKG_VERSION")));
    let session = unsafe {
        WinHttpOpen(
            ua.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        )
    };
    if session.is_null() {
        return Err("无法初始化系统网络组件 WinHTTP".into());
    }
    let session = WinHttpHandle(session);
    unsafe {
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 8_000, 25_000);
    }

    let mut current_url = start_url.to_string();
    for _hop in 0..=MAX_REDIRECTS {
        let (host, port, path) = parse_https_url(&current_url)?;
        let host_w = wide_z(&host);
        let conn = unsafe { WinHttpConnect(session.0, host_w.as_ptr(), port, 0) };
        if conn.is_null() {
            return Err("无法连接到 GitHub 服务器".into());
        }
        let conn = WinHttpHandle(conn);

        let verb_w = wide_z("GET");
        let path_w = wide_z(&path);
        let req = unsafe {
            WinHttpOpenRequest(
                conn.0,
                verb_w.as_ptr(),
                path_w.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            )
        };
        if req.is_null() {
            return Err("无法创建 HTTPS 请求".into());
        }
        let req = WinHttpHandle(req);

        // Disable automatic redirects so we explicitly validate every target host.
        let policy: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        unsafe {
            WinHttpSetOption(
                req.0,
                WINHTTP_OPTION_REDIRECT_POLICY,
                (&policy as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            );
        }

        let headers_text = if github_api && host.eq_ignore_ascii_case("api.github.com") {
            "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
        } else {
            "Accept: application/octet-stream, text/plain, */*\r\n"
        };
        let headers_w = wide_z(headers_text);
        let sent = unsafe {
            WinHttpSendRequest(
                req.0,
                headers_w.as_ptr(),
                u32::MAX,
                null(),
                0,
                0,
                0,
            )
        };
        if sent == 0 {
            return Err("连接 GitHub 超时或网络不可用".into());
        }
        if unsafe { WinHttpReceiveResponse(req.0, null_mut()) } == 0 {
            return Err("等待 GitHub 响应失败".into());
        }

        let status = query_status_code(req.0)?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = query_location_header(req.0)
                .ok_or_else(|| "重定向响应缺少目标地址".to_string())?;
            current_url = location;
            continue;
        }

        let content_len = query_content_length(req.0);
        let mut body = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let mut avail: u32 = 0;
            if unsafe { WinHttpQueryDataAvailable(req.0, &mut avail) } == 0 {
                return Err("读取响应流失败".into());
            }
            if avail == 0 {
                break;
            }
            let want = (avail as usize).min(chunk.len());
            let mut read_now: u32 = 0;
            if unsafe {
                WinHttpReadData(req.0, chunk.as_mut_ptr().cast(), want as u32, &mut read_now)
            } == 0
            {
                return Err("下载响应数据时中断".into());
            }
            if read_now == 0 {
                break;
            }
            if body.len().saturating_add(read_now as usize) > max_bytes {
                return Err("响应体积超过安全上限".into());
            }
            body.extend_from_slice(&chunk[..read_now as usize]);
            on_chunk(body.len() as u64, content_len);
        }
        return Ok((status, body));
    }

    Err("重定向次数过多".into())
}

fn query_status_code(req: *mut c_void) -> Result<u32, String> {
    let mut status: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err("无法读取 HTTP 状态码".into());
    }
    Ok(status)
}

fn query_content_length(req: *mut c_void) -> Option<u64> {
    let mut len: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut len as *mut u32).cast(),
            &mut size,
            null_mut(),
        )
    };
    (ok != 0 && len > 0).then_some(u64::from(len))
}

fn query_location_header(req: *mut c_void) -> Option<String> {
    let mut bytes: u32 = 0;
    unsafe {
        WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_LOCATION,
            null(),
            null_mut(),
            &mut bytes,
            null_mut(),
        );
    }
    if bytes == 0 || bytes > 16 * 1024 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize / 2) + 1];
    let ok = unsafe {
        WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_LOCATION,
            null(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    let len = (bytes as usize / 2).min(buf.len());
    String::from_utf16(&buf[..len])
        .ok()
        .map(|s| s.trim_end_matches('\0').trim().to_string())
}

fn wide_z(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

fn wide(path: &Path) -> Vec<u16> {
    wide_os(path.as_os_str())
}

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_OPTION_REDIRECT_POLICY: u32 = 88;
const WINHTTP_OPTION_REDIRECT_POLICY_NEVER: u32 = 0;
const WINHTTP_QUERY_CONTENT_LENGTH: u32 = 5;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_LOCATION: u32 = 33;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access_type: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetTimeouts(
        handle: *mut c_void,
        resolve: i32,
        connect: i32,
        send: i32,
        receive: i32,
    ) -> i32;
    fn WinHttpConnect(
        session: *mut c_void,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> *mut c_void;
    fn WinHttpOpenRequest(
        connect: *mut c_void,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetOption(
        handle: *mut c_void,
        option: u32,
        buffer: *const c_void,
        len: u32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: *mut c_void,
        headers: *const u16,
        headers_len: u32,
        optional: *const c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: *mut c_void,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpQueryDataAvailable(request: *mut c_void, avail: *mut u32) -> i32;
    fn WinHttpReadData(
        request: *mut c_void,
        buffer: *mut c_void,
        to_read: u32,
        read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
}

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptOpenAlgorithmProvider(
        alg: *mut *mut c_void,
        alg_id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(alg: *mut c_void, flags: u32) -> i32;
    fn BCryptCreateHash(
        alg: *mut c_void,
        hash: *mut *mut c_void,
        obj: *mut u8,
        obj_len: u32,
        secret: *const u8,
        secret_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptHashData(hash: *mut c_void, input: *const u8, input_len: u32, flags: u32) -> i32;
    fn BCryptFinishHash(hash: *mut c_void, output: *mut u8, output_len: u32, flags: u32) -> i32;
    fn BCryptDestroyHash(hash: *mut c_void) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        hkey: isize,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        reg_type: *mut u32,
        data: *mut c_void,
        data_len: *mut u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing_and_comparison() {
        assert_eq!(parse_semver("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("1.12.3-beta.1"), Some((1, 12, 3)));
        assert_eq!(parse_semver("invalid"), None);
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }

    #[test]
    fn public_checksums_pin_a_stable_release_when_the_api_is_limited() {
        let digest = "0".repeat(64);
        let sums = format!("# MSI-Toolkit v0.1.1\n{digest}  {SETUP_ASSET}\n");
        let CheckOutcome::Available(info) = parse_release_checksums("0.1.0", sums.as_bytes()) else { panic!("missing update") };
        assert_eq!(info.asset_url, format!("{RELEASES_PAGE_URL}/download/v0.1.1/{SETUP_ASSET}"));
        assert_eq!(info.sha256_url, format!("{RELEASES_PAGE_URL}/download/v0.1.1/{CHECKSUM_ASSET}"));
        assert!(matches!(parse_release_checksums("0.1.1", sums.as_bytes()), CheckOutcome::UpToDate { .. }));
        for bad in [sums.replace("v0.1.1", "v0.1.1-rc1"), sums.replace("v0.1.1", "v0.1.1/../"),
                    sums.replace(&digest, "invalid"), sums.lines().skip(1).collect::<String>()] {
            assert!(matches!(parse_release_checksums("0.1.0", bad.as_bytes()), CheckOutcome::Failed(_)));
        }
    }

    #[test]
    fn sha256_native_cng_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b"").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn checksum_parser_and_url_allowlist_enforce_strict_bounds() {
        let txt = "\u{feff}BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  MSI-Toolkit-Setup.exe\n";
        assert_eq!(
            parse_sha256_for_asset(txt, SETUP_ASSET).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(is_allowed_release_url(
            "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/MSI-Toolkit-Setup.exe"
        ));
        assert!(!is_allowed_release_url(
            "https://evil.example.com/MSI-Toolkit-Setup.exe"
        ));
        assert!(!is_allowed_release_url(
            "http://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/MSI-Toolkit-Setup.exe"
        ));
    }

    #[test]
    fn release_json_prefers_setup_exe_and_falls_back_to_zip() {
        let json_both = br###"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/pkeran/MSI-Toolkit/releases/tag/v0.2.0",
            "published_at": "2026-11-26T10:00:00Z",
            "body": "## What's New\n- Single-file native installer",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "MSI-Toolkit-win64.zip",
                    "browser_download_url": "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/MSI-Toolkit-win64.zip"
                },
                {
                    "name": "MSI-Toolkit-Setup.exe",
                    "browser_download_url": "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/MSI-Toolkit-Setup.exe"
                },
                {
                    "name": "SHA256SUMS.txt",
                    "browser_download_url": "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/SHA256SUMS.txt"
                }
            ]
        }"###;
        match parse_release_json("0.1.0", json_both) {
            CheckOutcome::Available(info) => {
                assert_eq!(info.tag, "v0.2.0");
                assert_eq!(info.asset_name, SETUP_ASSET);
                assert_eq!(info.asset_kind, AssetKind::SetupExe);
                assert_eq!(info.notes_summary, "What's New");
            }
            other => panic!("expected Available(SetupExe), got {other:?}"),
        }

        let json_zip_only = br#"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/pkeran/MSI-Toolkit/releases/tag/v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "MSI-Toolkit-win64.zip",
                    "browser_download_url": "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/MSI-Toolkit-win64.zip"
                },
                {
                    "name": "SHA256SUMS.txt",
                    "browser_download_url": "https://github.com/pkeran/MSI-Toolkit/releases/download/v0.2.0/SHA256SUMS.txt"
                }
            ]
        }"#;
        match parse_release_json("0.1.0", json_zip_only) {
            CheckOutcome::Available(info) => {
                assert_eq!(info.asset_name, BUNDLE_ASSET);
                assert_eq!(info.asset_kind, AssetKind::ZipBundle);
            }
            other => panic!("expected Available(ZipBundle), got {other:?}"),
        }
        assert_eq!(
            parse_release_json("0.2.0", json_both),
            CheckOutcome::UpToDate {
                current: "0.2.0".into(),
                latest: "0.2.0".into(),
            }
        );
    }
}
