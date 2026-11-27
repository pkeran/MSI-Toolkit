//! Windows 11 DWM bridge.
//!
//! Every attribute below was verified on the reference machine (Windows 11
//! 26200, 150% DPI) with `hr = 0x00000000` and a `DwmGetWindowAttribute`
//! read-back, plus a pixel test proving the system backdrop actually composites
//! through Slint's transparent surface. See `docs/UI_DESIGN_SPEC.md` §8.
//!
//! Design rules:
//!   * Zero extra runtime dependencies: only `dwmapi.dll` / `user32.dll` calls.
//!   * Never fatal: any failing attribute degrades to a solid surface.
//!   * No `WS_EX_LAYERED`, no `DwmEnableBlurBehindWindow` — both break Mica.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW, DWMSBT_TABBEDWINDOW, DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetSystemMetrics, LoadImageW, SendMessageW, SetWindowLongPtrW,
    SystemParametersInfoW, GWLP_WNDPROC, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    LR_SHARED, MINMAXINFO, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SWP_NOSIZE, WINDOWPOS,
    WM_GETMINMAXINFO, WM_SETICON, WM_WINDOWPOSCHANGING, WNDPROC,
};
use windows::core::PCWSTR;

/// `DwmSetWindowAttribute(38)` values that are not all exported by every
/// `windows` crate version.
const DWMSBT_ACRYLIC: DWM_SYSTEMBACKDROP_TYPE = DWM_SYSTEMBACKDROP_TYPE(3);
const _: () = assert!(DWMSBT_MAINWINDOW.0 == 2);
const _: () = assert!(DWMSBT_TABBEDWINDOW.0 == 4);
const _: () = assert!(DWMSBT_ACRYLIC.0 == 3);
const _: () = assert!(DWMWCP_ROUND.0 == 2);

/// Outcome of the DWM handshake, surfaced in the diagnostics export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwmReport {
    /// `DWMWA_USE_IMMERSIVE_DARK_MODE (20)` accepted.
    pub immersive_dark: bool,
    /// `DWMWA_WINDOW_CORNER_PREFERENCE (33) = ROUND` accepted.
    pub rounded: bool,
    /// `DWMWA_SYSTEMBACKDROP_TYPE (38) = Mica` accepted **and** read back.
    pub mica: bool,
    /// `DwmExtendFrameIntoClientArea(-1)` accepted.
    pub frame_extended: bool,
    /// `WM_GETMINMAXINFO` subclass installed (960x620 logical minimum).
    pub min_size: bool,
    /// Final dark/light decision used for the tokens.
    pub dark: bool,
    /// `SPI_GETCLIENTAREAANIMATION` (false => all durations collapse to 0 ms).
    pub animations: bool,
}

impl Default for DwmReport {
    fn default() -> Self {
        Self {
            immersive_dark: false,
            rounded: false,
            mica: false,
            frame_extended: false,
            min_size: false,
            dark: true,
            animations: true,
        }
    }
}

impl DwmReport {
    /// One-line summary for the diagnostics export (no paths, no serials).
    pub fn summary(&self) -> String {
        format!(
            "dwm: dark={} immersive={} rounded={} mica={} frame={} min-size={} animations={}",
            self.dark, self.immersive_dark, self.rounded, self.mica, self.frame_extended,
            self.min_size, self.animations
        )
    }
}

/// Extracts the native `HWND` from a Slint window.
pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(win32) => {
            let raw_hwnd = win32.hwnd.get();
            (raw_hwnd != 0).then(|| HWND(raw_hwnd as *mut c_void))
        }
        _ => None,
    }
}

/// `HKCU\...\Themes\Personalize\AppsUseLightTheme` (0 = dark).
///
/// Returns `None` when the value is absent (fresh profile / policy), so the
/// caller can fall back to Slint's own resolved colour scheme.
pub fn registry_prefers_dark() -> Option<bool> {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            windows::core::w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    (status.0 == 0).then_some(value == 0)
}

/// Resolved Windows 11 system accent pair `(dark_light2, light_dark1)`, matching
/// WinUI 2.8.7 (`SystemAccentColorLight2` on dark, `SystemAccentColorDark1` on light).
pub fn system_accent_pair() -> Option<((u8, u8, u8), (u8, u8, u8))> {
    let mut palette = [0u8; 32];
    let mut size = palette.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent"),
            windows::core::w!("AccentPalette"),
            RRF_RT_REG_BINARY,
            None,
            Some(palette.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status.0 != 0 || size != 32 {
        return None;
    }
    extract_accent_pair(&palette)
}

fn extract_accent_pair(palette: &[u8; 32]) -> Option<((u8, u8, u8), (u8, u8, u8))> {
    // Win11 Explorer AccentPalette layout (8 x RGBA):
    // [0]=Light3, [1]=Light2, [2]=Light1, [3]=Base, [4]=Dark1, [5]=Dark2, [6]=Dark3
    let dark_accent = (palette[4], palette[5], palette[6]);
    let light_accent = (palette[16], palette[17], palette[18]);
    let lum = |(r, g, b): (u8, u8, u8)| -> f32 {
        0.2126 * (f32::from(r) / 255.0)
            + 0.7152 * (f32::from(g) / 255.0)
            + 0.0722 * (f32::from(b) / 255.0)
    };
    // Guard against near-black dark accent or near-white light accent (WCAG safety).
    if lum(dark_accent) < 0.32 || lum(light_accent) > 0.45 {
        return None;
    }
    Some((dark_accent, light_accent))
}

pub fn open_power_settings() {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(hwnd: *mut c_void, operation: *const u16, file: *const u16, parameters: *const u16, directory: *const u16, show: i32) -> isize;
    }
    let uri = wide("ms-settings:powersleep");
    unsafe { ShellExecuteW(std::ptr::null_mut(), std::ptr::null(), uri.as_ptr(), std::ptr::null(), std::ptr::null(), 1); }
}

/// Converts a Unix timestamp in milliseconds to local wall-clock `(hour, minute, second)`.
pub fn local_hms(unix_ms: u64) -> Option<(u16, u16, u16)> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct SystemTimeWin {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millisecond: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FileTimeToLocalFileTime(utc: *const FileTime, local: *mut FileTime) -> i32;
        fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SystemTimeWin) -> i32;
    }
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = unix_ms.checked_mul(10_000)?.checked_add(UNIX_EPOCH_TICKS)?;
    let utc = FileTime {
        low: ticks as u32,
        high: (ticks >> 32) as u32,
    };
    let mut local = FileTime::default();
    let mut st = SystemTimeWin::default();
    unsafe {
        if FileTimeToLocalFileTime(&utc, &mut local) == 0 {
            return None;
        }
        if FileTimeToSystemTime(&local, &mut st) == 0 {
            return None;
        }
    }
    Some((st.hour, st.minute, st.second))
}

/// `SPI_GETCLIENTAREAANIMATION`; false means "reduce motion" is on.
pub fn client_area_animations() -> bool {
    let mut enabled: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut i32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && enabled != 0
}

fn set_u32(hwnd: HWND, attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, value: u32) -> bool {
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            &value as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok()
}

fn get_u32(hwnd: HWND, attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE) -> Option<u32> {
    let mut value: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            attribute,
            &mut value as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok().then_some(value)
}

/// Applies the full Win11 chrome in the verified order.
///
/// Call this *after* the first frame has been scheduled; the software renderer
/// has no `RenderingSetup` hook, and Slint paints `WM_ERASEBKGND` black, so
/// there is no white flash in between.
pub fn apply_chrome(hwnd: HWND, dark: bool) -> DwmReport {
    let mut report = DwmReport { dark, ..DwmReport::default() };

    // 1. dark frame
    report.immersive_dark = set_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(dark));
    // 2. rounded corners
    report.rounded = set_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0 as u32);
    // 3. frame border colour: near-black on dark, default on light
    let border = if dark { 0x0033_2A26 } else { 0xFFFF_FFFF };
    let _ = set_u32(hwnd, DWMWA_BORDER_COLOR, border);
    // 4. system backdrop: Mica (2). Read back to prove acceptance.
    let accepted = set_u32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW.0 as u32);
    report.mica = accepted
        && get_u32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE).is_some_and(|v| v == DWMSBT_MAINWINDOW.0 as u32);
    if !report.mica {
        // Clean fallback: ask for the plain solid surface rather than leaving a
        // half-applied backdrop.
        let _ = set_u32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, 1);
    }
    // 5. extend the frame so the client area is composited with the backdrop
    let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    report.frame_extended = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) }.is_ok();
    // 6. bind the embedded MSI-Toolkit icon resource (#1) to titlebar + taskbar + Alt-Tab
    apply_window_icon(hwnd);

    report
}

/// Applies the embedded Win32 icon resource (ID 1) at exact DPI-scaled small & big sizes
/// so the titlebar, taskbar, and Alt-Tab always match `assets/msilt.ico`.
pub fn apply_window_icon(hwnd: HWND) {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let res_id = PCWSTR(1usize as *const u16);
        let flags = LR_DEFAULTCOLOR | LR_SHARED;
        let sm_w = GetSystemMetrics(SM_CXSMICON).max(16);
        let sm_h = GetSystemMetrics(SM_CYSMICON).max(16);
        if let Ok(small) = LoadImageW(Some(module.into()), res_id, IMAGE_ICON, sm_w, sm_h, flags) {
            if !small.is_invalid() {
                let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
            }
        }
        let lg_w = GetSystemMetrics(SM_CXICON).max(32);
        let lg_h = GetSystemMetrics(SM_CYICON).max(32);
        if let Ok(big) = LoadImageW(Some(module.into()), res_id, IMAGE_ICON, lg_w, lg_h, flags) {
            if !big.is_invalid() {
                let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
            }
        }
    }
}

/// Re-applies only the pieces that change with the theme.
#[allow(dead_code)]
pub fn retheme(hwnd: HWND, dark: bool) -> bool {
    let a = set_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(dark));
    let border = if dark { 0x0033_2A26 } else { 0xFFFF_FFFF };
    let b = set_u32(hwnd, DWMWA_BORDER_COLOR, border);
    a && b
}

/// Mica Alt (`DWMWA_SYSTEMBACKDROP_TYPE = 4`), used for the nav rail later.
#[allow(dead_code)]
pub fn set_mica_alt(hwnd: HWND) -> bool {
    set_u32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TABBEDWINDOW.0 as u32)
}

// ── minimum tracking size ───────────────────────────────────────────────────
//
// Slint rejects `width` + `min-width` on the root `Window`, so the 960x620 floor
// is enforced natively through a `WM_GETMINMAXINFO` subclass instead of a
// second layout constraint.

static ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);
static MIN_WIDTH: AtomicIsize = AtomicIsize::new(0);
static MIN_HEIGHT: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn min_size_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_GETMINMAXINFO {
        let info = lparam.0 as *mut MINMAXINFO;
        if !info.is_null() {
            let w = MIN_WIDTH.load(Ordering::Relaxed);
            let h = MIN_HEIGHT.load(Ordering::Relaxed);
            if w > 0 {
                unsafe { (*info).ptMinTrackSize.x = w as i32 };
            }
            if h > 0 {
                unsafe { (*info).ptMinTrackSize.y = h as i32 };
            }
        }
    } else if msg == WM_WINDOWPOSCHANGING {
        // `MoveWindow` / `SetWindowPos` do not go through the tracking-size loop,
        // so the floor is also clamped here. Without this a programmatic resize
        // can shrink the window below the documented minimum.
        let pos = lparam.0 as *mut WINDOWPOS;
        if !pos.is_null() && (unsafe { (*pos).flags }.0 & SWP_NOSIZE.0) == 0 {
            let w = MIN_WIDTH.load(Ordering::Relaxed);
            let h = MIN_HEIGHT.load(Ordering::Relaxed);
            if w > 0 && (unsafe { (*pos).cx }) < w as i32 {
                unsafe { (*pos).cx = w as i32 };
            }
            if h > 0 && (unsafe { (*pos).cy }) < h as i32 {
                unsafe { (*pos).cy = h as i32 };
            }
        }
    }
    let original = ORIGINAL_PROC.load(Ordering::Relaxed);
    if original == 0 {
        return unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let proc: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(original) };
    unsafe { CallWindowProcW(proc, hwnd, msg, wparam, lparam) }
}

/// Installs the `WM_GETMINMAXINFO` hook. Idempotent; safe to call twice.
pub fn enforce_min_size(hwnd: HWND, width: i32, height: i32) -> bool {
    MIN_WIDTH.store(width as isize, Ordering::Relaxed);
    MIN_HEIGHT.store(height as isize, Ordering::Relaxed);
    if ORIGINAL_PROC.load(Ordering::Relaxed) != 0 {
        return true;
    }
    let new_proc = min_size_proc as *const () as isize;
    let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc) };
    if previous == 0 {
        return false;
    }
    ORIGINAL_PROC.store(previous, Ordering::Relaxed);
    true
}

/// Kept for completeness: the exact UTF-16 helper used by callers that need a
/// raw wide string without pulling in `windows::core::w!` at the call site.
#[allow(dead_code)]
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Current `PrivateUsage` in KiB — the number quoted in the performance budget
/// (12.6 MiB on the reference machine with the software renderer).
pub fn private_kib() -> Option<u32> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) }
        .is_ok()
        // `PagefileUsage` is the private commit charge; that is the figure the
        // performance budget quotes as "Private".
        .then(|| (counters.PagefileUsage / 1024) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_palette_extracts_win11_light2_and_dark1() {
        // Default Windows 11 blue AccentPalette (8 x RGBA)
        let mut palette = [0u8; 32];
        // [1] Light2 = #60CDFF
        palette[4..8].copy_from_slice(&[0x60, 0xCD, 0xFF, 0x00]);
        // [4] Dark1 = #005FB8
        palette[16..20].copy_from_slice(&[0x00, 0x5F, 0xB8, 0x00]);
        assert_eq!(
            extract_accent_pair(&palette),
            Some(((0x60, 0xCD, 0xFF), (0x00, 0x5F, 0xB8)))
        );
        // Reject near-black dark accent or near-white light accent
        let bad = [0x05u8; 32];
        assert_eq!(extract_accent_pair(&bad), None);
    }
}
