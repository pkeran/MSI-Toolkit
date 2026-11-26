//! Clean, zero-bloat Windows 11 native GUI for MSI-Toolkit Setup, Update & Uninstall.
//!
//! Uses only standard Win32 + Common Controls v6 + DWM rounded corners (0 GPU DLLs),
//! crisp `Segoe UI Variable Text` fonts scaled to Per-Monitor DPI, and native UAC
//! shields (`BCM_SETSHIELD`) on elevated actions.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex};
use std::thread;

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH,
    DeleteObject, FF_SWISS, FW_NORMAL, FW_SEMIBOLD, FillRect, HBRUSH, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BCM_SETSHIELD, BST_CHECKED, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    InitCommonControlsEx, PBM_SETPOS, PBM_SETRANGE32, PBS_SMOOTH, PROGRESS_CLASSW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON,
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, IDC_ARROW, KillTimer,
    LoadCursorW, LoadIconW, MB_ICONERROR, MB_OK, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, SendMessageW, SetTimer,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DESTROY,
    WM_ERASEBKGND, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::ops::{self, InstallOptions, SystemProbe, UninstallOptions, wide};

const CLASS_NAME: PCWSTR = w!("MSIToolkit.Setup.Window.v1");
const WM_PROGRESS: u32 = WM_APP + 10;
const WM_DONE: u32 = WM_APP + 11;
const AUTO_CLOSE_TIMER: usize = 99;

const ID_BTN_PRIMARY: isize = 101;
const ID_BTN_CANCEL: isize = 102;
const ID_CHK_AUTOSTART: isize = 201;
const ID_CHK_STARTMENU: isize = 202;
const ID_CHK_DESKTOP: isize = 203;
const ID_CHK_LAUNCH: isize = 204;
const ID_CHK_PURGE: isize = 205;

// Crisp Windows 11 Light Porcelain palette (0x00BBGGRR COLORREF format)
const CLR_BG: COLORREF = COLORREF(0x00FB_F9F8); // #F8F9FB
const CLR_CARD: COLORREF = COLORREF(0x00FF_FFFF); // #FFFFFF
const CLR_BORDER: COLORREF = COLORREF(0x00EB_E6E4); // #E4E6EB
const CLR_ACCENT: COLORREF = COLORREF(0x003E_30D2); // #D2303E (refined MSI crimson stripe)
const CLR_TEXT: COLORREF = COLORREF(0x001C_1918); // #18191C
const CLR_TEXT_MUTED: COLORREF = COLORREF(0x0060_5652); // #525660

#[derive(Debug, Clone)]
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

struct SharedState {
    status_text: String,
    percent: u32,
    done_ok: Option<Result<(), String>>,
}

struct WindowState {
    mode: GuiMode,
    probe: SystemProbe,
    busy: bool,
    finished: bool,
    exit_code: i32,
    scale: f32,
    card_rect: RECT,
    h_bg: HBRUSH,
    h_card: HBRUSH,
    h_border: HBRUSH,
    h_accent: HBRUSH,
    f_title: HFONT,
    f_body: HFONT,
    f_small: HFONT,
    lbl_status: HWND,
    progress: HWND,
    btn_primary: HWND,
    btn_cancel: HWND,
    chk_autostart: HWND,
    chk_startmenu: HWND,
    chk_desktop: HWND,
    chk_launch: HWND,
    chk_purge: HWND,
    shared: Arc<Mutex<SharedState>>,
}

impl WindowState {
    fn s(&self, logical: i32) -> i32 {
        (logical as f32 * self.scale).round() as i32
    }
}

impl Drop for WindowState {
    fn drop(&mut self) {
        unsafe {
            for h in [
                HGDIOBJ(self.h_bg.0),
                HGDIOBJ(self.h_card.0),
                HGDIOBJ(self.h_border.0),
                HGDIOBJ(self.h_accent.0),
                HGDIOBJ(self.f_title.0),
                HGDIOBJ(self.f_body.0),
                HGDIOBJ(self.f_small.0),
            ] {
                if !h.is_invalid() {
                    let _ = DeleteObject(h);
                }
            }
        }
    }
}

pub fn run(mode: GuiMode) -> i32 {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst = HINSTANCE(GetModuleHandleW(None).map(|m| m.0).unwrap_or(null_mut()));
        let icon = LoadIconW(Some(hinst), PCWSTR(1usize as *const u16)).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let src_ref = match &mode {
            GuiMode::Install { source_dir, .. } | GuiMode::Update { source_dir } => {
                source_dir.as_deref()
            }
            GuiMode::Uninstall { .. } => None,
        };
        let probe = ops::probe_system(src_ref);

        let (logical_w, logical_h, title) = match &mode {
            GuiMode::Install { .. } => (
                500,
                390,
                if probe.installed {
                    "升级 MSI-Toolkit"
                } else {
                    "安装 MSI-Toolkit"
                },
            ),
            GuiMode::Update { .. } => (460, 210, "更新 MSI-Toolkit"),
            GuiMode::Uninstall { .. } => (480, 290, "卸载 MSI-Toolkit"),
        };

        let title_w = wide(title);
        let style = WINDOW_STYLE(
            WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0,
        );
        let hwnd = match CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS_NAME,
            PCWSTR(title_w.as_ptr()),
            style,
            0,
            0,
            logical_w,
            logical_h,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return 1,
        };

        let dpi = GetDpiForWindow(hwnd).clamp(96, 288);
        let scale = dpi as f32 / 96.0;
        let sc = |v: i32| (v as f32 * scale).round() as i32;
        let win_w = sc(logical_w);
        let win_h = sc(logical_h);
        let sx = GetSystemMetrics(SM_CXSCREEN);
        let sy = GetSystemMetrics(SM_CYSCREEN);
        let x = ((sx - win_w) / 2).max(0);
        let y = ((sy - win_h) / 2).max(0);
        let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(hwnd, x, y, win_w, win_h, false);

        let corner = DWMWCP_ROUND.0 as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            std::mem::size_of::<u32>() as u32,
        );

        let f_title = make_font("Segoe UI Variable Display", sc(20), FW_SEMIBOLD.0 as i32);
        let f_body = make_font("Segoe UI Variable Text", sc(14), FW_NORMAL.0 as i32);
        let f_small = make_font("Segoe UI Variable Text", sc(12), FW_NORMAL.0 as i32);

        let mut st = Box::new(WindowState {
            mode: mode.clone(),
            probe: probe.clone(),
            busy: false,
            finished: false,
            exit_code: 0,
            scale,
            card_rect: RECT::default(),
            h_bg: CreateSolidBrush(CLR_BG),
            h_card: CreateSolidBrush(CLR_CARD),
            h_border: CreateSolidBrush(CLR_BORDER),
            h_accent: CreateSolidBrush(CLR_ACCENT),
            f_title,
            f_body,
            f_small,
            lbl_status: HWND::default(),
            progress: HWND::default(),
            btn_primary: HWND::default(),
            btn_cancel: HWND::default(),
            chk_autostart: HWND::default(),
            chk_startmenu: HWND::default(),
            chk_desktop: HWND::default(),
            chk_launch: HWND::default(),
            chk_purge: HWND::default(),
            shared: Arc::new(Mutex::new(SharedState {
                status_text: String::new(),
                percent: 0,
                done_ok: None,
            })),
        });

        build_controls(hwnd, hinst, &mut st, logical_w, logical_h);

        let st_ptr = Box::into_raw(st);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, st_ptr as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);

        // Auto-start elevated work if launched via --apply or --update
        match &(*st_ptr).mode {
            GuiMode::Install {
                auto_apply: Some(opts),
                ..
            } => {
                let opts = opts.clone();
                begin_install(hwnd, &mut *st_ptr, opts);
            }
            GuiMode::Update { source_dir } => {
                let p = &(*st_ptr).probe;
                let opts = InstallOptions {
                    auto_start: p.auto_start_enabled,
                    start_menu: p.start_menu_exists,
                    desktop_shortcut: p.desktop_shortcut_exists,
                    launch_ui: true,
                    source_dir: source_dir.clone(),
                    from_update: true,
                };
                begin_install(hwnd, &mut *st_ptr, opts);
            }
            GuiMode::Uninstall {
                auto_apply: Some(opts),
            } => {
                let opts = *opts;
                begin_uninstall(hwnd, &mut *st_ptr, opts);
            }
            _ => {}
        }

        let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let boxed = Box::from_raw(st_ptr);
        boxed.exit_code
    }
}

unsafe fn build_controls(
    hwnd: HWND,
    hinst: HINSTANCE,
    st: &mut WindowState,
    logical_w: i32,
    logical_h: i32,
) {
    let scale = st.scale;
    let sc = |v: i32| (v as f32 * scale).round() as i32;
    let client_w = logical_w - 16;
    let margin = 24;
    let content_w = client_w - margin * 2;

    match &st.mode {
        GuiMode::Install { .. } => {
            let headline = if let Some(old) = &st.probe.installed_version {
                format!("升级 MSI-Toolkit（v{old} → v{}）", ops::APP_VERSION)
            } else if st.probe.installed {
                format!("升级 / 修复 MSI-Toolkit v{}", ops::APP_VERSION)
            } else {
                format!("安装 MSI-Toolkit v{}", ops::APP_VERSION)
            };
            let subtitle = if st.probe.installed {
                "平滑覆盖程序文件，自动保留你的配置、规则与自启偏好"
            } else {
                "超轻量 Windows 11 原生硬件控制中心 · 零第三方驱动"
            };
            create_label(hwnd, hinst, &headline, sc(margin), sc(20), sc(content_w), sc(28), st.f_title);
            create_label(hwnd, hinst, subtitle, sc(margin), sc(50), sc(content_w), sc(20), st.f_small);

            // Options card geometry
            st.card_rect = RECT {
                left: sc(margin),
                top: sc(80),
                right: sc(margin + content_w),
                bottom: sc(244),
            };

            let inner_x = margin + 16;
            let inner_w = content_w - 32;
            create_label(
                hwnd,
                hinst,
                &format!("安装目录：{}（系统保护路径，保障免 UAC 运行）", st.probe.install_dir.display()),
                sc(inner_x),
                sc(94),
                sc(inner_w),
                sc(20),
                st.f_small,
            );

            st.chk_autostart = create_checkbox(
                hwnd,
                hinst,
                ID_CHK_AUTOSTART,
                "开机自动启动后台守护进程（登录后免 UAC 静默运行）",
                sc(inner_x),
                sc(122),
                sc(inner_w),
                sc(22),
                st.f_body,
                st.probe.auto_start_enabled,
            );
            st.chk_startmenu = create_checkbox(
                hwnd,
                hinst,
                ID_CHK_STARTMENU,
                "创建开始菜单快捷方式",
                sc(inner_x),
                sc(150),
                sc(inner_w),
                sc(22),
                st.f_body,
                st.probe.start_menu_exists,
            );
            st.chk_desktop = create_checkbox(
                hwnd,
                hinst,
                ID_CHK_DESKTOP,
                "创建桌面快捷方式",
                sc(inner_x),
                sc(178),
                sc(inner_w),
                sc(22),
                st.f_body,
                st.probe.desktop_shortcut_exists,
            );
            st.chk_launch = create_checkbox(
                hwnd,
                hinst,
                ID_CHK_LAUNCH,
                "安装完成后立即启动 MSI-Toolkit",
                sc(inner_x),
                sc(206),
                sc(inner_w),
                sc(22),
                st.f_body,
                true,
            );

            let initial_status = if !st.probe.has_payload {
                "提示：未检测到内置安装包载荷（请使用完整 MSI-Toolkit-Setup.exe）"
            } else {
                "准备就绪，点击下方按钮开始。"
            };
            st.lbl_status = create_label(
                hwnd,
                hinst,
                initial_status,
                sc(margin),
                sc(258),
                sc(content_w),
                sc(20),
                st.f_small,
            );
            st.progress = create_progress(hwnd, hinst, sc(margin), sc(282), sc(content_w), sc(14));

            let btn_y = logical_h - 80;
            let btn_w = 114;
            let btn_h = 34;
            st.btn_cancel = create_button(
                hwnd,
                hinst,
                ID_BTN_CANCEL,
                "取消",
                sc(margin + content_w - btn_w * 2 - 10),
                sc(btn_y),
                sc(btn_w),
                sc(btn_h),
                st.f_body,
                false,
            );
            let primary_label = if st.probe.installed { "立即升级" } else { "立即安装" };
            st.btn_primary = create_button(
                hwnd,
                hinst,
                ID_BTN_PRIMARY,
                primary_label,
                sc(margin + content_w - btn_w),
                sc(btn_y),
                sc(btn_w),
                sc(btn_h),
                st.f_body,
                true,
            );
            if !st.probe.elevated {
                SendMessageW(st.btn_primary, BCM_SETSHIELD, Some(WPARAM(0)), Some(LPARAM(1)));
            }
            if !st.probe.has_payload {
                let _ = EnableWindow(st.btn_primary, false);
            }
        }
        GuiMode::Update { .. } => {
            create_label(
                hwnd,
                hinst,
                &format!("正在更新 MSI-Toolkit 至 v{}", ops::APP_VERSION),
                sc(margin),
                sc(22),
                sc(content_w),
                sc(28),
                st.f_title,
            );
            create_label(
                hwnd,
                hinst,
                "升级过程中将自动保留你的配置、自动化规则与开机自启设置。",
                sc(margin),
                sc(54),
                sc(content_w),
                sc(20),
                st.f_small,
            );
            st.lbl_status = create_label(
                hwnd,
                hinst,
                "正在准备升级…",
                sc(margin),
                sc(92),
                sc(content_w),
                sc(20),
                st.f_small,
            );
            st.progress = create_progress(hwnd, hinst, sc(margin), sc(118), sc(content_w), sc(16));
            st.btn_primary = create_button(
                hwnd,
                hinst,
                ID_BTN_PRIMARY,
                "完成",
                sc(margin + content_w - 104),
                sc(logical_h - 74),
                sc(104),
                sc(32),
                st.f_body,
                true,
            );
            let _ = EnableWindow(st.btn_primary, false);
        }
        GuiMode::Uninstall { .. } => {
            create_label(
                hwnd,
                hinst,
                "卸载 MSI-Toolkit",
                sc(margin),
                sc(20),
                sc(content_w),
                sc(28),
                st.f_title,
            );
            create_label(
                hwnd,
                hinst,
                "将停止后台守护进程并移除程序文件、开机任务与快捷方式。",
                sc(margin),
                sc(52),
                sc(content_w),
                sc(20),
                st.f_small,
            );

            st.card_rect = RECT {
                left: sc(margin),
                top: sc(80),
                right: sc(margin + content_w),
                bottom: sc(158),
            };
            create_label(
                hwnd,
                hinst,
                "安全承诺：绝不触碰 msiapcfg.dll、WmiAcpi、Nahimic 或 Killer 等底层驱动。",
                sc(margin + 14),
                sc(94),
                sc(content_w - 28),
                sc(20),
                st.f_small,
            );
            st.chk_purge = create_checkbox(
                hwnd,
                hinst,
                ID_CHK_PURGE,
                "同时清除本地用户配置与自动化规则（%LOCALAPPDATA%\\MSI-Toolkit）",
                sc(margin + 14),
                sc(122),
                sc(content_w - 28),
                sc(22),
                st.f_body,
                false,
            );

            st.lbl_status = create_label(
                hwnd,
                hinst,
                "确认后将彻底清理程序文件，无后台残留。",
                sc(margin),
                sc(170),
                sc(content_w),
                sc(20),
                st.f_small,
            );
            st.progress = create_progress(hwnd, hinst, sc(margin), sc(194), sc(content_w), sc(14));

            let btn_y = logical_h - 76;
            let btn_w = 114;
            let btn_h = 34;
            st.btn_cancel = create_button(
                hwnd,
                hinst,
                ID_BTN_CANCEL,
                "取消",
                sc(margin + content_w - btn_w * 2 - 10),
                sc(btn_y),
                sc(btn_w),
                sc(btn_h),
                st.f_body,
                false,
            );
            st.btn_primary = create_button(
                hwnd,
                hinst,
                ID_BTN_PRIMARY,
                "确认卸载",
                sc(margin + content_w - btn_w),
                sc(btn_y),
                sc(btn_w),
                sc(btn_h),
                st.f_body,
                true,
            );
            if !st.probe.elevated {
                SendMessageW(st.btn_primary, BCM_SETSHIELD, Some(WPARAM(0)), Some(LPARAM(1)));
            }
        }
    }
}

unsafe fn begin_install(hwnd: HWND, st: &mut WindowState, opts: InstallOptions) {
    if st.busy {
        return;
    }
    if !ops::is_elevated() {
        let mut args = vec![
            "--apply".to_string(),
            format!("--autostart={}", i32::from(opts.auto_start)),
            format!("--startmenu={}", i32::from(opts.start_menu)),
            format!("--desktop={}", i32::from(opts.desktop_shortcut)),
            format!("--launch={}", i32::from(opts.launch_ui)),
        ];
        if opts.from_update {
            args.push("--from-update".into());
        }
        if let Some(src) = &opts.source_dir {
            args.push("--source".into());
            args.push(src.to_string_lossy().into_owned());
        }
        set_status(st, 5, "等待管理员授权…");
        match ops::elevate_and_wait(&args) {
            Ok(0) => {
                st.exit_code = 0;
                let _ = DestroyWindow(hwnd);
            }
            Ok(code) => {
                st.exit_code = code;
                set_status(st, 0, "安装未完成，请重试。");
            }
            Err(msg) => {
                set_status(st, 0, &msg);
            }
        }
        return;
    }

    st.busy = true;
    disable_inputs(st);
    let shared = st.shared.clone();
    let hwnd_raw = hwnd.0 as usize;
    thread::spawn(move || {
        let res = ops::run_install(&opts, |pct, msg| {
            if let Ok(mut s) = shared.lock() {
                s.percent = pct;
                s.status_text = msg.to_string();
            }
            let _ = unsafe {
                PostMessageW(Some(HWND(hwnd_raw as _)), WM_PROGRESS, WPARAM(0), LPARAM(0))
            };
        });
        if let Ok(mut s) = shared.lock() {
            s.done_ok = Some(res);
        }
        let _ = unsafe { PostMessageW(Some(HWND(hwnd_raw as _)), WM_DONE, WPARAM(0), LPARAM(0)) };
    });
}

unsafe fn begin_uninstall(hwnd: HWND, st: &mut WindowState, opts: UninstallOptions) {
    if st.busy {
        return;
    }
    if !ops::is_elevated() {
        let mut args = vec!["--uninstall".to_string(), "--apply-uninstall".to_string()];
        if opts.purge_config {
            args.push("--purge".into());
        }
        set_status(st, 5, "等待管理员授权…");
        match ops::elevate_and_wait(&args) {
            Ok(0) => {
                st.exit_code = 0;
                let _ = DestroyWindow(hwnd);
            }
            Ok(code) => {
                st.exit_code = code;
                set_status(st, 0, "卸载未完成，请重试。");
            }
            Err(msg) => {
                set_status(st, 0, &msg);
            }
        }
        return;
    }

    st.busy = true;
    disable_inputs(st);
    let shared = st.shared.clone();
    let hwnd_raw = hwnd.0 as usize;
    thread::spawn(move || {
        let res = ops::run_uninstall(&opts, |pct, msg| {
            if let Ok(mut s) = shared.lock() {
                s.percent = pct;
                s.status_text = msg.to_string();
            }
            let _ = unsafe {
                PostMessageW(Some(HWND(hwnd_raw as _)), WM_PROGRESS, WPARAM(0), LPARAM(0))
            };
        });
        if let Ok(mut s) = shared.lock() {
            s.done_ok = Some(res);
        }
        let _ = unsafe { PostMessageW(Some(HWND(hwnd_raw as _)), WM_DONE, WPARAM(0), LPARAM(0)) };
    });
}

unsafe fn disable_inputs(st: &WindowState) {
    for h in [
        st.btn_primary,
        st.btn_cancel,
        st.chk_autostart,
        st.chk_startmenu,
        st.chk_desktop,
        st.chk_launch,
        st.chk_purge,
    ] {
        if !h.0.is_null() {
            let _ = EnableWindow(h, false);
        }
    }
}

unsafe fn set_status(st: &WindowState, pct: u32, text: &str) {
    if !st.lbl_status.0.is_null() {
        let w = wide(text);
        let _ = SetWindowTextW(st.lbl_status, PCWSTR(w.as_ptr()));
    }
    if !st.progress.0.is_null() {
        SendMessageW(st.progress, PBM_SETPOS, Some(WPARAM(pct as usize)), Some(LPARAM(0)));
    }
}

unsafe fn is_checked(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    SendMessageW(hwnd, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 == BST_CHECKED.0 as isize
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let st_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState };
    let Some(st) = (unsafe { st_ptr.as_mut() }) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };

    match msg {
        WM_ERASEBKGND => unsafe {
            let hdc = HDC(wparam.0 as _);
            let mut rc = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, st.h_bg);
            // Crisp 3px accent bar along top edge
            let top_bar = RECT {
                left: 0,
                top: 0,
                right: rc.right,
                bottom: st.s(3).max(2),
            };
            FillRect(hdc, &top_bar, st.h_accent);
            // Optional options card surface + 1px hairline border
            if st.card_rect.right > st.card_rect.left {
                let outer = RECT {
                    left: st.card_rect.left - 1,
                    top: st.card_rect.top - 1,
                    right: st.card_rect.right + 1,
                    bottom: st.card_rect.bottom + 1,
                };
                FillRect(hdc, &outer, st.h_border);
                FillRect(hdc, &st.card_rect, st.h_card);
            }
            LRESULT(1)
        },
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
            let hdc = HDC(wparam.0 as _);
            let ctrl = HWND(lparam.0 as _);
            let in_card = is_child_in_card(hwnd, ctrl, &st.card_rect);
            SetTextColor(hdc, if ctrl == st.lbl_status { CLR_TEXT_MUTED } else { CLR_TEXT });
            SetBkMode(hdc, TRANSPARENT);
            SetBkColor(hdc, if in_card { CLR_CARD } else { CLR_BG });
            LRESULT(if in_card { st.h_card.0 } else { st.h_bg.0 } as isize)
        },
        WM_COMMAND => unsafe {
            let id = (wparam.0 & 0xFFFF) as isize;
            if id == ID_BTN_CANCEL && !st.busy {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            if id == ID_BTN_PRIMARY {
                if st.finished {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                match &st.mode {
                    GuiMode::Install { source_dir, .. } => {
                        let opts = InstallOptions {
                            auto_start: is_checked(st.chk_autostart),
                            start_menu: is_checked(st.chk_startmenu),
                            desktop_shortcut: is_checked(st.chk_desktop),
                            launch_ui: is_checked(st.chk_launch),
                            source_dir: source_dir.clone(),
                            from_update: false,
                        };
                        begin_install(hwnd, st, opts);
                    }
                    GuiMode::Uninstall { .. } => {
                        let opts = UninstallOptions {
                            purge_config: is_checked(st.chk_purge),
                        };
                        begin_uninstall(hwnd, st, opts);
                    }
                    GuiMode::Update { .. } => {}
                }
                return LRESULT(0);
            }
            LRESULT(0)
        },
        WM_PROGRESS => unsafe {
            let (pct, text) = st
                .shared
                .lock()
                .map(|s| (s.percent, s.status_text.clone()))
                .unwrap_or((0, String::new()));
            set_status(st, pct, &text);
            LRESULT(0)
        },
        WM_DONE => unsafe {
            st.busy = false;
            st.finished = true;
            let result = st.shared.lock().ok().and_then(|mut s| s.done_ok.take());
            match result {
                Some(Ok(())) => {
                    st.exit_code = 0;
                    let done_label = match &st.mode {
                        GuiMode::Uninstall { .. } => "已安全卸载 MSI-Toolkit，点击完成关闭窗口。",
                        GuiMode::Update { .. } => "升级完成，正在启动新版 MSI-Toolkit…",
                        GuiMode::Install { .. } => "MSI-Toolkit 安装完成！",
                    };
                    set_status(st, 100, done_label);
                    let w = wide("完成");
                    let _ = SetWindowTextW(st.btn_primary, PCWSTR(w.as_ptr()));
                    SendMessageW(st.btn_primary, BCM_SETSHIELD, Some(WPARAM(0)), Some(LPARAM(0)));
                    let _ = EnableWindow(st.btn_primary, true);
                    if matches!(st.mode, GuiMode::Update { .. })
                        || matches!(
                            st.mode,
                            GuiMode::Install {
                                auto_apply: Some(InstallOptions { from_update: true, .. }),
                                ..
                            }
                        )
                    {
                        let _ = SetTimer(Some(hwnd), AUTO_CLOSE_TIMER, 1_100, None);
                    }
                }
                Some(Err(err)) => {
                    st.exit_code = 1;
                    set_status(st, 0, &format!("操作失败：{err}"));
                    let _ = EnableWindow(st.btn_primary, true);
                    if !st.btn_cancel.0.is_null() {
                        let _ = EnableWindow(st.btn_cancel, true);
                    }
                    let title = wide("MSI-Toolkit 安装程序");
                    let body = wide(&err);
                    MessageBoxW(
                        Some(hwnd),
                        PCWSTR(body.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        MB_OK | MB_ICONERROR,
                    );
                }
                None => {}
            }
            LRESULT(0)
        },
        WM_TIMER if wparam.0 == AUTO_CLOSE_TIMER => unsafe {
            let _ = KillTimer(Some(hwnd), AUTO_CLOSE_TIMER);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        },
        WM_CLOSE => unsafe {
            if !st.busy {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        },
        WM_DESTROY => unsafe {
            PostQuitMessage(0);
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn is_child_in_card(parent: HWND, child: HWND, card: &RECT) -> bool {
    if card.right <= card.left || child.0.is_null() {
        return false;
    }
    let mut rc = RECT::default();
    if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(child, &mut rc).is_err() {
        return false;
    }
    let mut pt = windows::Win32::Foundation::POINT {
        x: rc.left + 2,
        y: rc.top + 2,
    };
    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(parent, &mut pt);
    pt.x >= card.left && pt.x <= card.right && pt.y >= card.top && pt.y <= card.bottom
}

unsafe fn make_font(face: &str, height: i32, weight: i32) -> HFONT {
    let face_w = wide(face);
    CreateFontW(
        -height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
        PCWSTR(face_w.as_ptr()),
    )
}

unsafe fn create_label(
    parent: HWND,
    hinst: HINSTANCE,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: HFONT,
) -> HWND {
    let t = wide(text);
    let ctrl = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(t.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        Some(parent),
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    ctrl
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_checkbox(
    parent: HWND,
    hinst: HINSTANCE,
    id: isize,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: HFONT,
    checked: bool,
) -> HWND {
    let t = wide(text);
    let ctrl = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(t.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut _)),
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    if checked {
        SendMessageW(
            ctrl,
            BM_SETCHECK,
            Some(WPARAM(BST_CHECKED.0 as usize)),
            Some(LPARAM(0)),
        );
    }
    ctrl
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_button(
    parent: HWND,
    hinst: HINSTANCE,
    id: isize,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: HFONT,
    primary: bool,
) -> HWND {
    let t = wide(text);
    let bs = if primary { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON };
    let ctrl = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(t.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | bs as u32),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut _)),
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    ctrl
}

unsafe fn create_progress(
    parent: HWND,
    hinst: HINSTANCE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    let ctrl = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PROGRESS_CLASSW,
        PCWSTR::null(),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | PBS_SMOOTH),
        x,
        y,
        w,
        h,
        Some(parent),
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    SendMessageW(ctrl, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(100)));
    SendMessageW(ctrl, PBM_SETPOS, Some(WPARAM(0)), Some(LPARAM(0)));
    ctrl
}
