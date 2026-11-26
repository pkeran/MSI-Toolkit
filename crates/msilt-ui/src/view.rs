//! Snapshot -> Slint projection.
//!
//! Rules encoded here:
//!   * `None` on the wire becomes `-1` in Slint, never `0`.
//!   * a snapshot only replaces state when `revision` is newer;
//!   * telemetry never rewrites hardware state (RPM / EC duty / editable curve
//!     are three different quantities);
//!   * the chart geometry is produced here so the canvas stays two `Path`s.

use std::collections::VecDeque;

use msilt_core::{
    AutomationRuleView, CapabilityFlags, ControlChange, DaemonStatus, EcWriteStatus, FanChannel,
    GpuMuxMode, OemInventory, OemItemKind, OemPackageId, OemScope, PerformanceMode, PowerSource,
    StartupMode, StartupStatus, SystemSnapshot, TelemetrySample,
};
use slint::{Color, Model, ModelRc, SharedString, VecModel};

use crate::editor::{CurveEditor, ZoneEditor};
use crate::{AppState, AutoRuleRow, CurvePoint, OemItemRow, OemPkgRow, SampleRow};

/// Samples kept for the trend canvas: 15 minutes at 1 Hz, so the 1/5/15 min
/// window selector is truthful.
pub const HISTORY_CAP: usize = 900;
/// Rows kept for the visible sample table (the `ScrollView` shows these).
pub const SAMPLE_ROWS: usize = 40;
/// Maximum vertices per rendered polyline. Longer series are decimated so a
/// 900-point history stays one cheap `Path` instead of 900 segments.
pub const MAX_PATH_POINTS: usize = 180;
/// Y axis used for the temperature trend, in °C.
pub const TREND_MAX_C: f32 = 100.0;

#[derive(Debug, Default)]
pub struct History {
    pub cpu: VecDeque<Option<i16>>,
    pub gpu: VecDeque<Option<i16>>,
    pub samples: VecDeque<SampleRow>,
    pub last_unix_ms: u64,
}

impl History {
    pub fn push(&mut self, sample: &TelemetrySample) {
        push_bounded(&mut self.cpu, sample.cpu_temperature_c);
        push_bounded(&mut self.gpu, sample.gpu_temperature_c);
        self.last_unix_ms = sample.sampled_at_unix_ms;
        let row = SampleRow {
            time: {
                let seconds = sample.sampled_at_unix_ms / 1000;
                format!("{:02}:{:02}:{:02}", seconds / 3600 % 24, seconds / 60 % 60, seconds % 60).into()
            },
            cpu: opt_text(sample.cpu_temperature_c, "°C"),
            gpu: opt_text(sample.gpu_temperature_c, "°C"),
            cpu_rpm: opt_text(sample.cpu_fan_rpm, ""),
            gpu_rpm: opt_text(sample.gpu_fan_rpm, ""),
            gpu_power: sample
                .gpu_power_mw
                .map_or_else(|| "—".to_string(), |mw| format!("{:.0}", f64::from(mw) / 1000.0))
                .into(),
        };
        if self.samples.len() == SAMPLE_ROWS {
            self.samples.pop_back();
        }
        self.samples.push_front(row);
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.cpu.clear();
        self.gpu.clear();
        self.samples.clear();
    }
}

fn push_bounded(queue: &mut VecDeque<Option<i16>>, value: Option<i16>) {
    if queue.len() == HISTORY_CAP {
        queue.pop_front();
    }
    queue.push_back(value);
}

fn opt_text<T: std::fmt::Display>(value: Option<T>, unit: &str) -> SharedString {
    match value {
        Some(v) if unit.is_empty() => format!("{v}").into(),
        Some(v) => format!("{v} {unit}").into(),
        None => "—".into(),
    }
}

/// Builds an SVG polyline in a 0..1000 x 0..1000 viewbox. Missing readings start
/// a new sub-path (the line breaks) instead of being interpolated. Series longer
/// than `MAX_PATH_POINTS` are decimated by taking a regular stride and always
/// keeping the newest sample.
pub fn series_path(values: &VecDeque<Option<i16>>, max_value: f32) -> String {
    if values.len() < 2 || max_value <= 0.0 {
        return String::new();
    }
    let stride = values.len().div_ceil(MAX_PATH_POINTS).max(1);
    let span = (values.len() - 1) as f32;
    let mut out = String::with_capacity(MAX_PATH_POINTS * 18);
    let mut pen_down = false;
    for (index, value) in values.iter().enumerate() {
        let keep = index % stride == 0 || index + 1 == values.len();
        if !keep {
            continue;
        }
        let Some(value) = value else {
            pen_down = false;
            continue;
        };
        let x = index as f32 / span * 1000.0;
        let y = (1.0 - (f32::from(*value) / max_value).clamp(0.0, 1.0)) * 1000.0;
        out.push_str(if pen_down { " L" } else { " M" });
        out.push_str(&format!("{x:.1} {y:.1}"));
        pen_down = true;
    }
    out
}

/// `AppState` colour for a severity level (0 muted, 1 warn, 2 danger, 3 ok).
fn capability(snapshot: &SystemSnapshot, flag: CapabilityFlags) -> bool {
    matches!(snapshot.status, DaemonStatus::Ready | DaemonStatus::Degraded)
        && snapshot.capabilities.contains(flag) && snapshot.writable.contains(flag)
}

pub fn can_submit(snapshot: &SystemSnapshot, change: &ControlChange) -> bool {
    if !capability(snapshot, change.capability()) || change.validate_parameters().is_err() { return false; }
    let h = &snapshot.hardware;
    match change {
        ControlChange::SetPerformanceMode { mode } => h.supported_performance_modes.contains(mode),
        ControlChange::SetChargeLimit { percent } => h.supported_charge_limits.contains(percent),
        ControlChange::SetDisplayRefreshRate { hz } => h.supported_refresh_rates.contains(hz),
        ControlChange::SetColorProfile { profile_id } => h.supported_color_profiles.contains(profile_id),
        ControlChange::SetFanCurve { channel, curve } => {
            let fan = match channel { FanChannel::Cpu => h.cpu_fan.as_ref(), FanChannel::Gpu => h.gpu_fan.as_ref() };
            snapshot.ec_write_status == EcWriteStatus::Enabled
                && fan.and_then(|f| f.limits.as_ref()).is_some_and(|limits| curve.validate_with_limits(limits).is_ok())
        }
        ControlChange::SetCoolerBoost { .. } => true,
        ControlChange::SetDisplayBrightness { .. } => h.display_brightness_percent.is_some(),
        ControlChange::SetKeyboardLighting { .. } => true,
        _ => false,
    }
}

pub fn choice_index<T: PartialEq>(choices: &[T], current: Option<&T>) -> i32 {
    current.and_then(|value| choices.iter().position(|item| item == value)).map_or(-1, |index| index as i32)
}

fn mode_name(mode: PerformanceMode) -> &'static str {
    match mode { PerformanceMode::SuperBattery => "超级省电", PerformanceMode::Silent => "静音", PerformanceMode::Balanced => "平衡", PerformanceMode::ExtremePerformance => "极致性能" }
}

fn mode_index(mode: PerformanceMode) -> i32 {
    match mode {
        PerformanceMode::SuperBattery => 0,
        PerformanceMode::Silent => 1,
        PerformanceMode::Balanced => 2,
        PerformanceMode::ExtremePerformance => 3,
    }
}

#[cfg(test)]
pub fn mode_from_index(index: i32) -> Option<PerformanceMode> {
    match index {
        0 => Some(PerformanceMode::SuperBattery),
        1 => Some(PerformanceMode::Silent),
        2 => Some(PerformanceMode::Balanced),
        3 => Some(PerformanceMode::ExtremePerformance),
        _ => None,
    }
}

fn mux_index(mode: GpuMuxMode) -> i32 {
    match mode {
        GpuMuxMode::Hybrid => 0,
        GpuMuxMode::Discrete => 1,
        GpuMuxMode::Integrated => 2,
    }
}

fn ec_index(status: EcWriteStatus) -> i32 {
    match status {
        EcWriteStatus::Unavailable => 0,
        EcWriteStatus::ReadOnlyFirmware => 1,
        EcWriteStatus::UnverifiedMapping => 2,
        EcWriteStatus::ConflictDetected => 3,
        EcWriteStatus::Enabled => 4,
        EcWriteStatus::RecoveryRequired => 5,
    }
}

fn sync_vec_model<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    next: Vec<T>,
    assign: impl FnOnce(ModelRc<T>),
) {
    if let Some(existing) = current.as_any().downcast_ref::<VecModel<T>>() {
        if existing.row_count() == next.len() {
            for (index, item) in next.into_iter().enumerate() {
                if existing.row_data(index).as_ref() != Some(&item) {
                    existing.set_row_data(index, item);
                }
            }
            return;
        }
    }
    assign(ModelRc::new(VecModel::from(next)));
}

/// Projection of the demo/real snapshot into the shell globals.
pub fn apply_snapshot(state: &AppState<'_>, snapshot: &SystemSnapshot, demo: bool) {
    let hardware = &snapshot.hardware;
    state.set_revision(snapshot.revision.to_string().into());
    state.set_demo(demo);

    state.set_cap_performance(capability(snapshot, CapabilityFlags::PERFORMANCE_MODE) && !hardware.supported_performance_modes.is_empty());
    state.set_cap_fan(capability(snapshot, CapabilityFlags::FAN_CURVE));
    state.set_cap_cooler(capability(snapshot, CapabilityFlags::COOLER_BOOST));
    state.set_cap_charge(capability(snapshot, CapabilityFlags::CHARGE_LIMIT) && !hardware.supported_charge_limits.is_empty());
    state.set_cap_refresh(capability(snapshot, CapabilityFlags::DISPLAY_REFRESH_RATE) && !hardware.supported_refresh_rates.is_empty());
    state.set_cap_brightness(capability(snapshot, CapabilityFlags::DISPLAY_BRIGHTNESS));
    state.set_cap_color(capability(snapshot, CapabilityFlags::COLOR_PROFILE) && !hardware.supported_color_profiles.is_empty());
    state.set_cap_keyboard(capability(snapshot, CapabilityFlags::KEYBOARD_LIGHTING));
    state.set_cap_mux(capability(snapshot, CapabilityFlags::GPU_MUX));
    state.set_cap_telemetry(snapshot.capabilities.contains(CapabilityFlags::TELEMETRY));
    state.set_ec_write_status(ec_index(snapshot.ec_write_status));
    state.set_ec_writable(snapshot.ec_write_status == EcWriteStatus::Enabled);

    state.set_board_id(hardware.identity.board_id.clone().unwrap_or_else(|| "—".into()).into());
    state.set_ec_firmware(hardware.identity.ec_firmware.clone().unwrap_or_else(|| "—".into()).into());
    state.set_performance_mode(hardware.performance_mode.map_or(-1, mode_index));
    sync_vec_model(
        state.get_supported_modes(),
        hardware.supported_performance_modes.iter().copied().map(mode_index).collect::<Vec<i32>>(),
        |m| state.set_supported_modes(m),
    );
    sync_vec_model(
        state.get_mode_labels(),
        hardware.supported_performance_modes.iter().map(|mode| SharedString::from(mode_name(*mode))).collect::<Vec<_>>(),
        |m| state.set_mode_labels(m),
    );
    state.set_mode_choice(choice_index(&hardware.supported_performance_modes, hardware.performance_mode.as_ref()));
    state.set_cooler_boost_known(hardware.cooler_boost.is_some());
    state.set_cooler_boost(hardware.cooler_boost.unwrap_or(false));
    state.set_power_source(match hardware.power_source {
        Some(PowerSource::Ac) => 0,
        Some(PowerSource::Battery) => 1,
        None => -1,
    });
    state.set_battery_percent(hardware.battery_percent.map_or(-1, i32::from));
    state.set_charge_limit(hardware.charge_limit_percent.map_or(-1, i32::from));
    sync_vec_model(
        state.get_charge_limits(),
        hardware.supported_charge_limits.iter().map(|v| i32::from(*v)).collect::<Vec<i32>>(),
        |m| state.set_charge_limits(m),
    );
    sync_vec_model(
        state.get_charge_labels(),
        hardware.supported_charge_limits.iter().map(|v| SharedString::from(format!("{v}%"))).collect::<Vec<_>>(),
        |m| state.set_charge_labels(m),
    );
    state.set_charge_choice(choice_index(&hardware.supported_charge_limits, hardware.charge_limit_percent.as_ref()));
    state.set_refresh_hz(hardware.display_refresh_hz.map_or(-1, i32::from));
    sync_vec_model(
        state.get_refresh_rates(),
        hardware.supported_refresh_rates.iter().map(|v| i32::from(*v)).collect::<Vec<i32>>(),
        |m| state.set_refresh_rates(m),
    );
    sync_vec_model(
        state.get_refresh_labels(),
        hardware.supported_refresh_rates.iter().map(|v| SharedString::from(format!("{v} Hz"))).collect::<Vec<_>>(),
        |m| state.set_refresh_labels(m),
    );
    state.set_refresh_choice(choice_index(&hardware.supported_refresh_rates, hardware.display_refresh_hz.as_ref()));
    state.set_brightness(hardware.display_brightness_percent.map_or(-1, i32::from));
    state.set_color_profile(hardware.color_profile_id.clone().unwrap_or_else(|| "—".into()).into());
    state.set_color_profile_index(
        hardware
            .color_profile_id
            .as_ref()
            .and_then(|id| hardware.supported_color_profiles.iter().position(|p| p == id))
            .map_or(-1, |index| index as i32),
    );
    sync_vec_model(
        state.get_color_profiles(),
        hardware
            .supported_color_profiles
            .iter()
            .map(|id| SharedString::from(id.as_str()))
            .collect::<Vec<SharedString>>(),
        |m| state.set_color_profiles(m),
    );
    state.set_mux_current(hardware.gpu_mux.map_or(-1, |mux| mux_index(mux.current)));
    state.set_mux_pending(hardware.gpu_mux.and_then(|mux| mux.pending).map_or(-1, mux_index));
    state.set_override_count(snapshot.manual_overrides.len() as i32);
    state.set_panel_info(
        hardware
            .panel_name
            .as_deref()
            .unwrap_or(if demo { crate::demo::PANEL } else { "内置显示器" })
            .into(),
    );
    state.set_keyboard_device(
        if demo {
            "24 区背光键盘"
        } else if hardware.keyboard_zones.is_some() || snapshot.capabilities.contains(CapabilityFlags::KEYBOARD_LIGHTING) {
            "24 区背光键盘"
        } else {
            "—"
        }
        .into(),
    );
    if let Some(conflict) = hardware.keyboard_conflict {
        state.set_keyboard_conflict(conflict);
    }

    if let Some(detail) = hardware.battery_detail {
        state.set_battery_designed(detail.designed_capacity_mwh.map_or_else(|| "—".into(), |mwh| format!("{:.1} Wh", f64::from(mwh) / 1000.0)).into());
        state.set_battery_full(detail.full_charged_capacity_mwh.map_or_else(|| "—".into(), |mwh| format!("{:.1} Wh", f64::from(mwh) / 1000.0)).into());
        state.set_battery_health(detail.health_tenths_percent.map_or_else(|| "—".into(), |t| format!("{:.1}%", f64::from(t) / 10.0)).into());
        state.set_battery_voltage(detail.voltage_mv.map_or_else(|| "—".into(), |mv| format!("{:.2} V", f64::from(mv) / 1000.0)).into());
        state.set_battery_rate(detail.rate_mw.map_or_else(|| "—".into(), |mw| {
            let sign = if mw > 0 { "+" } else { "" };
            format!("{sign}{:.1} W", f64::from(mw) / 1000.0)
        }).into());
        state.set_battery_cycles(detail.cycle_count.map_or_else(|| "—".into(), |c| format!("{c} 次")).into());
    } else {
        state.set_battery_designed("—".into());
        state.set_battery_full("—".into());
        state.set_battery_health("—".into());
        state.set_battery_voltage("—".into());
        state.set_battery_rate("—".into());
        state.set_battery_cycles("—".into());
    }
}

pub fn apply_system_tools(state: &AppState<'_>, startup: &StartupStatus, oem: &OemInventory) {
    state.set_startup_enabled(startup.enabled);
    state.set_startup_installed(startup.installed_in_program_files);
    state.set_startup_mode_label(
        if !startup.enabled {
            "未开启"
        } else {
            match startup.mode {
                Some(StartupMode::ScheduledTask) => "计划任务（免 UAC）",
                Some(StartupMode::UserRunKey) => "用户登录自启",
                _ => "已开启",
            }
        }
        .into(),
    );
    state.set_startup_detail(
        if startup.detail.is_empty() {
            "开机后在后台静默启动守护进程"
        } else {
            startup.detail.as_str()
        }
        .into(),
    );

    state.set_oem_elevated(oem.elevated);
    state.set_oem_has_backup(oem.has_backup);
    state.set_oem_protected_intact(oem.protected_intact);
    let running_services = oem.items.iter().filter(|item| item.running).count() as i32;
    let total_kib: u64 = oem.processes.iter().map(|p| u64::from(p.memory_kib)).sum();
    state.set_oem_running_services(running_services);
    state.set_oem_running_procs(oem.processes.len() as i32);
    state.set_oem_proc_ram(format!("{:.0} MiB", total_kib as f64 / 1024.0).into());
    let proc_summary = if oem.processes.is_empty() {
        "当前无运行中的 MSI 官方后台进程".to_string()
    } else {
        let names: Vec<&str> = oem.processes.iter().take(5).map(|p| p.name.as_str()).collect();
        let extra = oem.processes.len().saturating_sub(5);
        if extra > 0 {
            format!("活动进程：{} 等 {} 个", names.join(" · "), oem.processes.len())
        } else {
            format!("活动进程：{}", names.join(" · "))
        }
    };
    state.set_oem_proc_summary(proc_summary.into());
    state.set_oem_summary(oem.summary.clone().into());

    let items: Vec<OemItemRow> = oem
        .items
        .iter()
        .map(|item| OemItemRow {
            id: item.id.clone().into(),
            name: item.display_name.clone().into(),
            kind: match item.kind {
                OemItemKind::Service => "服务",
                OemItemKind::ScheduledTask => "计划任务",
                OemItemKind::RegistryRun => "登录启动项",
                OemItemKind::StoreStartup => "商店自启",
            }
            .into(),
            tier: match item.tier {
                OemScope::Core => 1,
                OemScope::Lighting => 2,
                OemScope::TrueColor => 3,
                OemScope::All => 0,
            },
            present: item.present,
            running: item.running,
            auto_start: item.auto_start,
            startup_label: item.startup_label.clone().into(),
        })
        .collect();
    state.set_oem_items(ModelRc::new(VecModel::from(items)));

    let packages: Vec<OemPkgRow> = oem
        .packages
        .iter()
        .map(|pkg| OemPkgRow {
            id: match pkg.id {
                OemPackageId::MsiCenterUwp => 0,
                OemPackageId::MsiCenterSdk => 1,
                OemPackageId::MsiTrueColor => 2,
            },
            name: pkg.display_name.clone().into(),
            version: pkg.version.clone().into(),
            installed: pkg.installed,
            can_uninstall: pkg.can_uninstall,
            note: pkg.note.clone().into(),
        })
        .collect();
    state.set_oem_packages(ModelRc::new(VecModel::from(packages)));
}

pub fn apply_automation_rules(state: &AppState<'_>, rules: &[AutomationRuleView], summary: &str) {
    let rows: Vec<AutoRuleRow> = rules
        .iter()
        .map(|rule| AutoRuleRow {
            id: rule.id.clone().into(),
            name: rule.name.clone().into(),
            enabled: rule.enabled,
            priority: i32::from(rule.priority),
            trigger: rule.trigger.clone().into(),
            actions: rule.actions.clone().into(),
            diagnostic: rule.diagnostic.clone().into(),
            active: rule.active,
        })
        .collect();
    state.set_auto_rules(ModelRc::new(VecModel::from(rows)));
    if !summary.is_empty() {
        state.set_automation_summary(summary.into());
    }
}

/// Projects both curve editors into the canvas models (mutating in-place to avoid layout churn).
pub fn apply_curves(state: &AppState<'_>, curves: &[CurveEditor; 2]) {
    sync_vec_model(state.get_cpu_curve(), curve_model(&curves[0]), |m| state.set_cpu_curve(m));
    sync_vec_model(state.get_gpu_curve(), curve_model(&curves[1]), |m| state.set_gpu_curve(m));
    let max_duty = curves[0].max_duty_percent().max(curves[1].max_duty_percent());
    state.set_curve_max_duty(i32::from(max_duty));
    state.set_cpu_curve_max(i32::from(curves[0].max_duty_percent()));
    state.set_gpu_curve_max(i32::from(curves[1].max_duty_percent()));
    state.set_cpu_curve_dirty(curves[0].is_dirty());
    state.set_gpu_curve_dirty(curves[1].is_dirty());
    state.set_cpu_curve_editable(curves[0].is_editable());
    state.set_gpu_curve_editable(curves[1].is_editable());
}

fn curve_model(editor: &CurveEditor) -> Vec<CurvePoint> {
    editor
        .knots()
        .iter()
        .enumerate()
        .map(|(index, knot)| CurvePoint {
            temp: i32::from(knot.temperature_c),
            duty: i32::from(knot.duty_percent),
            floor: i32::from(knot.floor_percent),
            anchor: index == 0 && editor.fixed_anchor(),
        })
        .collect()
}

pub fn apply_curve_flags(state: &AppState<'_>, curves: &[CurveEditor; 2], channel: usize) {
    let editor = &curves[channel.min(1)];
    state.set_curve_channel(channel as i32);
    state.set_curve_dirty(curves.iter().any(CurveEditor::is_dirty));
    state.set_curve_error(editor.hint().unwrap_or("").into());
}

/// Projects the zone editor into the 24-cell physical keyboard + HSV color pad
/// using in-place `VecModel` diffing so dragging the color pad stays 60fps.
pub fn apply_zones(state: &AppState<'_>, zones: &ZoneEditor, dark: bool) {
    let _ = dark;
    let colors = zones
        .scaled()
        .iter()
        .map(|c| Color::from_rgb_u8(c.r, c.g, c.b))
        .collect::<Vec<Color>>();
    let active = zones.active_color();
    let (h, s, v) = zones.hsv();
    let pure = zones.pure_hue_rgb();
    sync_vec_model(state.get_zones(), colors, |m| state.set_zones(m));
    sync_vec_model(state.get_zone_selected(), zones.selected().to_vec(), |m| state.set_zone_selected(m));
    state.set_zone_brightness(i32::from(zones.brightness()));
    state.set_zone_color_hex(format!("#{:02X}{:02X}{:02X}", active.r, active.g, active.b).into());
    state.set_zone_hue(h);
    state.set_zone_sat(s);
    state.set_zone_val(v);
    state.set_zone_pure_hue(Color::from_rgb_u8(pure.r, pure.g, pure.b));
    state.set_zone_active_color(Color::from_rgb_u8(active.r, active.g, active.b));
}

pub fn apply_history_paths(state: &AppState<'_>, history: &History) {
    let count = (state.get_monitor_window().max(1) as usize * 60).min(HISTORY_CAP);
    let tail = |queue: &VecDeque<Option<i16>>| queue.iter().skip(queue.len().saturating_sub(count)).copied().collect::<VecDeque<_>>();
    state.set_cpu_path(series_path(&tail(&history.cpu), TREND_MAX_C).into());
    state.set_gpu_path(series_path(&tail(&history.gpu), TREND_MAX_C).into());
    state.set_series_len(history.cpu.len() as i32);
}

pub fn apply_history(state: &AppState<'_>, history: &History) {
    apply_history_paths(state, history);
    state.set_samples(ModelRc::new(VecModel::from(history.samples.iter().cloned().collect::<Vec<_>>())));
    state.set_sample_table(ModelRc::new(VecModel::from(history.samples.iter().map(|row| {
        ModelRc::new(VecModel::from([&row.time, &row.cpu, &row.gpu, &row.cpu_rpm, &row.gpu_rpm, &row.gpu_power].map(|text| slint::StandardListViewItem::from(text.clone())).to_vec()))
    }).collect::<Vec<_>>())));
}

pub fn apply_telemetry(state: &AppState<'_>, sample: &TelemetrySample, age_ms: u64) {
    state.set_cpu_temp(sample.cpu_temperature_c.map_or(-1, i32::from));
    state.set_gpu_temp(sample.gpu_temperature_c.map_or(-1, i32::from));
    state.set_cpu_rpm(sample.cpu_fan_rpm.map_or(-1, i32::from));
    state.set_gpu_rpm(sample.gpu_fan_rpm.map_or(-1, i32::from));
    state.set_cpu_duty(sample.cpu_reported_duty_percent.map_or(-1, i32::from));
    state.set_gpu_duty(sample.gpu_reported_duty_percent.map_or(-1, i32::from));
    state.set_gpu_power_mw(sample.gpu_power_mw.map_or(-1, |mw| mw.min(i32::MAX as u32) as i32));
    state.set_telemetry_age(format_age(age_ms).into());
}

/// Human-readable age; `—` when there has never been a sample.
pub fn format_age(age_ms: u64) -> String {
    if age_ms == u64::MAX {
        return "—".to_string();
    }
    if age_ms < 1_000 {
        format!("{age_ms} ms 前")
    } else {
        format!("{} s 前", age_ms / 1_000)
    }
}

/// Connection banner derived from the daemon state. `None` means "all clear".
pub struct Notice {
    pub level: i32,
    pub title: String,
    pub text: String,
    pub action: String,
}

pub fn notice(snapshot: &SystemSnapshot, connected: bool, demo: bool, installed: bool) -> Option<Notice> {
    if demo { return None; }
    if !connected {
        return Some(Notice {
            level: 2,
            title: "后台服务未连接".into(),
            text: if installed {
                "暂时无法调节硬件，点击右侧按钮启动服务。".into()
            } else {
                "请先启动后台服务。".into()
            },
            action: if installed { "启动服务".into() } else { "重新连接".into() },
        });
    }
    match snapshot.ec_write_status {
        EcWriteStatus::Enabled => None,
        EcWriteStatus::RecoveryRequired => Some(Notice {
            level: 2,
            title: "硬件控制已暂停".into(),
            text: "上次调节未收到确认，为保护硬件已暂停控制，请重启服务或重启电脑后重试。".into(),
            action: String::new(),
        }),
        EcWriteStatus::ConflictDetected => Some(Notice {
            level: 1,
            title: "MSI 官方软件正在占用硬件".into(),
            text: "部分调节已暂停，温度监控仍可正常查看。".into(),
            action: "去管理".into(),
        }),
        EcWriteStatus::ReadOnlyFirmware => Some(Notice {
            level: 1,
            title: "当前为仅查看模式".into(),
            text: "当前机型暂不支持硬件调节，仍可查看温度与风扇状态。".into(),
            action: String::new(),
        }),
        EcWriteStatus::UnverifiedMapping => Some(Notice {
            level: 1,
            title: "当前为仅查看模式".into(),
            text: "当前机型暂未开放硬件调节，仅提供状态查看。".into(),
            action: String::new(),
        }),
        EcWriteStatus::Unavailable => Some(Notice {
            level: 1,
            title: "硬件控制暂不可用".into(),
            text: "暂时无法访问硬件控制接口，状态监控仍可正常查看。".into(),
            action: String::new(),
        }),
    }
}

#[allow(dead_code)]
pub fn channel_name(channel: FanChannel) -> &'static str {
    match channel {
        FanChannel::Cpu => "CPU",
        FanChannel::Gpu => "GPU",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cpu: Option<i16>, gpu: Option<i16>) -> TelemetrySample {
        TelemetrySample {
            sampled_at_unix_ms: 1,
            cpu_temperature_c: cpu,
            gpu_temperature_c: gpu,
            ..TelemetrySample::default()
        }
    }

    #[test]
    fn missing_readings_break_the_line_instead_of_being_interpolated() {
        let mut history = History::default();
        for s in [
            sample(Some(40), None),
            sample(Some(45), Some(50)),
            sample(None, Some(55)),
            sample(Some(60), Some(70)),
        ] {
            history.push(&s);
        }
        let path = series_path(&history.cpu, TREND_MAX_C);
        assert!(path.starts_with(" M"), "{path}");
        assert_eq!(path.matches(" M").count(), 2, "a gap must start a new sub-path: {path}");
        assert!(path.contains(" L"));
        assert_eq!(series_path(&VecDeque::new(), TREND_MAX_C), "");
    }

    #[test]
    fn samples_are_capped_and_aged() {
        let mut history = History::default();
        for i in 0..(SAMPLE_ROWS + 5) {
            history.push(&sample(Some(i as i16), Some(0)));
        }
        assert_eq!(history.samples.len(), SAMPLE_ROWS);
        assert_eq!(history.samples[0].time.as_str(), "00:00:00");
        assert_eq!(history.samples[SAMPLE_ROWS - 1].time.as_str(), "00:00:00");
        assert_eq!(history.samples[0].cpu.as_str(), "44 °C");
    }

    #[test]
    fn long_histories_are_decimated_to_one_path() {
        let mut history = History::default();
        for i in 0..HISTORY_CAP {
            history.push(&sample(Some((i % 90) as i16), None));
        }
        assert_eq!(history.cpu.len(), HISTORY_CAP);
        let path = series_path(&history.cpu, TREND_MAX_C);
        let vertices = path.matches(" M").count() + path.matches(" L").count();
        assert!(vertices <= MAX_PATH_POINTS + 1, "{vertices} vertices");
        assert!(vertices >= MAX_PATH_POINTS / 2, "{vertices} vertices");
    }

    #[test]
    fn none_is_never_rendered_as_zero() {
        assert_eq!(opt_text(None::<i16>, "°C").as_str(), "—");
        assert_eq!(opt_text(Some(0i16), "°C").as_str(), "0 °C");
    }

    #[test]
    fn age_formatting_is_explicit() {
        assert_eq!(format_age(u64::MAX), "—");
        assert_eq!(format_age(240), "240 ms 前");
        assert_eq!(format_age(3_500), "3 s 前");
    }

    #[test]
    fn mode_round_trips_through_the_ui_index() {
        for index in 0..4 {
            let mode = mode_from_index(index).expect("mode");
            assert_eq!(mode_index(mode), index);
        }
        assert!(mode_from_index(9).is_none());
    }
}
