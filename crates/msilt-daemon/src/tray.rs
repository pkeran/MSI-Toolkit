use std::io;

use msilt_core::{CapabilityFlags, ControlChange, PerformanceMode, SystemSnapshot};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

use crate::win::{os_error, wide};

pub(crate) const TRAY_MESSAGE: u32 = WM_APP + 1;
pub(crate) const TRAY_ID: u32 = 1;
const OPEN: usize = 1;
const EXIT: usize = 2;
const COOLER: usize = 3;
const MODE: usize = 16;
const REFRESH: usize = 64;
const MODES: [PerformanceMode; 4] = [
    PerformanceMode::SuperBattery,
    PerformanceMode::Silent,
    PerformanceMode::Balanced,
    PerformanceMode::ExtremePerformance,
];

pub(crate) fn mode_name(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::SuperBattery => "超级省电",
        PerformanceMode::Silent => "静音",
        PerformanceMode::Balanced => "平衡",
        PerformanceMode::ExtremePerformance => "极致性能",
    }
}

pub(crate) enum Choice {
    Open,
    Exit,
    Control(ControlChange),
}

pub(crate) struct Tray {
    hwnd: HWND,
    icon: HICON,
    installed: bool,
}

impl Tray {
    pub(crate) fn new(hwnd: HWND) -> io::Result<Self> {
        let mut tray = Self {
            hwnd,
            icon: make_icon()?,
            installed: false,
        };
        tray.restore();
        Ok(tray)
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    pub(crate) fn restore(&mut self) {
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = self.icon;
        for (dest, value) in data.szTip.iter_mut().zip("MSI Toolkit".encode_utf16()) {
            *dest = value;
        }
        self.installed = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
        if self.installed {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                }
                self.installed = false;
            }
        }
    }

    pub(crate) fn menu(
        &self,
        snapshot: &SystemSnapshot,
        x: i32,
        y: i32,
    ) -> io::Result<Option<Choice>> {
        let menu = Menu::new()?;
        menu.item(OPEN, "打开 MSI-Toolkit", true, false)?;
        unsafe {
            let _ = SetMenuDefaultItem(menu.0, OPEN as u32, 0);
        }
        let mode = Menu::new()?;
        for (index, entry) in MODES.iter().copied().enumerate() {
            mode.item(
                MODE + index,
                mode_name(entry),
                snapshot
                    .writable
                    .contains(CapabilityFlags::PERFORMANCE_MODE)
                    && snapshot
                        .hardware
                        .supported_performance_modes
                        .contains(&entry),
                snapshot.hardware.performance_mode == Some(entry),
            )?;
        }
        menu.submenu(mode, "性能模式")?;
        let cooler = snapshot.hardware.cooler_boost;
        menu.item(
            COOLER,
            "强冷散热",
            cooler.is_some() && snapshot.writable.contains(CapabilityFlags::COOLER_BOOST),
            cooler == Some(true),
        )?;
        let refresh = Menu::new()?;
        let rates = &snapshot.hardware.supported_refresh_rates;
        let can_refresh = snapshot
            .writable
            .contains(CapabilityFlags::DISPLAY_REFRESH_RATE);
        for (index, hz) in rates.iter().take(32).copied().enumerate() {
            refresh.item(
                REFRESH + index,
                &format!("{hz} Hz"),
                can_refresh,
                snapshot.hardware.display_refresh_hz == Some(hz),
            )?;
        }
        if rates.is_empty() {
            refresh.item(0, "不可用", false, false)?;
        }
        menu.submenu(refresh, "屏幕刷新率")?;
        unsafe { AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null()) }.map_err(os_error)?;
        menu.item(EXIT, "退出后台服务", true, false)?;
        let (mut x, mut y) = (x, y);
        if (x, y) == (-1, -1) {
            let mut cursor = Default::default();
            unsafe { GetCursorPos(&mut cursor) }.map_err(os_error)?;
            (x, y) = (cursor.x, cursor.y);
        }
        let choice = unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            let choice = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                x,
                y,
                None,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            choice.0 as usize
        };
        Ok(match choice {
            OPEN => Some(Choice::Open),
            EXIT => Some(Choice::Exit),
            COOLER => cooler.map(|enabled| {
                Choice::Control(ControlChange::SetCoolerBoost { enabled: !enabled })
            }),
            id if (MODE..MODE + MODES.len()).contains(&id) => {
                Some(Choice::Control(ControlChange::SetPerformanceMode {
                    mode: MODES[id - MODE],
                }))
            }
            id if (REFRESH..REFRESH + 32).contains(&id) => rates
                .get(id - REFRESH)
                .map(|&hz| Choice::Control(ControlChange::SetDisplayRefreshRate { hz })),
            _ => None,
        })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            if self.installed {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            let _ = DestroyIcon(self.icon);
        }
    }
}

struct Menu(HMENU);

impl Menu {
    fn new() -> io::Result<Self> {
        unsafe { CreatePopupMenu() }.map(Self).map_err(os_error)
    }

    fn item(&self, id: usize, name: &str, enabled: bool, checked: bool) -> io::Result<()> {
        let name = wide(name);
        let mut flags = MF_STRING;
        if !enabled {
            flags |= MF_GRAYED;
        }
        if checked {
            flags |= MF_CHECKED;
        }
        unsafe { AppendMenuW(self.0, flags, id, PCWSTR(name.as_ptr())) }.map_err(os_error)
    }

    fn submenu(&self, child: Self, name: &str) -> io::Result<()> {
        let name = wide(name);
        unsafe {
            AppendMenuW(
                self.0,
                MF_POPUP | MF_STRING,
                child.0.0 as usize,
                PCWSTR(name.as_ptr()),
            )
        }
        .map_err(os_error)?;
        std::mem::forget(child); // DestroyMenu recursively owns attached submenus.
        Ok(())
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

fn make_icon() -> io::Result<HICON> {
    // Load the unified MSI-Toolkit icon resource (#1, embedded from assets/msilt.ico)
    // at exact system small-icon DPI metrics so tray, titlebar, and taskbar match 1:1.
    unsafe {
        let module = GetModuleHandleW(None).map_err(os_error)?;
        let w = GetSystemMetrics(SM_CXSMICON).max(16);
        let h = GetSystemMetrics(SM_CYSMICON).max(16);
        let handle = LoadImageW(
            Some(module.into()),
            PCWSTR(1usize as *const u16),
            IMAGE_ICON,
            w,
            h,
            LR_DEFAULTCOLOR,
        )
        .map_err(os_error)?;
        Ok(HICON(handle.0))
    }
}
