//! MSI-Toolkit Fluent 2 shell.
//!
//! Renderer: Slint 1.19 with `renderer-software` + `backend-winit` (measured on
//! the reference machine at 12.6 MiB private / ~118 ms first frame / 0 GPU
//! context / 0 dGPU wake). Widget style: Fluent 2 from `std-widgets.slint`.
//!
//! Ownership: the UI never touches the EC, never creates an OSD/crosshair
//! window, and never polls IPC. Every hardware intent becomes one `Control`
//! command carrying the current `expected_revision`.
//!
//! Flags:
//!   `--demo`          load a read-only snapshot of the MS-17S3 factory values
//!   `--check-layout`  headless self-check of the geometry + editor invariants

#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

mod demo;
mod dwm;
mod editor;
mod ipc_client;
mod subscriptions;
mod options;
mod dump;
mod updater;
mod view;

use std::cell::RefCell;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use msilt_core::{
    AutomationPreset, AutomationRuleView, ControlChange, FanChannel, IpcCommand, IpcEvent,
    IpcMessage, IpcReply, IpcResult, OemAction, OemInventory, OemPackageId, OemScope,
    StartupStatus, SystemSnapshot, TelemetrySample, FAN_POINT_COUNT, KEYBOARD_ZONE_COUNT,
};
use slint::{Color, SharedString, Timer, TimerMode};

slint::include_modules!();

use editor::{CurveEditor, ZoneEditor};
use ipc_client::{Endpoint, Incoming, IpcClient};
use subscriptions::SubscriptionPlan;
use options::Options;
use view::{History, Notice};

/// Command codes. **Must stay in sync with the `AppState.command` calls in
/// `ui/**/*.slint`.**
mod cmd {
    pub const PAGE: i32 = 10;
    pub const NAV_TOGGLE: i32 = 11;
    pub const MODE: i32 = 20;
    pub const COOLER: i32 = 21;
    pub const CHARGE: i32 = 22;
    pub const REFRESH: i32 = 23;
    pub const BRIGHTNESS: i32 = 24;
    pub const COLOR: i32 = 25;
    pub const CURVE_APPLY: i32 = 31;
    pub const CURVE_RESET: i32 = 32;
    pub const CURVE_PRESET: i32 = 33;
    pub const ZONE_APPLY: i32 = 41;
    pub const ZONE_PRESET: i32 = 42;
    pub const ZONE_SELECT: i32 = 43;
    pub const ZONE_BRIGHTNESS: i32 = 44;
    pub const ZONE_COLOR: i32 = 45;
    pub const RULE_TOGGLE: i32 = 50;
    pub const DRY_RUN: i32 = 51;
    pub const ENGINE: i32 = 52;
    pub const RULE_ADD_PRESET: i32 = 53;
    pub const RULE_DELETE: i32 = 54;
    pub const RELEASE_OVERRIDES: i32 = 55;
    pub const RECONNECT: i32 = 60;
    pub const BANNER: i32 = 61;
    pub const TOAST_DISMISS: i32 = 62;
    pub const OPEN_SETTINGS: i32 = 63;
    pub const MONITOR_PAUSE: i32 = 70;
    pub const MONITOR_WINDOW: i32 = 71;
    pub const EXPORT: i32 = 80;
    pub const THEME: i32 = 82;
    pub const STARTUP: i32 = 83;
    pub const OEM_SCOPE: i32 = 84;
    pub const OEM_DISABLE_AUTO: i32 = 85;
    pub const OEM_STOP_NOW: i32 = 86;
    pub const OEM_DISABLE_AND_STOP: i32 = 87;
    pub const OEM_RESTORE: i32 = 88;
    pub const OEM_UNINSTALL_ASK: i32 = 89;
    pub const GPU_TELEMETRY: i32 = 90;
    pub const OSD_ENABLE: i32 = 91;
    pub const OSD_DURATION: i32 = 92;
    pub const OEM_UNINSTALL_CONFIRM: i32 = 93;
    pub const OEM_REFRESH: i32 = 94;
    pub const TEST_OSD: i32 = 95;
    pub const UPDATE_CHECK: i32 = 96;
    pub const UPDATE_INSTALL: i32 = 97;
    pub const UPDATE_OPEN_PAGE: i32 = 98;
    pub const UPDATE_AUTO_CHECK: i32 = 99;
    pub const OPEN_CONFIG_DIR: i32 = 102;
    pub const LAUNCH_UNINSTALL: i32 = 103;
}

const TOAST_TTL: Duration = Duration::from_millis(3_200);
const MIN_WINDOW: (f32, f32) = (820.0, 580.0);

/// Process start, used for the first-frame measurement (the software renderer
/// has no `RenderingSetup` hook, so "frame is up" is observed from the DWM
/// callback that runs right after `show`).
static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn elapsed_ms() -> i32 {
    PROCESS_START
        .get()
        .map(|start| start.elapsed().as_millis().min(i32::MAX as u128) as i32)
        .unwrap_or(-1)
}

// ── application state ───────────────────────────────────────────────────────

struct App {
    ui: slint::Weak<AppWindow>,
    snapshot: SystemSnapshot,
    curves: [CurveEditor; 2],
    zones: ZoneEditor,
    history: History,
    demo: bool,
    ipc: Option<IpcClient>,
    endpoint: Endpoint,
    connected: bool,
    dark: bool,
    started: Instant,
    report: dwm::DwmReport,
    toast_visible: bool,
    toast_seq: u64,
    toast_notice: Notice,
    app_installed: bool,
    demo_tick: u64,
    subscriptions: SubscriptionPlan,
    pending_control: Option<(u64, ControlChange)>,
    config: Option<(u64, msilt_core::AppConfig)>,
    config_pending: Option<u64>,
    theme_override: Option<bool>,
    first_frame_ms: i32,
    startup: StartupStatus,
    oem: OemInventory,
    oem_busy: bool,
    pending_tools_notice: Option<&'static str>,
    pending_releases: Vec<msilt_core::ControlTarget>,
    auto_rules: Vec<AutomationRuleView>,
    auto_summary: String,
    update_state: i32,
    update_status_text: String,
    update_release: Option<updater::ReleaseInfo>,
    startup_update_checked: bool,
    headless: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn take_app() -> Option<App> {
    APP.with(|cell| cell.borrow_mut().take())
}

fn put_app(app: App) {
    APP.with(|cell| *cell.borrow_mut() = Some(app));
}

fn with_app(mut f: impl FnMut(&mut App)) {
    let Some(mut app) = take_app() else {
        return;
    };
    f(&mut app);
    put_app(app);
}

fn curves_from(snapshot: &SystemSnapshot) -> [CurveEditor; 2] {
    let make = |channel: FanChannel, state: Option<&msilt_core::FanState>| match state {
        Some(state) => CurveEditor::new(channel, state),
        None => CurveEditor::placeholder(channel),
    };
    [
        make(FanChannel::Cpu, snapshot.hardware.cpu_fan.as_ref()),
        make(FanChannel::Gpu, snapshot.hardware.gpu_fan.as_ref()),
    ]
}

impl App {
    fn push_toast(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let state = ui.global::<AppState>();
        state.set_toast_level(self.toast_notice.level);
        state.set_toast_title(self.toast_notice.title.clone().into());
        state.set_toast_text(self.toast_notice.text.clone().into());
        state.set_toast_visible(self.toast_visible);
    }

    fn push_system_metrics(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let state = ui.global::<AppState>();
        state.set_session_seconds(self.started.elapsed().as_secs().min(i32::MAX as u64) as i32);
        state.set_private_kib(dwm::private_kib().map_or(-1, |kib| kib as i32));
        state.set_app_installed(if self.demo { true } else { self.app_installed });
    }

    /// Pushes the view model into Slint globals (used on state/config/tools changes,
    /// NEVER on 1 Hz telemetry ticks or toast visibility toggles).
    fn push_all(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let state = ui.global::<AppState>();
        view::apply_snapshot(&state, &self.snapshot, self.demo);
        view::apply_curves(&state, &self.curves);
        view::apply_curve_flags(&state, &self.curves, self.curve_channel());
        view::apply_zones(&state, &self.zones, self.dark);
        if state.get_page() == 6 {
            view::apply_history(&state, &self.history);
        } else if state.get_page() == 0 {
            view::apply_history_paths(&state, &self.history);
        }
        view::apply_system_tools(&state, &self.startup, &self.oem);
        view::apply_automation_rules(&state, &self.auto_rules, &self.auto_summary);
        state.set_oem_busy(self.oem_busy);
        state.set_dark(self.dark);
        if let Some((_, config)) = self.config {
            state.set_theme_choice(match config.theme {
                msilt_core::Theme::System => 0,
                msilt_core::Theme::Dark => 1,
                msilt_core::Theme::Light => 2,
            });
        } else {
            state.set_theme_choice(match self.theme_override {
                None => 0,
                Some(true) => 1,
                Some(false) => 2,
            });
        }

        state.set_connection(if self.demo {
            3
        } else if self.connected {
            1
        } else if self.ipc.is_some() {
            0
        } else {
            2
        });
        state.set_connection_text(
            if self.demo {
                "演示模式"
            } else if self.connected {
                "服务已连接"
            } else if self.ipc.is_some() {
                "正在连接…"
            } else {
                "服务未运行"
            }
            .into(),
        );
        state.set_demo(self.demo);
        state.set_first_frame_ms(self.first_frame_ms);
        state.set_curve_apply_pending(self.pending_control.is_some());
        state.set_request_pending(self.pending_control.is_some() || self.config_pending.is_some());
        state.set_config_known(self.config.is_some());
        if let Some((_, config)) = self.config {
            state.set_automation_enabled(config.automation.enabled);
            state.set_osd_enabled(config.osd.enabled);
            state.set_osd_duration(i32::from(config.osd.duration_ms));
            state.set_update_auto_check(config.updates.auto_check);
            state.set_override_policy(match config.automation.manual_override {
                msilt_core::ManualOverride::UntilReleased => "手动恢复前保持".into(),
                msilt_core::ManualOverride::Timed { seconds } => format!("{seconds} 秒后恢复自动").into(),
            });
        }
        state.set_app_installed(if self.demo { true } else { self.app_installed });
        state.set_diagnostic(self.endpoint.diagnostic);
        state.set_render_backend("纯 CPU 软件渲染".into());
        if state.get_page() == 7 {
            state.set_session_seconds(self.started.elapsed().as_secs().min(i32::MAX as u64) as i32);
            state.set_private_kib(dwm::private_kib().map_or(-1, |kib| kib as i32));
        }
        state.set_diagnostics_hint(self.report.summary().into());
        state.set_update_state(self.update_state);
        state.set_update_latest(
            self.update_release
                .as_ref()
                .map(|r| r.tag.clone())
                .unwrap_or_default()
                .into(),
        );
        state.set_update_status_text(self.update_status_text.clone().into());

        // Persistent banner ONLY for actionable blockers or new-version notice (never transient spam).
        let installed = !self.demo && self.app_installed;
        let notice = view::notice(&self.snapshot, self.connected, self.demo, installed)
            .or_else(|| {
                (self.update_state == 3).then(|| {
                    let tag = self
                        .update_release
                        .as_ref()
                        .map(|r| r.tag.as_str())
                        .unwrap_or("新版本");
                    Notice {
                        level: 4,
                        title: format!("发现新版本 {tag}"),
                        text: "可在「系统与工具」中一键升级。".into(),
                        action: "前往更新".into(),
                    }
                })
            });
        match notice {
            Some(notice) => {
                state.set_banner_visible(true);
                state.set_banner_level(notice.level);
                state.set_banner_title(notice.title.into());
                state.set_banner_text(notice.text.into());
                state.set_banner_action(notice.action.into());
            }
            None => {
                state.set_banner_visible(false);
                state.set_banner_title("".into());
                state.set_banner_text("".into());
                state.set_banner_action("".into());
            }
        }

        // Sync floating toast state as well.
        self.push_toast();
    }

    fn curve_channel(&self) -> usize {
        self.ui
            .upgrade()
            .map(|ui| ui.global::<AppState>().get_curve_channel().clamp(0, 1) as usize)
            .unwrap_or(0)
    }

    fn transient(&mut self, level: i32, title: &str, text: String) {
        self.toast_seq = self.toast_seq.wrapping_add(1);
        let seq = self.toast_seq;
        self.toast_notice = Notice {
            level,
            title: title.to_string(),
            text,
            action: String::new(),
        };
        self.toast_visible = true;
        self.push_toast();
        Timer::single_shot(TOAST_TTL, move || {
            with_app(|app| {
                if app.toast_seq == seq && app.toast_visible {
                    // Keep title/text intact while fading out smoothly.
                    app.toast_visible = false;
                    app.push_toast();
                }
            });
        });
    }

    fn dismiss_toast(&mut self) {
        self.toast_seq = self.toast_seq.wrapping_add(1);
        if self.toast_visible {
            self.toast_visible = false;
            self.push_toast();
        }
    }

    fn apply_snapshot(&mut self, snapshot: SystemSnapshot) {
        if snapshot.revision < self.snapshot.revision {
            return;
        }
        let keep_preview = self.curves.iter().any(CurveEditor::is_dirty);
        self.snapshot = snapshot;
        if !keep_preview {
            self.curves = curves_from(&self.snapshot);
        }
        self.connected = true;
    }

    fn apply_telemetry(&mut self, sample: &TelemetrySample) {
        self.history.push(sample);
        let age = unix_now_ms().saturating_sub(sample.sampled_at_unix_ms);
        if let Some(ui) = self.ui.upgrade() {
            let state = ui.global::<AppState>();
            view::apply_telemetry(&state, sample, age);
            match state.get_page() {
                0 => view::apply_history_paths(&state, &self.history),
                6 => view::apply_history(&state, &self.history),
                _ => {}
            }
        }
    }

    fn on_disconnect(&mut self, reason: String) {
        self.connected = false;
        self.subscriptions = SubscriptionPlan::default();
        self.pending_control = None;
        self.config_pending = None;
        self.pending_tools_notice = None;
        self.pending_releases.clear();
        self.oem_busy = false;
        self.config = None;
        self.ipc = None;
        self.transient(2, "后台服务已断开", reason);
    }

    fn sync_subscriptions(&mut self) {
        if !self.connected || self.demo { return; }
        let Some(ui) = self.ui.upgrade() else { return; };
        let state = ui.global::<AppState>();
        let desired = subscriptions::for_page(state.get_page(), state.get_monitor_paused());
        if let Some(topics) = self.subscriptions.next(desired) {
            if let Some(ipc) = self.ipc.as_ref() {
                if let Err(error) = ipc.send(IpcCommand::SetSubscriptions { topics }) {
                    self.on_disconnect(format!("连接中断：{error}"));
                }
            }
        }
    }

    fn submit(&mut self, change: ControlChange) {
        if self.demo {
            self.transient(4, "演示模式", "当前为演示模式，不会改动本机硬件。".into());
            return;
        }
        if self.endpoint.diagnostic || self.pending_control.is_some() || !self.connected {
            return;
        }
        if !view::can_submit(&self.snapshot, &change) {
            self.transient(1, "暂不支持此选项", "当前设备或状态无法应用该设置。".into());
            return;
        }
        let Some(ipc) = self.ipc.as_ref() else {
            self.transient(2, "后台服务未连接", "请先启动或重新连接后台服务。".into());
            return;
        };
        let command = IpcCommand::Control {
            expected_revision: self.snapshot.revision,
            change: change.clone(),
        };
        match ipc.send(command) {
            Ok(id) => {
                self.pending_control = Some((id, change));
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_curve_apply_pending(true);
                    state.set_request_pending(true);
                }
            }
            Err(error) => self.on_disconnect(format!("发送失败：{error}")),
        }
    }

    fn on_command(&mut self, kind: i32, a: i32, b: i32) {
        match kind {
            cmd::PAGE => {
                let target = a.clamp(0, 7);
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_page(target);
                    if target == 0 {
                        view::apply_history_paths(&state, &self.history);
                    } else if target == 6 {
                        view::apply_history(&state, &self.history);
                    } else if target == 7 {
                        self.push_system_metrics();
                    }
                }
                self.sync_subscriptions();
                return;
            }
            cmd::NAV_TOGGLE => {
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_nav_collapsed(!state.get_nav_collapsed());
                }
                return;
            }
            cmd::CURVE_RESET => {
                if let Some(editor) = self.curves.get_mut(a.clamp(0, 1) as usize) {
                    editor.reset();
                }
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    view::apply_curves(&state, &self.curves);
                    view::apply_curve_flags(&state, &self.curves, self.curve_channel());
                }
                return;
            }
            cmd::CURVE_PRESET => {
                if let Some(editor) = self.curves.get_mut(a.clamp(0, 1) as usize) {
                    editor.preset(b.clamp(0, 2) as u8);
                }
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    view::apply_curves(&state, &self.curves);
                    view::apply_curve_flags(&state, &self.curves, self.curve_channel());
                }
                return;
            }
            cmd::CURVE_APPLY => {
                let channel = a.clamp(0, 1) as usize;
                let curve = self.curves[channel].curve();
                let fan_channel = if channel == 0 { FanChannel::Cpu } else { FanChannel::Gpu };
                self.submit(ControlChange::SetFanCurve { channel: fan_channel, curve });
                return;
            }
            cmd::ZONE_PRESET => {
                self.zones.apply_preset(b.clamp(0, 3) as usize);
                if let Some(ui) = self.ui.upgrade() {
                    view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
                }
                return;
            }
            cmd::ZONE_SELECT => {
                if a == 1 {
                    self.zones.select_all();
                } else {
                    self.zones.select_none();
                }
                if let Some(ui) = self.ui.upgrade() {
                    view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
                }
                return;
            }
            cmd::ZONE_BRIGHTNESS => {
                self.zones.set_brightness(a);
                if let Some(ui) = self.ui.upgrade() {
                    view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
                }
                return;
            }
            cmd::ZONE_APPLY => {
                let zones = self.zones.scaled();
                self.submit(ControlChange::SetKeyboardLighting { zones });
                if self.demo {
                    if let Some(ui) = self.ui.upgrade() {
                        ui.global::<AppState>().set_keyboard_submit("演示模式".into());
                    }
                }
                return;
            }
            cmd::MODE => {
                if let Some(mode) = self.snapshot.hardware.supported_performance_modes.get(a as usize).copied() {
                    self.submit(ControlChange::SetPerformanceMode { mode });
                    return;
                }
            }
            cmd::COOLER => {
                self.submit(ControlChange::SetCoolerBoost { enabled: a != 0 });
                return;
            }
            cmd::CHARGE => {
                if let Some(percent) = self.snapshot.hardware.supported_charge_limits.get(a as usize).copied() {
                    self.submit(ControlChange::SetChargeLimit { percent });
                }
                return;
            }
            cmd::REFRESH => {
                if let Some(hz) = self.snapshot.hardware.supported_refresh_rates.get(a as usize).copied() {
                    self.submit(ControlChange::SetDisplayRefreshRate { hz });
                }
                return;
            }
            cmd::BRIGHTNESS => {
                self.submit(ControlChange::SetDisplayBrightness { percent: a.clamp(0, 100) as u8 });
                return;
            }
            cmd::OPEN_SETTINGS => {
                dwm::open_power_settings();
                return;
            }
            cmd::RECONNECT => {
                self.transient(4, "正在重新连接", "正在连接后台服务…".into());
                self.ipc = None;
                self.connected = false;
                self.subscriptions = SubscriptionPlan::default();
                self.pending_control = None;
                schedule_connect(self.ui.clone(), 0);
                return;
            }
            cmd::TOAST_DISMISS => {
                self.dismiss_toast();
                return;
            }
            cmd::MONITOR_PAUSE => {
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_monitor_paused(!state.get_monitor_paused());
                }
                self.sync_subscriptions();
                return;
            }
            cmd::MONITOR_WINDOW => {
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_monitor_window(a.clamp(1, 15));
                    view::apply_history(&state, &self.history);
                }
                return;
            }
            cmd::GPU_TELEMETRY => {
                if let Some(ui) = self.ui.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_gpu_telemetry(!state.get_gpu_telemetry());
                }
                self.transient(
                    4,
                    "低功耗监控模式",
                    "GPU 温度通过系统接口读取，不会额外唤醒独立显卡。".into(),
                );
                return;
            }
            cmd::OSD_ENABLE => {
                self.update_config(|config| config.osd.enabled = a != 0);
                return;
            }
            cmd::OSD_DURATION => {
                self.update_config(|config| config.osd.duration_ms = a.clamp(800, 5000) as u16);
                return;
            }
            cmd::ENGINE => { self.update_config(|config| config.automation.enabled = a != 0); return; }
            cmd::THEME => {
                let target = match a {
                    1 => msilt_core::Theme::Dark,
                    2 => msilt_core::Theme::Light,
                    _ => msilt_core::Theme::System,
                };
                self.theme_override = match target {
                    msilt_core::Theme::System => None,
                    msilt_core::Theme::Dark => Some(true),
                    msilt_core::Theme::Light => Some(false),
                };
                self.dark = match target {
                    msilt_core::Theme::Dark => true,
                    msilt_core::Theme::Light => false,
                    msilt_core::Theme::System => dwm::registry_prefers_dark().unwrap_or(true),
                };
                if let Some(ui) = self.ui.upgrade() {
                    let theme = ui.global::<Theme>();
                    theme.set_dark(self.dark);
                    theme.invoke_apply();
                    if let Some(hwnd) = dwm::hwnd_of(ui.window()) {
                        dwm::retheme(hwnd, self.dark);
                    }
                }
                if let Some((_, config)) = self.config.as_mut() {
                    config.theme = target;
                }
                if !self.demo {
                    self.update_config(|config| config.theme = target);
                    return;
                }
            }
            cmd::STARTUP => {
                let enabled = a != 0;
                if self.demo {
                    self.startup.enabled = enabled;
                    self.transient(4, "演示模式", format!("开机自启预览已切换为{}。", if enabled { "开启" } else { "关闭" }));
                } else {
                    self.oem_busy = true;
                    self.pending_tools_notice = Some(if enabled { "已开启 MSI-Toolkit 开机自启" } else { "已关闭 MSI-Toolkit 开机自启" });
                    self.send_simple(IpcCommand::SetStartup { enabled });
                }
            }
            cmd::OEM_SCOPE => {
                if let Some(ui) = self.ui.upgrade() {
                    ui.global::<AppState>().set_oem_scope(a.clamp(0, 3));
                }
                return;
            }
            cmd::OEM_DISABLE_AUTO | cmd::OEM_STOP_NOW | cmd::OEM_DISABLE_AND_STOP | cmd::OEM_RESTORE => {
                let scope = scope_from(a);
                let (action, notice) = match kind {
                    cmd::OEM_DISABLE_AUTO => (OemAction::DisableAutoStart { scope }, "已关闭所选 MSI 官方软件的开机自启（已保存可逆备份）"),
                    cmd::OEM_STOP_NOW => (OemAction::StopNow { scope }, "已停止所选 MSI 官方软件后台服务与进程"),
                    cmd::OEM_DISABLE_AND_STOP => (OemAction::DisableAndStop { scope }, "已禁启并停止所选 MSI 官方软件，底层驱动保持完好"),
                    _ => (OemAction::RestoreBackup {}, "已从可逆备份恢复 MSI 官方软件原始启动状态"),
                };
                if self.demo {
                    self.transient(4, "演示模式", "演示模式下不会改动本机 MSI 软件。".into());
                } else {
                    self.oem_busy = true;
                    self.pending_tools_notice = Some(notice);
                    self.send_simple(IpcCommand::ManageOem { action });
                }
            }
            cmd::OEM_UNINSTALL_ASK => {
                if let Some(ui) = self.ui.upgrade() {
                    ui.global::<AppState>().set_oem_confirm_uninstall(a);
                }
                return;
            }
            cmd::OEM_UNINSTALL_CONFIRM => {
                if let Some(ui) = self.ui.upgrade() {
                    ui.global::<AppState>().set_oem_confirm_uninstall(-1);
                }
                let Some(package) = package_from(a) else { return; };
                if self.demo {
                    self.transient(4, "演示模式", "演示模式下不会卸载本机软件。".into());
                } else {
                    self.oem_busy = true;
                    self.pending_tools_notice = Some("卸载已完成，底层硬件驱动保持完好");
                    self.transient(4, "正在卸载", "正在卸载所选 MSI 软件，底层硬件驱动将保持完好。".into());
                    self.send_simple(IpcCommand::ManageOem {
                        action: OemAction::UninstallPackage { package },
                    });
                }
            }
            cmd::OEM_REFRESH => {
                if !self.demo {
                    self.oem_busy = true;
                    self.send_simple(IpcCommand::GetSystemTools {});
                } else {
                    self.transient(4, "已刷新", String::new());
                }
            }
            cmd::RULE_TOGGLE => {
                let Some(rule) = self.auto_rules.get_mut(a as usize) else { return; };
                let enabled = b != 0;
                if self.demo {
                    rule.enabled = enabled;
                    rule.diagnostic = if enabled { "已启用（演示）" } else { "已禁用" }.into();
                } else {
                    let id = rule.id.clone();
                    self.send_simple(IpcCommand::SetAutomationRuleEnabled { id, enabled });
                }
            }
            cmd::RULE_ADD_PRESET => {
                let Some(preset) = preset_from(a) else { return; };
                if self.demo {
                    let (id, name, trigger, actions) = demo_preset_meta(preset);
                    if !self.auto_rules.iter().any(|r| r.id == id) {
                        self.auto_rules.push(AutomationRuleView {
                            id: id.into(),
                            name: name.into(),
                            enabled: true,
                            priority: 500,
                            trigger: trigger.into(),
                            actions: actions.into(),
                            diagnostic: "已添加到演示列表".into(),
                            active: false,
                        });
                    }
                } else {
                    self.send_simple(IpcCommand::AddAutomationPreset { preset });
                }
            }
            cmd::RULE_DELETE => {
                let Some(rule) = self.auto_rules.get(a as usize) else { return; };
                if self.demo {
                    self.auto_rules.remove(a as usize);
                } else {
                    let id = rule.id.clone();
                    self.send_simple(IpcCommand::DeleteAutomationRule { id });
                }
            }
            cmd::DRY_RUN => {
                if self.demo {
                    if let Some(ui) = self.ui.upgrade() {
                        ui.global::<AppState>().set_dry_run_result("测试完成：当前已接电源，命中 1 条规则，无冲突。".into());
                    }
                    self.transient(3, "模拟测试完成", "当前规则配置正常，无冲突。".into());
                } else {
                    self.send_simple(IpcCommand::DryRunAutomation {});
                }
            }
            cmd::RELEASE_OVERRIDES => {
                if self.demo {
                    self.snapshot.manual_overrides.clear();
                    self.transient(3, "已恢复自动切换", String::new());
                } else if !self.snapshot.manual_overrides.is_empty() {
                    self.pending_releases = self.snapshot.manual_overrides.clone();
                    if let Some(target) = self.pending_releases.pop() {
                        let expected_revision = self.snapshot.revision;
                        self.send_simple(IpcCommand::ReleaseManualOverride { expected_revision, target });
                    }
                }
            }
            cmd::TEST_OSD => {
                if self.demo {
                    self.transient(4, "演示模式", "请连接后台服务以预览屏幕悬浮提示。".into());
                } else {
                    self.send_simple(IpcCommand::TestOsd {});
                }
            }
            cmd::UPDATE_CHECK => {
                self.start_update_check(false);
                return;
            }
            cmd::UPDATE_INSTALL => {
                self.start_update_install();
                return;
            }
            cmd::UPDATE_OPEN_PAGE => {
                let url = self
                    .update_release
                    .as_ref()
                    .map(|r| r.html_url.as_str())
                    .unwrap_or(updater::RELEASES_PAGE_URL);
                updater::open_release_page(url);
                return;
            }
            cmd::UPDATE_AUTO_CHECK => {
                let enabled = a != 0;
                if self.demo {
                    if let Some((_, config)) = self.config.as_mut() {
                        config.updates.auto_check = enabled;
                    }
                } else {
                    self.update_config(|config| config.updates.auto_check = enabled);
                    return;
                }
            }
            cmd::OPEN_CONFIG_DIR => {
                if let Err(err) = updater::open_config_dir() {
                    self.transient(1, "无法打开配置目录", err);
                }
            }
            cmd::LAUNCH_UNINSTALL => {
                if self.demo {
                    self.transient(4, "演示模式", "演示模式下不会启动卸载程序。".into());
                } else if let Err(err) = updater::launch_uninstaller() {
                    self.transient(1, "无法启动卸载程序", err);
                }
            }
            cmd::EXPORT => {
                self.transient(4, "诊断摘要", self.report.summary());
                return;
            }
            cmd::BANNER => {
                if !self.connected && !self.demo {
                    self.app_installed = updater::is_installed();
                    if self.app_installed {
                        match updater::start_installed_daemon() {
                            Ok(()) => {
                                self.transient(4, "正在启动服务", "正在建立连接…".into());
                                self.ipc = None;
                                self.connected = false;
                                self.subscriptions = SubscriptionPlan::default();
                                self.pending_control = None;
                                schedule_connect(self.ui.clone(), 1);
                            }
                            Err(err) => {
                                self.transient(1, "启动服务失败", err);
                            }
                        }
                        self.push_all();
                    } else {
                        self.on_command(cmd::RECONNECT, 0, 0);
                    }
                } else if let Some(ui) = self.ui.upgrade() {
                    ui.global::<AppState>().set_page(7);
                    self.push_system_metrics();
                    self.sync_subscriptions();
                }
                return;
            }
            _ => return,
        }
        self.push_all();
    }

    fn start_update_check(&mut self, silent: bool) {
        if self.update_state == 1 || self.update_state == 4 {
            return;
        }
        if self.demo {
            let current = env!("CARGO_PKG_VERSION");
            self.update_state = 2;
            self.update_status_text = format!("当前 v{current} · 演示模式不访问网络");
            self.transient(4, "演示模式", format!("当前版本 v{current}（演示模式下不连接 GitHub）。"));
            self.push_all();
            return;
        }
        self.update_state = 1;
        if !silent {
            self.update_status_text = format!("当前 v{} · 正在检查 GitHub Releases…", env!("CARGO_PKG_VERSION"));
        }
        self.push_all();

        let current = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            let outcome = updater::check_latest(&current);
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| match &outcome {
                    updater::CheckOutcome::Available(info) => {
                        app.update_state = 3;
                        app.update_status_text = if info.notes_summary.is_empty() {
                            format!("当前 v{current} → 发现新版本 {}", info.tag)
                        } else {
                            format!("v{current} → {} · {}", info.tag, info.notes_summary)
                        };
                        app.update_release = Some(info.clone());
                        if !silent {
                            app.transient(
                                4,
                                "发现新版本",
                                format!("MSI-Toolkit {} 已发布，点击「立即更新」校验并一键升级。", info.tag),
                            );
                        }
                        app.push_all();
                    }
                    updater::CheckOutcome::UpToDate { latest, .. } => {
                        app.update_state = 2;
                        app.update_release = None;
                        app.update_status_text = format!("当前 v{current} · 已是最新（v{latest}）");
                        if !silent {
                            app.transient(3, "已是最新版本", format!("当前 MSI-Toolkit v{current} 已是最新发行版。"));
                        }
                        app.push_all();
                    }
                    updater::CheckOutcome::NoReleaseYet { .. } => {
                        app.update_state = 2;
                        app.update_release = None;
                        app.update_status_text = format!("当前 v{current} · 远端尚未发布新版本");
                        if !silent {
                            app.transient(4, "暂无更新", "GitHub 仓库尚未发布更新版本。".into());
                        }
                        app.push_all();
                    }
                    updater::CheckOutcome::Failed(reason) => {
                        app.update_state = if silent { 0 } else { 5 };
                        app.update_status_text = if silent {
                            format!("当前 v{current} · GitHub Releases")
                        } else {
                            format!("检查失败：{reason}")
                        };
                        if !silent {
                            app.transient(1, "检查更新失败", reason.clone());
                        }
                        app.push_all();
                    }
                });
            });
        });
    }

    fn start_update_install(&mut self) {
        if self.demo || self.update_state == 4 {
            return;
        }
        let Some(info) = self.update_release.clone() else {
            self.start_update_check(false);
            return;
        };
        self.update_state = 4;
        self.update_status_text = format!("正在下载 {} 并校验 SHA-256…", info.tag);
        self.transient(4, "开始下载更新", format!("正在下载 {} 安装包并验证完整性…", info.tag));
        self.push_all();

        std::thread::spawn(move || {
            let outcome = updater::download_and_launch_update(&info, |stage_text| {
                let text = stage_text.to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| {
                        if app.update_state == 4 {
                            app.update_status_text = text.clone();
                            app.push_all();
                        }
                    });
                });
            });
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| match &outcome {
                    updater::InstallOutcome::Launched { tag } => {
                        app.update_state = 4;
                        app.update_status_text = format!("{tag} 校验通过 · 已启动升级安装程序");
                        app.transient(
                            3,
                            "校验通过，正在升级",
                            format!("请在弹出的管理员授权窗口确认，安装程序将自动替换并重启至 {tag}。"),
                        );
                        app.push_all();
                    }
                    updater::InstallOutcome::Failed(reason) => {
                        app.update_state = 3;
                        app.update_status_text = format!("更新失败：{reason}");
                        app.transient(2, "自动更新失败", reason.clone());
                        app.push_all();
                    }
                });
            });
        });
    }

    fn send_simple(&mut self, command: IpcCommand) {
        if !self.connected {
            self.oem_busy = false;
            self.pending_tools_notice = None;
            self.transient(2, "后台服务未连接", "请先启动或重新连接后台服务。".into());
            return;
        }
        let Some(ipc) = self.ipc.as_ref() else {
            self.oem_busy = false;
            self.pending_tools_notice = None;
            return;
        };
        if let Err(error) = ipc.send(command) {
            self.oem_busy = false;
            self.pending_tools_notice = None;
            self.on_disconnect(format!("发送失败：{error}"));
        }
    }

    fn update_config(&mut self, edit: impl FnOnce(&mut msilt_core::AppConfig)) {
        if self.demo || self.endpoint.diagnostic || self.config_pending.is_some() { return; }
        let Some((revision, mut config)) = self.config else { return; };
        let previous = config;
        edit(&mut config);
        if previous == config || config.validate().is_err() { return; }
        if let Some(ipc) = self.ipc.as_ref() {
            match ipc.send(IpcCommand::SetConfig { expected_revision: revision, config }) {
                Ok(id) => self.config_pending = Some(id),
                Err(error) => self.on_disconnect(error.to_string()),
            }
        }
        self.push_all();
    }

    fn on_command_text(&mut self, kind: i32, value: SharedString) {
        if kind == cmd::COLOR {
            let profile = value.to_string();
            if profile == "—" || profile.is_empty() {
                return;
            }
            self.submit(ControlChange::SetColorProfile { profile_id: profile });
            return;
        } else if kind == cmd::ZONE_COLOR {
            if let Some(rgb) = editor::parse_hex_rgb(value.as_str()) {
                self.zones.paint(rgb);
                if let Some(ui) = self.ui.upgrade() {
                    view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
                }
            } else {
                self.transient(1, "颜色格式无效", "请输入 6 位十六进制颜色，例如 #38BDF8。".into());
            }
            return;
        }
        self.push_all();
    }

    fn on_curve_edit(&mut self, channel: i32, index: i32, duty: i32) {
        let channel = channel.clamp(0, 1) as usize;
        if index < 0 || index as usize >= FAN_POINT_COUNT {
            return;
        }
        let changed = self.curves[channel].set_duty(index as usize, duty);
        if changed {
            self.curves[channel].clear_hint();
        }
        if let Some(ui) = self.ui.upgrade() {
            let state = ui.global::<AppState>();
            state.set_curve_channel(channel as i32);
            view::apply_curves(&state, &self.curves);
            view::apply_curve_flags(&state, &self.curves, channel);
        }
    }

    fn on_zone_edit(&mut self, index: i32, additive: bool) {
        if index < 0 || index as usize >= KEYBOARD_ZONE_COUNT {
            return;
        }
        self.zones.toggle(index as usize, additive);
        if let Some(ui) = self.ui.upgrade() {
            view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
        }
    }

    fn on_zone_hsv(&mut self, h: f32, s: f32, v: f32) {
        self.zones.paint_hsv(h, s, v);
        if let Some(ui) = self.ui.upgrade() {
            view::apply_zones(&ui.global::<AppState>(), &self.zones, self.dark);
        }
    }

    fn on_response(&mut self, id: u64, reply: IpcResult) {
        let completed_change = if self.pending_control.as_ref().is_some_and(|(pending, _)| *pending == id) {
            self.pending_control.take().map(|(_, change)| change)
        } else {
            None
        };
        let control = completed_change.is_some();
        if self.config_pending == Some(id) { self.config_pending = None; }
        match reply {
            IpcResult::Ok { reply } => match reply {
                IpcReply::Hello { snapshot, version } => {
                    if version != msilt_core::IPC_VERSION {
                        self.on_disconnect("版本不兼容，请重启或重新安装".into());
                        return;
                    }
                    // Revision belongs to a daemon generation, not the lifetime of the UI.
                    self.snapshot = snapshot;
                    self.curves = curves_from(&self.snapshot);
                    self.history = History::default();
                    self.connected = true;
                    self.sync_subscriptions();
                    if let Some(ipc) = self.ipc.as_ref() {
                        let _ = ipc.send(IpcCommand::GetConfig {});
                        let _ = ipc.send(IpcCommand::GetSystemTools {});
                        let _ = ipc.send(IpcCommand::GetAutomationRules {});
                    }
                }
                IpcReply::Subscriptions { snapshot, topics } => {
                    if !self.subscriptions.acknowledge(topics) {
                        self.on_disconnect("连接状态异常，请重新连接".into());
                        return;
                    }
                    self.apply_snapshot(snapshot);
                    self.sync_subscriptions();
                }
                IpcReply::Snapshot { snapshot } | IpcReply::Applied { snapshot } => {
                    if control {
                        self.curves = curves_from(&snapshot);
                    }
                    self.apply_snapshot(snapshot);
                    match completed_change {
                        Some(ControlChange::SetFanCurve { channel, .. }) => {
                            let label = match channel {
                                FanChannel::Cpu => "CPU 风扇曲线已应用",
                                FanChannel::Gpu => "GPU 风扇曲线已应用",
                            };
                            self.transient(3, label, String::new());
                        }
                        Some(ControlChange::SetKeyboardLighting { .. }) => {
                            if let Some(ui) = self.ui.upgrade() {
                                ui.global::<AppState>().set_keyboard_submit("已应用".into());
                            }
                            self.transient(3, "键盘灯效已应用", String::new());
                        }
                        _ => {}
                    }
                    if let Some(next_target) = self.pending_releases.pop() {
                        let expected_revision = self.snapshot.revision;
                        self.send_simple(IpcCommand::ReleaseManualOverride {
                            expected_revision,
                            target: next_target,
                        });
                    }
                }
                IpcReply::Config { config, revision } => {
                    self.config = Some((revision, config));
                    if self.theme_override.is_none() {
                        self.dark = match config.theme {
                            msilt_core::Theme::Light => false,
                            msilt_core::Theme::Dark => true,
                            msilt_core::Theme::System => dwm::registry_prefers_dark().unwrap_or(self.dark),
                        };
                        if let Some(ui) = self.ui.upgrade() {
                            let theme = ui.global::<Theme>();
                            theme.set_dark(self.dark);
                            theme.invoke_apply();
                            if let Some(hwnd) = dwm::hwnd_of(ui.window()) { dwm::retheme(hwnd, self.dark); }
                        }
                    }
                    if !self.demo && !self.headless && !self.endpoint.diagnostic {
                        if config.updates.auto_check && !self.startup_update_checked {
                            self.startup_update_checked = true;
                            let weak_update = self.ui.clone();
                            Timer::single_shot(Duration::from_millis(1_800), move || {
                                if weak_update.upgrade().is_some() {
                                    with_app(|app| app.start_update_check(true));
                                }
                            });
                        }
                    }
                }
                IpcReply::SystemTools { startup, oem } => {
                    self.oem_busy = false;
                    self.startup = startup;
                    self.oem = oem;
                    if let Some(ui) = self.ui.upgrade() {
                        ui.global::<AppState>().set_oem_confirm_uninstall(-1);
                    }
                    if let Some(msg) = self.pending_tools_notice.take() {
                        self.transient(3, "操作完成", msg.into());
                        if let Some(ipc) = self.ipc.as_ref() {
                            let _ = ipc.send(IpcCommand::GetSnapshot {});
                        }
                    }
                }
                IpcReply::AutomationRules { rules, summary } => {
                    self.auto_rules = rules;
                    self.auto_summary = summary.clone();
                    if let Some(ui) = self.ui.upgrade() {
                        ui.global::<AppState>().set_dry_run_result(summary.into());
                    }
                }
                IpcReply::OsdShown {} => {
                    self.transient(3, "悬浮提示已显示", "已在屏幕右下角预览效果。".into());
                }
            },
            IpcResult::Error { error } => {
                self.oem_busy = false;
                self.pending_tools_notice = None;
                self.pending_releases.clear();
                self.transient(2, "操作未生效", error_code_text(error.code));
            }
        }
    }
}

fn scope_from(index: i32) -> OemScope {
    match index {
        1 => OemScope::Core,
        2 => OemScope::Lighting,
        3 => OemScope::TrueColor,
        _ => OemScope::All,
    }
}

fn package_from(index: i32) -> Option<OemPackageId> {
    match index {
        0 => Some(OemPackageId::MsiCenterUwp),
        1 => Some(OemPackageId::MsiCenterSdk),
        2 => Some(OemPackageId::MsiTrueColor),
        _ => None,
    }
}

fn preset_from(index: i32) -> Option<AutomationPreset> {
    match index {
        0 => Some(AutomationPreset::BatterySilent60Hz),
        1 => Some(AutomationPreset::AcBalanced240Hz),
        2 => Some(AutomationPreset::LowBatterySuperSave),
        3 => Some(AutomationPreset::LockScreenDarkKeyboard),
        _ => None,
    }
}

fn demo_preset_meta(preset: AutomationPreset) -> (&'static str, &'static str, &'static str, &'static str) {
    match preset {
        AutomationPreset::BatterySilent60Hz => ("battery-silent-60hz", "拔电切静音与 60Hz", "电源切换 · 使用电池", "静音模式 + 60 Hz 刷新率"),
        AutomationPreset::AcBalanced240Hz => ("ac-balanced-240hz", "插电恢复平衡与 240Hz", "电源切换 · 交流电", "平衡模式 + 240 Hz 刷新率"),
        AutomationPreset::LowBatterySuperSave => ("low-battery-supersave", "低电量自动超级省电", "电量 ≤ 20%（迟滞 3%）", "超级省电 + 亮度 35%"),
        AutomationPreset::LockScreenDarkKeyboard => ("lock-dark-keyboard", "锁屏自动熄灭键盘灯", "会话锁屏", "24 区键盘灯全黑"),
    }
}

fn error_code_text(code: msilt_core::ErrorCode) -> String {
    use msilt_core::ErrorCode as E;
    let text = match code {
        E::InvalidRequest => "请求格式有误",
        E::IncompatibleVersion => "程序与后台服务版本不一致，请更新或重启",
        E::AccessDenied => "当前账户权限不足",
        E::NotReady => "后台服务正在初始化，请稍后再试",
        E::Unsupported => "当前设备不支持此项设置",
        E::ReadOnlyFirmware => "当前机型暂不支持该项硬件调节",
        E::DeniedWrite => "该操作受硬件安全策略保护",
        E::WriteNotAllowed => "当前为仅查看模式，无法修改",
        E::ValidationFailed => "设置值超出允许范围",
        E::RevisionConflict => "硬件状态刚有变化，请再试一次",
        E::RequestExpired => "等待超时，请重试",
        E::ConflictDetected => "被其他软件占用或状态冲突",
        E::Busy => "后台正在处理上一项操作，请稍候",
        E::TimedOut => "硬件响应超时，请确认当前状态",
        E::BackendUnavailable => "硬件接口暂不可用",
        E::WriteFailed => "设置未能写入硬件",
        E::VerificationFailed => "硬件未确认生效，请重试",
        E::RollbackFailed | E::RecoveryRequired => "为保护硬件已暂停控制，请重启服务或重启电脑",
        E::Internal => "后台服务出现内部异常",
    };
    text.to_string()
}

// ── event plumbing ──────────────────────────────────────────────────────────

fn process_inbox() {
    let items: Vec<Incoming> = APP.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(app) = guard.as_mut() else {
            return Vec::new();
        };
        app.ipc.as_ref().map(IpcClient::drain).unwrap_or_default()
    });
    if items.is_empty() {
        return;
    }
    let Some(mut app) = take_app() else {
        return;
    };
    let mut needs_full_push = false;
    for item in items {
        match item {
            Incoming::Message(IpcMessage::Event(event)) => match event {
                IpcEvent::StateChanged { snapshot } => {
                    app.apply_snapshot(snapshot);
                    needs_full_push = true;
                }
                // 1 Hz telemetry updates ONLY scalar readouts + active trend chart (never full model rebuild).
                IpcEvent::TelemetryTick { sample } => app.apply_telemetry(&sample),
                IpcEvent::OsdNotify { .. } => {}
            },
            Incoming::Message(IpcMessage::Response(response)) => {
                app.on_response(response.id, response.result);
                needs_full_push = true;
            }
            Incoming::Message(IpcMessage::Request(_)) => {}
            Incoming::Lost(reason) => {
                app.on_disconnect(reason);
                needs_full_push = true;
            }
        }
    }
    if needs_full_push {
        app.push_all();
    }
    put_app(app);
}

fn schedule_connect(ui: slint::Weak<AppWindow>, attempt: u32) {
    let delay = match attempt {
        0 => 0,
        1 => 200,
        2 => 400,
        3 => 800,
        4 => 1_600,
        _ => 5_000,
    };
    let weak = ui.clone();
    Timer::single_shot(Duration::from_millis(delay), move || {
        if weak.upgrade().is_none() {
            return;
        }
        let waker: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(|| {
            let _ = slint::invoke_from_event_loop(process_inbox);
        });
        let endpoint = APP.with(|cell| cell.borrow().as_ref().map(|app| app.endpoint.clone()));
        let Some(endpoint) = endpoint else { return; };
        match IpcClient::connect(endpoint, waker) {
            Ok(client) => {
                let hello_sent = client.hello().is_ok();
                let mut slot = Some(client);
                with_app(|app| {
                    app.connected = false;
                    app.subscriptions = SubscriptionPlan::default();
                    app.ipc = slot.take();
                    if !hello_sent {
                        app.transient(2, "连接失败", "无法连接到后台服务。".into());
                    }
                    app.push_all();
                });
            }
            Err(_) => {
                with_app(|app| {
                    app.connected = false;
                    app.push_all();
                });
                if attempt < 6 {
                    schedule_connect(ui, attempt + 1);
                }
            }
        }
    });
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn schedule_chrome(weak: slint::Weak<AppWindow>, dark: bool, attempt: u32) {
    let weak_inner = weak.clone();
    Timer::single_shot(Duration::from_millis(if attempt == 0 { 0 } else { 16 }), move || {
        let Some(ui) = weak_inner.upgrade() else {
            return;
        };
        let Some(hwnd) = dwm::hwnd_of(ui.window()) else {
            if attempt < 8 {
                schedule_chrome(weak.clone(), dark, attempt + 1);
            }
            return;
        };
        let report = dwm::apply_chrome(hwnd, dark);
        let scale = ui.window().scale_factor().max(1.0);
        let min_size = dwm::enforce_min_size(
            hwnd,
            (MIN_WINDOW.0 * scale) as i32,
            (MIN_WINDOW.1 * scale) as i32,
        );
        with_app(|app| {
            app.report = dwm::DwmReport { min_size, ..report };
            app.first_frame_ms = elapsed_ms();
            if let Some(ui) = app.ui.upgrade() {
                ui.global::<Theme>().set_mica(report.mica);
            }
            app.push_all();
        });
    });
}

fn run(options: Options) -> ExitCode {
    let demo = options.demo;
    let headless = if options.dump_png.is_some() {
        match dump::install() {
            Ok(window) => Some(window),
            Err(error) => { eprintln!("msilt-ui: {error}"); return ExitCode::FAILURE; }
        }
    } else { None };
    let ui = match AppWindow::new() {
        Ok(ui) => ui,
        Err(error) => {
            eprintln!("msilt-ui: 无法创建窗口：{error}");
            return ExitCode::FAILURE;
        }
    };

    // ── theme ───────────────────────────────────────────────────────────────
    let animations = headless.is_none() && dwm::client_area_animations();
    let dark = options.dark.unwrap_or_else(|| dwm::registry_prefers_dark().unwrap_or(true));
    {
        let theme = ui.global::<Theme>();
        if headless.is_none() {
            if let Some(((dr, dg, db), (lr, lg, lb))) = dwm::system_accent_pair() {
                theme.set_accent_dark(Color::from_rgb_u8(dr, dg, db));
                theme.set_accent_light(Color::from_rgb_u8(lr, lg, lb));
            }
        }
        theme.set_dark(dark);
        theme.set_reduced_motion(!animations);
        theme.set_mica(headless.is_none());
        theme.invoke_apply();
    }
    ui.window().set_size(slint::LogicalSize::new(options.width as f32, options.height as f32));

    // ── view model ──────────────────────────────────────────────────────────
    let snapshot = if demo { demo::snapshot() } else { SystemSnapshot::default() };
    let curves = curves_from(&snapshot);
    let zones = ZoneEditor::new(snapshot.hardware.keyboard_zones.as_ref());
    let report = dwm::DwmReport { dark, animations, ..dwm::DwmReport::default() };

    let app = App {
        ui: ui.as_weak(),
        snapshot,
        curves,
        zones,
        history: History::default(),
        demo,
        ipc: None,
        endpoint: options.endpoint,
        connected: false,
        dark,
        started: Instant::now(),
        report,
        toast_visible: false,
        toast_seq: 0,
        toast_notice: Notice { level: 4, title: String::new(), text: String::new(), action: String::new() },
        app_installed: !demo && updater::is_installed(),
        demo_tick: 0,
        subscriptions: SubscriptionPlan::default(),
        pending_control: None,
        config: demo.then_some((0, msilt_core::AppConfig::default())),
        config_pending: None,
        theme_override: options.dark,
        first_frame_ms: -1,
        startup: if demo { demo::startup_status() } else { StartupStatus::default() },
        oem: if demo { demo::oem_inventory() } else { OemInventory::default() },
        oem_busy: false,
        pending_tools_notice: None,
        pending_releases: Vec::new(),
        auto_rules: if demo { demo::automation_rules() } else { Vec::new() },
        auto_summary: if demo { "演示规则 · 2 条已加载".into() } else { "正在读取规则…".into() },
        update_state: 0,
        update_status_text: format!("当前 v{} · GitHub Releases", env!("CARGO_PKG_VERSION")),
        update_release: None,
        startup_update_checked: false,
        headless: headless.is_some(),
    };
    put_app(app);

    {
        let state = ui.global::<AppState>();
        state.set_ui_version(env!("CARGO_PKG_VERSION").into());
        state.set_daemon_version(env!("CARGO_PKG_VERSION").into());
        state.set_page(options.page);
        state.set_keyboard_conflict(false);
        state.set_keyboard_layout("24 区背光".into());
        state.set_automation_summary("正在读取规则…".into());
        state.set_monitor_window(5);
        state.set_osd_duration(1800);
    }
    with_app(|app| app.push_all());
    if demo {
        with_app(|app| {
            for tick in 0..180 {
                app.apply_telemetry(&demo::telemetry(tick, 1_800_000_000_000 + tick * 1000));
            }
        });
    }
    if let (Some(window), Some(path)) = (headless.as_ref(), options.dump_png.as_ref()) {
        let result = ui.show().map_err(|e| e.to_string())
            .and_then(|()| dump::render(window, options.width, options.height))
            .and_then(|pixels| dump::save(path, &pixels).map_err(|e| e.to_string()));
        take_app();
        return match result {
            Ok(()) => { println!("PNG {} ({}x{}, page {})", path.display(), options.width, options.height, options.page); ExitCode::SUCCESS }
            Err(error) => { eprintln!("msilt-ui: PNG 导出失败：{error}"); ExitCode::FAILURE }
        };
    }

    // ── callbacks ───────────────────────────────────────────────────────────
    {
        let state = ui.global::<AppState>();
        state.on_command(|kind, a, b| {
            with_app(|app| app.on_command(kind, a, b));
        });
        state.on_command_text(|kind, value| {
            let mut slot = Some(value.clone());
            with_app(|app| {
                if let Some(text) = slot.take() {
                    app.on_command_text(kind, text);
                }
            });
        });
        state.on_curve_edit(|channel, index, duty| {
            with_app(|app| app.on_curve_edit(channel, index, duty));
        });
        state.on_zone_edit(|index, additive| {
            with_app(|app| app.on_zone_edit(index, additive));
        });
        state.on_zone_hsv(|h, s, v| {
            with_app(|app| app.on_zone_hsv(h, s, v));
        });
    }

    let ticker = Timer::default();
    if demo { ticker.start(TimerMode::Repeated, Duration::from_secs(1), || {
        with_app(|app| {
            if let Some(ui) = app.ui.upgrade() {
                let state = ui.global::<AppState>();
                if !subscriptions::for_page(state.get_page(), state.get_monitor_paused()).telemetry { return; }
                app.demo_tick = app.demo_tick.wrapping_add(1);
                let sample = demo::telemetry(app.demo_tick, unix_now_ms());
                app.apply_telemetry(&sample);
            }
        });
    }); }

    // ── window lifecycle ────────────────────────────────────────────────────
    ui.window().on_close_requested(|| {
        APP.with(|cell| {
            if let Some(app) = cell.borrow_mut().as_mut() {
                if let Some(mut ipc) = app.ipc.take() {
                    ipc.shutdown();
                }
            }
        });
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    if ui.show().is_err() {
        eprintln!("msilt-ui: 无法显示窗口");
        return ExitCode::FAILURE;
    }
    schedule_chrome(ui.as_weak(), dark, 0);
    if !demo {
        schedule_connect(ui.as_weak(), 0);
    }

    let outcome = slint::run_event_loop();
    // Explicit teardown: drop the pipe and the view model so nothing survives.
    if let Some(mut app) = take_app() {
        if let Some(mut ipc) = app.ipc.take() {
            ipc.shutdown();
        }
    }
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("msilt-ui: 事件循环异常退出：{error}");
            ExitCode::FAILURE
        }
    }
}

// ── headless self-check ─────────────────────────────────────────────────────

fn check_layout() -> ExitCode {
    let mut failures: Vec<String> = Vec::new();
    let snapshot = demo::snapshot();

    // 1. both channels publish six strictly increasing knots with a valid floor
    for (name, state) in [
        ("cpu", snapshot.hardware.cpu_fan.as_ref()),
        ("gpu", snapshot.hardware.gpu_fan.as_ref()),
    ] {
        let Some(state) = state else {
            failures.push(format!("{name}: 风扇状态缺失"));
            continue;
        };
        if state.curve.points.len() != FAN_POINT_COUNT {
            failures.push(format!("{name}: 期望 {FAN_POINT_COUNT} 个节点"));
        }
        for pair in state.curve.points.windows(2) {
            if pair[0].temperature_c >= pair[1].temperature_c {
                failures.push(format!("{name}: 温度节点未严格递增"));
            }
            if pair[0].duty_percent > pair[1].duty_percent {
                failures.push(format!("{name}: 占空比递减"));
            }
        }
        if let Some(limits) = state.limits {
            if let Err(error) = state.curve.validate_with_limits(&limits) {
                failures.push(format!("{name}: 出厂曲线不满足 limits：{error}"));
            }
            if limits.floor.points[0].duty_percent != 0 {
                failures.push(format!("{name}: 锚点不是 0%"));
            }
        } else {
            failures.push(format!("{name}: 缺少 limits，画布会退化为只读"));
        }
    }

    // 2. editors clamp instead of producing an illegal curve
    let demo_curves = curves_from(&snapshot);
    for (index, channel) in [FanChannel::Cpu, FanChannel::Gpu].into_iter().enumerate() {
        if !demo_curves[index].is_editable() {
            failures.push(format!("{}: 演示快照应可编辑", view::channel_name(channel)));
        }
    }
    let mut editor = CurveEditor::new(FanChannel::Cpu, snapshot.hardware.cpu_fan.as_ref().unwrap());
    editor.set_duty(0, 40);
    if editor.knots()[0].duty_percent != 0 {
        failures.push("锚点未被锁定".into());
    }
    editor.set_duty(5, 0);
    if editor.knots()[5].duty_percent < editor.knots()[5].floor_percent {
        failures.push("安全下限未生效".into());
    }
    for kind in 0..3u8 {
        editor.preset(kind);
        if let Err(error) = editor.curve().validate_shape() {
            failures.push(format!("预设 {kind} 产生非法曲线：{error}"));
        }
    }

    // 3. keyboard model
    let mut zones = ZoneEditor::new(snapshot.hardware.keyboard_zones.as_ref());
    if zones.colors().len() != KEYBOARD_ZONE_COUNT {
        failures.push("键盘区数量不是 24".into());
    }
    zones.select_all();
    if zones.selected_count() != KEYBOARD_ZONE_COUNT {
        failures.push("全选未覆盖 24 区".into());
    }
    zones.toggle(3, false);
    if zones.selected_count() != 1 || !zones.selected()[3] {
        failures.push("独占选择语义错误".into());
    }

    // 4. trend geometry
    let mut history = History::default();
    for tick in 0..8u64 {
        history.push(&demo::telemetry(tick, 1_000 + tick));
    }
    let path = view::series_path(&history.cpu, view::TREND_MAX_C);
    if !path.starts_with(" M") || !path.contains(" L") {
        failures.push(format!("趋势路径无效：{path}"));
    }

    // 5. banner + notice plumbing
    if view::notice(&snapshot, true, false, true).is_some() {
        failures.push("可写入快照不应产生 banner".into());
    }

    println!("MSI-Toolkit UI · 布局与交互自检");
    println!("  窗口            1120x720 逻辑像素（最小 820x580，支持半屏分屏与自适应缩放）");
    println!("  顶栏            40 px（与导航共用 Mica）· 状态胶囊 24 px");
    println!("  左导航          184 px（紧凑 48 px，<1008px 自动收起）· 无边框项高 36 px · 3x16 强调条");
    println!("  内容区          设置页居中 1000px · 工作区居中 1200px · 悬浮 Toast 零位移渐入渐出");
    println!("  控件体系        FluentButton · FluentSegmentBar 丝滑滑块 · FluentToggle · SettingsExpander");
    println!("  字体            14/13/12 px（Segoe UI Variable）· 数值 Cascadia Mono");
    println!("  页面            8 个（总览/性能与风扇/显卡与显示/电池与电源/键盘灯效/自动化/监控与提示/系统与工具）");
    println!("  风扇节点        6 × 2 通道，2x3 步进栅格，下限 {MAX_DUTY_REF}");
    println!("  键盘矩阵        6 × 4 = 24 区等比缩放键盘 + 2D HSV 色盘");
    println!("  渲染后端        纯 CPU 软件渲染（renderer-software + backend-winit，0 GPU 上下文）");

    if failures.is_empty() {
        println!("\n结果：通过（0 项失败）");
        ExitCode::SUCCESS
    } else {
        println!("\n结果：失败（{} 项）", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        ExitCode::FAILURE
    }
}

const MAX_DUTY_REF: &str = "150% 上限";

fn main() -> ExitCode {
    let _ = PROCESS_START.set(Instant::now());
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => { eprintln!("msilt-ui: {error}"); return ExitCode::FAILURE; }
    };
    if options.check_layout {
        return check_layout();
    }
    if options.help {
        println!("msilt-ui [--demo] [--page 0..7] [--width W] [--height H] [--theme system|light|dark] [--dump-png PATH] [--pipe NAME] [--check-layout]");
        return ExitCode::SUCCESS;
    }
    run(options)
}
