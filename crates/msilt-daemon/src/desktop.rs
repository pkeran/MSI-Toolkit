use std::{
    cell::Cell,
    ffi::c_void,
    io,
    os::windows::io::OwnedHandle,
    ptr,
    sync::{
        Arc,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Power::*, RemoteDesktop::*},
        UI::{Input::KeyboardAndMouse::*, Shell::NIN_SELECT, WindowsAndMessaging::*},
    },
    core::{GUID, PCWSTR, w},
};

use crate::{
    events::*,
    osd::Osd,
    security::launch_ui,
    tray::{self, Choice, Tray},
    win::os_error,
};

const DESKTOP_CLASS: PCWSTR = w!("MSIToolkit.Daemon.v1");
const WAKE_UI: u32 = WM_APP + 2;
const END_UI: u32 = WM_APP + 3;
const OPEN_HOTKEY: i32 = 0xa000;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const AC_DC: GUID = GUID::from_u128(0x5d3e9a59_e9d5_4b00_a6bd_ff34ff516548);
const BATTERY: GUID = GUID::from_u128(0xa7ad8041_b45a_4cae_87a3_eecbb468a9e1);
const SAVER: GUID = GUID::from_u128(0xe00958c0_c213_4ace_ac77_fecced2eeea5);
const DISPLAY: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);

pub(crate) struct UiSender {
    hwnd: usize,
    queue: SyncSender<UiEvent>,
}

impl UiSender {
    pub(crate) fn send(&self, event: UiEvent) -> bool {
        self.queue.try_send(event).is_ok()
            && unsafe { PostMessageW(Some(HWND(self.hwnd as _)), WAKE_UI, WPARAM(0), LPARAM(0)) }
                .is_ok()
    }

    pub(crate) fn finish(&self) {
        self.post(WM_CANCELMODE);
        self.post(END_UI);
    }

    fn post(&self, message: u32) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), message, WPARAM(0), LPARAM(0));
        }
    }
}

struct Routing {
    inbox: Arc<Inbox>,
    taskbar_created: u32,
    hide: Cell<bool>,
    headless: bool,
    session: u32,
}

struct EffectiveNotification(*mut c_void, Arc<Inbox>);

impl EffectiveNotification {
    fn register(inbox: Arc<Inbox>) -> io::Result<Self> {
        let mut registration = ptr::null_mut();
        let context = Some(Arc::as_ptr(&inbox).cast());
        // Version 2 adds GameMode and MixedReality on supported Windows releases.
        unsafe {
            PowerRegisterForEffectivePowerModeNotifications(
                2,
                Some(effective_mode),
                context,
                &mut registration,
            )
        }
        .or_else(|_| unsafe {
            PowerRegisterForEffectivePowerModeNotifications(
                1,
                Some(effective_mode),
                context,
                &mut registration,
            )
        })
        .map_err(os_error)?;
        Ok(Self(registration, inbox))
    }
}

impl Drop for EffectiveNotification {
    fn drop(&mut self) {
        if unsafe { PowerUnregisterFromEffectivePowerModeNotifications(self.0) }.is_err() {
            // Windows could still call back; keep its context alive until process exit.
            std::mem::forget(self.1.clone());
        }
    }
}

unsafe extern "system" fn effective_mode(mode: EFFECTIVE_POWER_MODE, context: *const c_void) {
    unsafe { &*context.cast::<Inbox>() }.effective(mode.0);
}

pub(crate) struct Desktop {
    window: HWND,
    instance: HINSTANCE,
    routing: Box<Routing>,
    tray: Option<Tray>,
    osd: Option<Osd>,
    power: Vec<HPOWERNOTIFY>,
    effective: Option<EffectiveNotification>,
    wts: bool,
    hotkey: bool,
    events: Receiver<UiEvent>,
    interactive: Arc<OwnedHandle>,
    last_open_ui: Option<Instant>,
}

impl Desktop {
    pub(crate) fn new(
        inbox: Arc<Inbox>,
        interactive: Arc<OwnedHandle>,
        session: u32,
        headless: bool,
    ) -> io::Result<(Self, UiSender)> {
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.map_err(os_error)?.0);
        let taskbar_created = if headless {
            0
        } else {
            unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
        };
        let routing = Box::new(Routing {
            inbox,
            taskbar_created,
            hide: Cell::new(false),
            headless,
            session,
        });
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: DESKTOP_CLASS,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let window = match unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                DESKTOP_CLASS,
                w!("MSI Toolkit Daemon"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                Some(ptr::from_ref(&*routing).cast()),
            )
        } {
            Ok(window) => window,
            Err(error) => {
                unsafe {
                    let _ = UnregisterClassW(DESKTOP_CLASS, Some(instance));
                }
                return Err(os_error(error));
            }
        };
        let (queue, events) = sync_channel(16);
        let mut desktop = Self {
            window,
            instance,
            routing,
            tray: None,
            osd: None,
            power: Vec::new(),
            effective: None,
            wts: false,
            hotkey: false,
            events,
            interactive,
            last_open_ui: None,
        };
        unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) }
            .map_err(os_error)?;
        desktop.wts = true;
        for guid in [AC_DC, BATTERY, SAVER, DISPLAY] {
            let registration = unsafe {
                RegisterPowerSettingNotification(
                    HANDLE(window.0),
                    &guid,
                    DEVICE_NOTIFY_WINDOW_HANDLE,
                )
            }
            .map_err(os_error)?;
            desktop.power.push(registration);
        }
        desktop.effective = EffectiveNotification::register(desktop.routing.inbox.clone()).ok();
        if !headless {
            desktop.osd = Some(Osd::new(window, instance));
            unsafe {
                let _ = ChangeWindowMessageFilterEx(window, tray::TRAY_MESSAGE, MSGFLT_ALLOW, None);
                if taskbar_created != 0 {
                    let _ =
                        ChangeWindowMessageFilterEx(window, taskbar_created, MSGFLT_ALLOW, None);
                }
            }
            desktop.tray = Some(Tray::new(window)?);
            desktop.hotkey = unsafe {
                RegisterHotKey(
                    Some(window),
                    OPEN_HOTKEY,
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    u32::from(b'M'),
                )
            }
            .is_ok();
            if !desktop.hotkey {
                eprintln!("msilt-daemon: Ctrl+Alt+M is not available");
            }
        }
        desktop.routing.inbox.flag(BITS_POWER_STATUS);
        Ok((
            desktop,
            UiSender {
                hwnd: window.0 as usize,
                queue,
            },
        ))
    }

    pub(crate) fn run(&mut self) -> io::Result<()> {
        let mut msg = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
            if result == 0 {
                break;
            }
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Some(osd) = &mut self.osd {
                if self.routing.hide.replace(false) {
                    osd.hide();
                }
                osd.expire();
            }
            while let Ok(event) = self.events.try_recv() {
                self.on_event(event);
            }
        }
        Ok(())
    }

    fn on_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Menu { x, y, snapshot } => {
                if let Some(osd) = &mut self.osd {
                    osd.hide();
                }
                let Some(tray) = &self.tray else {
                    return;
                };
                match tray.menu(&snapshot, x, y) {
                    Ok(Some(Choice::Open)) => self.routing.inbox.action(UserEvent::OpenUi),
                    Ok(Some(Choice::Exit)) => self.routing.inbox.stop(),
                    Ok(Some(Choice::Control(change))) => {
                        self.routing.inbox.action(UserEvent::Control {
                            expected: snapshot.revision,
                            change,
                        })
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("msilt-daemon: tray menu: {error}"),
                }
            }
            UiEvent::OpenUi => {
                let now = Instant::now();
                if self
                    .last_open_ui
                    .is_some_and(|prev| now.duration_since(prev) < Duration::from_millis(450))
                {
                    return;
                }
                self.last_open_ui = Some(now);
                if let Err(error) = launch_ui(&self.interactive) {
                    eprintln!("msilt-daemon: could not open Control Center: {error}");
                }
            }
            UiEvent::ShowOsd {
                title,
                detail,
                duration_ms,
                done,
            } => {
                if let Some(done) = done {
                    use msilt_core::ErrorCode;
                    let result = if !self.routing.inbox.can_work() {
                        Err(ErrorCode::NotReady)
                    } else {
                        self.osd
                            .get_or_insert_with(|| Osd::new(self.window, self.instance))
                            .show(title, &detail, duration_ms)
                            .map_err(|error| {
                                eprintln!("msilt-daemon: could not display OSD: {error}");
                                ErrorCode::BackendUnavailable
                            })
                    };
                    let _ = done.try_send(result);
                    return;
                }
                if let Some(osd) = &mut self.osd {
                    if let Err(error) = osd.show(title, &detail, duration_ms) {
                        eprintln!("msilt-daemon: could not display OSD: {error}");
                    }
                }
            }
            UiEvent::HideOsd => {
                if let Some(osd) = &mut self.osd {
                    osd.hide();
                }
            }
            UiEvent::RestoreTray => {
                if let Some(tray) = &mut self.tray {
                    tray.restore();
                }
            }
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        drop(self.effective.take());
        drop(self.osd.take());
        drop(self.tray.take());
        unsafe {
            if self.hotkey {
                let _ = UnregisterHotKey(Some(self.window), OPEN_HOTKEY);
            }
            if self.wts {
                let _ = WTSUnRegisterSessionNotification(self.window);
            }
            for registration in self.power.drain(..) {
                let _ = UnregisterPowerSettingNotification(registration);
            }
            let _ = DestroyWindow(self.window);
            let _ = UnregisterClassW(DESKTOP_CLASS, Some(self.instance));
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let creation = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, creation.lpCreateParams as isize);
        }
        return LRESULT(1);
    }
    let route = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Routing).as_ref() };
    let Some(route) = route else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_CLOSE => route.inbox.stop(),
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION if wparam.0 != 0 => route.inbox.stop(),
        END_UI => unsafe { PostQuitMessage(0) },
        WAKE_UI | WM_TIMER => {}
        WM_TIMECHANGE => route.inbox.flag(BITS_CLOCK),
        WM_DISPLAYCHANGE | WM_DEVICECHANGE => route.inbox.flag(BITS_DISPLAY),
        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_POWERSETTINGCHANGE if lparam.0 != 0 => {
                    let payload = lparam.0 as *const POWERBROADCAST_SETTING;
                    unsafe {
                        if (*payload).DataLength == 4 {
                            let value = ptr::addr_of!((*payload).Data)
                                .cast::<u32>()
                                .read_unaligned();
                            match (*payload).PowerSetting {
                                AC_DC => route.inbox.power_source(value),
                                BATTERY => route.inbox.battery(value),
                                SAVER => route.inbox.saver(value != 0),
                                DISPLAY => {
                                    route.hide.set(value == 0);
                                    route.inbox.display(value != 0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                PBT_APMSUSPEND => {
                    route.hide.set(true);
                    route.inbox.suspended(true);
                }
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                    route.inbox.suspended(false);
                    route.inbox.flag(BITS_POWER_STATUS);
                }
                PBT_APMPOWERSTATUSCHANGE => route.inbox.flag(BITS_POWER_STATUS),
                _ => {}
            }
            return LRESULT(1);
        }
        WM_WTSSESSION_CHANGE if lparam.0 as u32 == route.session => match wparam.0 as u32 {
            WTS_SESSION_LOCK => {
                route.hide.set(true);
                route.inbox.session(false);
            }
            WTS_SESSION_UNLOCK => route.inbox.session(true),
            WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOGOFF => {
                route.inbox.stop()
            }
            _ => {}
        },
        WM_HOTKEY if !route.headless && wparam.0 == OPEN_HOTKEY as usize => {
            route.inbox.action(UserEvent::OpenUi)
        }
        tray::TRAY_MESSAGE if !route.headless && ((lparam.0 as u32) >> 16) == tray::TRAY_ID => {
            match (lparam.0 as u32) & 0xffff {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    route.inbox.action(UserEvent::OpenUi)
                }
                WM_CONTEXTMENU => route.inbox.action(UserEvent::Menu {
                    x: wparam.0 as i16 as i32,
                    y: (wparam.0 >> 16) as i16 as i32,
                }),
                _ => {}
            }
        }
        _ if msg != 0 && msg == route.taskbar_created => route.inbox.flag(BITS_TRAY_RESTART),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;
    use crate::{iocp::Port, security::Security};

    #[test]
    fn hidden_message_pump_dispatches_clock_changes_and_quits() {
        let port = Port::new().unwrap();
        let (inbox, _actions) = Inbox::new(&port);
        let (ready, desktop) = sync_channel(1);
        let worker = {
            let inbox = inbox.clone();
            thread::spawn(move || {
                let security = Security::isolated().unwrap();
                let (mut desktop, ui) = Desktop::new(
                    inbox,
                    security.interactive_token.clone(),
                    security.session_id(),
                    true,
                )
                .unwrap();
                ready.send(ui).unwrap();
                desktop.run().unwrap();
            })
        };
        let ui = desktop.recv_timeout(Duration::from_secs(2)).unwrap();
        ui.post(WM_TIMECHANGE);
        ui.finish();
        worker.join().unwrap();
        assert_ne!(inbox.take().flags & BITS_CLOCK, 0);
    }
}
