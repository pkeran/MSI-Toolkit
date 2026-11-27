use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use msilt_core::{
    AuthorizedEcWrite, BatteryDetail, CapabilityFlags, EcRegisterIo, FanChannel, GpuMuxMode,
    GpuMuxState, HalError, HalResult, HardwareRead, HardwareReport, HardwareState,
    KEYBOARD_ZONE_COUNT, PlatformControl, Rgb8, TelemetrySample,
};
use serde::Serialize;

use crate::battery::{self, BatteryReport};
use crate::display::{self, DisplayReport, RefreshRate};
use crate::ec_map_loader::{self, MapIdentity};
use crate::executor::SerialExecutor;
use crate::hid_rgb::{self, HidRgbReport};
use crate::identity::{self, ConflictReport, SystemIdentity};
use crate::ms17s3;
use crate::nvml::{self, NvmlReport};
use crate::wmi_acpi::{AcpiReader, AcpiReport, Poll};
use crate::{Diagnostic, DiagnosticKind};

#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    pub acpi: bool,
    pub include_nvml: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ControlOptions {
    /// Enable the audited MSI_ACPI channel. EC writes still require a Core token.
    pub acpi: bool,
    /// Explicitly accept concurrent OEM lighting software; cannot bypass a sharing violation.
    pub allow_oem_lighting_conflict: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EcMapReport {
    pub validated: bool,
    pub identity: Option<MapIdentity>,
    pub board_and_bios_match: Option<bool>,
    writes_enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub schema_version: u16,
    pub sampled_at_unix_ms: u64,
    pub identity: SystemIdentity,
    pub ec_map: EcMapReport,
    pub display: DisplayReport,
    pub battery: BatteryReport,
    pub hid: HidRgbReport,
    pub conflicts: ConflictReport,
    pub acpi: Option<AcpiReport>,
    pub nvml: Option<NvmlReport>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ProbeReport {
    pub fn has_failures(&self) -> bool {
        !self.diagnostics.is_empty()
            || self.acpi.as_ref().is_some_and(|acpi| {
                !acpi.identity_matched
                    || !acpi.diagnostics.is_empty()
                    || acpi.methods.iter().any(|method| method.error.is_some())
            })
            || self
                .nvml
                .as_ref()
                .is_some_and(|nvml| !nvml.completed || !nvml.diagnostics.is_empty())
    }
}

/// `new` and `default` are read-only; `for_control` enables native settings.
pub struct WindowsHal {
    acpi_enabled: bool,
    nvml_once: bool,
    acpi: Option<AcpiReader>,
    last_diagnostics: Vec<Diagnostic>,
    control: Option<ControlOptions>,
    keyboard_zones: [Rgb8; KEYBOARD_ZONE_COUNT],
}

const INITIAL_KEYBOARD: [Rgb8; KEYBOARD_ZONE_COUNT] = [Rgb8 {
    r: 208,
    g: 32,
    b: 48,
}; KEYBOARD_ZONE_COUNT];

impl Default for WindowsHal {
    fn default() -> Self {
        Self {
            acpi_enabled: false,
            nvml_once: false,
            acpi: None,
            last_diagnostics: Vec::new(),
            control: None,
            keyboard_zones: INITIAL_KEYBOARD,
        }
    }
}

impl WindowsHal {
    pub fn new(options: ProbeOptions) -> Self {
        Self {
            acpi_enabled: options.acpi,
            nvml_once: options.include_nvml,
            ..Self::default()
        }
    }

    pub fn for_control(options: ControlOptions) -> Self {
        Self {
            acpi_enabled: options.acpi,
            control: Some(options),
            ..Self::default()
        }
    }

    /// Enables non-EC controls while independently opting into read-only ACPI.
    /// The ACPI write gate remains false; daemon EC transactions need a Core permit.
    pub fn for_platform_control(read_acpi: bool) -> Self {
        Self {
            acpi_enabled: read_acpi,
            control: Some(ControlOptions::default()),
            ..Self::default()
        }
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.last_diagnostics
    }

    pub fn diagnose(&mut self) -> ProbeReport {
        let mut report = self.read_domains(true);
        report.ec_map = match ec_map_loader::load_ms_17s3() {
            Ok(map) => {
                let identity = &report.identity;
                let board_and_bios_match = identity
                    .board_id
                    .as_deref()
                    .zip(identity.board_revision.as_deref())
                    .zip(identity.bios_version.as_deref())
                    .map(|((id, rev), bios)| {
                        id == map.identity.board_id
                            && rev == map.identity.board_revision
                            && bios == map.identity.bios_version
                    });
                EcMapReport {
                    validated: true,
                    identity: Some(map.identity),
                    board_and_bios_match,
                    writes_enabled: false,
                }
            }
            Err(error) => {
                report.diagnostics.push(Diagnostic::invalid(
                    "MS-17S3 EC envelope",
                    error.to_string(),
                ));
                EcMapReport::default()
            }
        };
        report.acpi = self
            .acpi_enabled
            .then(|| self.read_acpi(Poll::Full, report.sampled_at_unix_ms));
        report.nvml = self.nvml_sample();
        self.last_diagnostics = report.diagnostics.clone();
        if let Some(acpi) = &report.acpi {
            self.last_diagnostics.extend(acpi_errors(acpi).cloned());
        }
        if let Some(nvml) = &report.nvml {
            self.last_diagnostics
                .extend(nvml.diagnostics.iter().cloned());
        }
        report
    }

    fn read_domains(&self, include_factory_profiles: bool) -> ProbeReport {
        let mut diagnostics = Vec::new();
        let sampled_at_unix_ms = timestamp(&mut diagnostics);
        static IDENTITY: Lane = OnceLock::new();
        static BATTERY: Lane = OnceLock::new();

        let (identity, conflicts) = observe(
            &IDENTITY,
            "identity and SCM",
            3,
            &mut diagnostics,
            |_, issues| (identity::inspect(issues), identity::conflicts(issues)),
        );
        let inspect_controls = self.control.is_some();
        let display = observe(
            &DISPLAY,
            "display and ICC",
            4,
            &mut diagnostics,
            move |deadline, issues| {
                display::inspect(deadline, issues, inspect_controls, include_factory_profiles)
            },
        );
        let battery = observe(
            &BATTERY,
            "battery interrogation",
            3,
            &mut diagnostics,
            battery::inspect,
        );
        let mut hid = observe(
            &HID,
            "HID descriptor enumeration",
            3,
            &mut diagnostics,
            hid_rgb::inspect,
        );
        hid.oem_conflict = hid_rgb::rgb_conflict(&conflicts);
        ProbeReport {
            schema_version: 1,
            sampled_at_unix_ms,
            identity,
            ec_map: EcMapReport::default(),
            display,
            battery,
            hid,
            conflicts,
            acpi: None,
            nvml: None,
            diagnostics,
        }
    }

    fn acpi_reader(&mut self) -> Result<&mut AcpiReader, Diagnostic> {
        if !self.acpi_enabled {
            return Err(Diagnostic::new("MSI_ACPI", DiagnosticKind::Unavailable)
                .detail("ACPI was not explicitly enabled"));
        }
        if self.acpi.is_none() {
            self.acpi = Some(AcpiReader::authorize()?);
        }
        Ok(self.acpi.as_mut().expect("authorized ACPI reader"))
    }

    fn read_acpi(&mut self, poll: Poll, timestamp: u64) -> AcpiReport {
        match self.acpi_reader() {
            Ok(reader) => reader.poll(poll, timestamp),
            Err(error) => AcpiReport {
                diagnostics: vec![error],
                ..Default::default()
            },
        }
    }

    fn nvml_sample(&mut self) -> Option<NvmlReport> {
        if !std::mem::take(&mut self.nvml_once) {
            return None;
        }
        static NVML: Lane = OnceLock::new();
        Some(
            match on_lane(&NVML, "NVML one-shot", Duration::from_secs(6), |deadline| {
                Ok(nvml::inspect(deadline))
            }) {
                Ok(report) => report,
                Err(error) => NvmlReport {
                    diagnostics: vec![error],
                    ..Default::default()
                },
            },
        )
    }

    fn report_result<T>(&mut self, result: Result<T, Diagnostic>) -> HalResult<T> {
        self.last_diagnostics.clear();
        result.map_err(|error| {
            let mapped = HalError::from(&error);
            self.last_diagnostics.push(error);
            mapped
        })
    }

    fn platform<T>(
        &mut self,
        call: impl FnOnce(ControlOptions) -> Result<T, Diagnostic>,
    ) -> HalResult<T> {
        let result = self
            .control
            .ok_or_else(|| {
                Diagnostic::new("Windows platform control", DiagnosticKind::Unsupported)
                    .detail("use the explicit control constructor")
            })
            .and_then(call);
        self.report_result(result)
    }
}

type Lane = OnceLock<Result<SerialExecutor<()>, Diagnostic>>;
static DISPLAY: Lane = OnceLock::new();
static HID: Lane = OnceLock::new();

fn on_lane<T: Send + 'static>(
    lane: &Lane,
    name: &'static str,
    budget: Duration,
    read: impl FnOnce(Instant) -> Result<T, Diagnostic> + Send + 'static,
) -> Result<T, Diagnostic> {
    lane.get_or_init(|| SerialExecutor::spawn(name, || ()))
        .as_ref()
        .map_err(Clone::clone)?
        .run(name, budget, move |_, deadline| read(deadline))
}

fn observe<T: Default + Send + 'static>(
    lane: &Lane,
    name: &'static str,
    seconds: u64,
    diagnostics: &mut Vec<Diagnostic>,
    read: impl FnOnce(Instant, &mut Vec<Diagnostic>) -> T + Send + 'static,
) -> T {
    match on_lane(lane, name, Duration::from_secs(seconds), move |deadline| {
        let mut diagnostics = Vec::new();
        let result = read(deadline, &mut diagnostics);
        Ok((result, diagnostics))
    }) {
        Ok((value, issues)) => {
            diagnostics.extend(issues);
            value
        }
        Err(error) => {
            diagnostics.push(error);
            T::default()
        }
    }
}

fn timestamp(issues: &mut Vec<Diagnostic>) -> u64 {
    match SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
    {
        Some(ms) => ms,
        None => {
            issues.push(Diagnostic::invalid(
                "system clock",
                "cannot represent the current Unix timestamp",
            ));
            0
        }
    }
}

fn acpi_errors(acpi: &AcpiReport) -> impl Iterator<Item = &Diagnostic> {
    acpi.methods
        .iter()
        .filter_map(|method| method.error.as_ref())
        .chain(acpi.diagnostics.iter())
}

fn hardware_state(
    report: &ProbeReport,
    acpi: Option<&AcpiReport>,
    keyboard_zones: Option<[Rgb8; KEYBOARD_ZONE_COUNT]>,
) -> HardwareReport {
    let control = keyboard_zones.is_some();
    let mut state = HardwareState::default();
    let mut readable = CapabilityFlags::NONE;
    let mut controls_detected = CapabilityFlags::NONE;
    state
        .identity
        .board_id
        .clone_from(&report.identity.board_id);
    state.power_source = report.battery.power_source;
    state.battery_percent = report.battery.system_percent;
    state.battery_detail = report.battery.packs.first().map(battery_detail);
    state.keyboard_conflict = report.hid.oem_conflict;
    if let Some(acpi) = acpi.filter(|a| a.identity_matched) {
        state.identity.ec_firmware.clone_from(&acpi.ec_firmware);
        readable |= CapabilityFlags::TELEMETRY;
        if let Some(bytes) = &acpi.control_bytes {
            let mut register = |flag, can_control| {
                readable |= flag;
                if can_control {
                    controls_detected |= flag;
                }
            };
            if let Some(raw) = bytes.scalar(ms17s3::COOLER) {
                state.cooler_boost = Some(raw & ms17s3::COOLER_MASK != 0);
                register(CapabilityFlags::COOLER_BOOST, true);
            }
            if let Some(mode) = bytes
                .scalar(ms17s3::SHIFT)
                .zip(bytes.scalar(ms17s3::FAN_MODE))
                .zip(bytes.scalar(ms17s3::BATTERY_MODE))
                .and_then(|((shift, fan), battery)| ms17s3::decode_mode(shift, fan, battery))
            {
                state.performance_mode = Some(mode);
                state.supported_performance_modes = ms17s3::MODES.to_vec();
                register(CapabilityFlags::PERFORMANCE_MODE, true);
            }
            if let Some(percent) = bytes.scalar(ms17s3::CHARGE).and_then(ms17s3::decode_charge) {
                state.charge_limit_percent = Some(percent);
                state.supported_charge_limits = ms17s3::CHARGE_LIMITS.to_vec();
                register(
                    CapabilityFlags::CHARGE_LIMIT,
                    ms17s3::CHARGE_LIMITS.contains(&percent),
                );
            }
            state.cpu_fan = bytes.fan(FanChannel::Cpu);
            state.gpu_fan = bytes.fan(FanChannel::Gpu);
            let fans = [state.cpu_fan, state.gpu_fan];
            if fans.iter().any(Option::is_some) {
                register(
                    CapabilityFlags::FAN_CURVE,
                    fans.iter().flatten().any(|fan| fan.limits.is_some()),
                );
            }
            // The CCD panel route supplies an independent check on the EC MUX indicator.
            let vendor = report
                .display
                .panel
                .as_ref()
                .and_then(|panel| panel.adapter_vendor_id);
            if let Some(current) = bytes
                .scalar(ms17s3::MUX)
                .and_then(|raw| mux_indicator(raw, vendor))
            {
                state.gpu_mux = Some(GpuMuxState {
                    current,
                    pending: None,
                });
                register(CapabilityFlags::GPU_MUX, false);
            }
        }
    }
    if let Some(panel) = &report.display.panel {
        state.panel_name = panel_name(panel);
        state.display_refresh_hz = panel.refresh.and_then(RefreshRate::rounded_hz);
        state
            .supported_refresh_rates
            .clone_from(&panel.supported_refresh_hz);
        if state.display_refresh_hz.is_some() {
            readable |= CapabilityFlags::DISPLAY_REFRESH_RATE;
        }
        if control && !state.supported_refresh_rates.is_empty() {
            controls_detected |= CapabilityFlags::DISPLAY_REFRESH_RATE;
        }
    }
    if let Some(brightness) = &report.display.brightness {
        state.display_brightness_percent = Some(brightness.current_percent);
        readable |= CapabilityFlags::DISPLAY_BRIGHTNESS;
        if control {
            controls_detected |= CapabilityFlags::DISPLAY_BRIGHTNESS;
        }
    }
    state
        .color_profile_id
        .clone_from(&report.display.color.default_profile_id);
    if state.color_profile_id.is_some() || report.display.color.associated_profile_ids.is_some() {
        readable |= CapabilityFlags::COLOR_PROFILE;
    }
    // A read-only snapshot should still describe available color choices.
    if control {
        state
            .supported_color_profiles
            .clone_from(&report.display.color.settable_profile_ids);
        if !state.supported_color_profiles.is_empty() {
            controls_detected |= CapabilityFlags::COLOR_PROFILE;
        }
        if report.hid.enumeration_complete
            && report
                .hid
                .collections
                .iter()
                .filter(|c| c.lighting_compatible())
                .count()
                == 1
        {
            state.keyboard_zones = keyboard_zones;
            readable |= CapabilityFlags::KEYBOARD_LIGHTING;
            controls_detected |= CapabilityFlags::KEYBOARD_LIGHTING;
        }
    } else {
        state.supported_color_profiles = report
            .display
            .color
            .installed_profile_ids
            .clone()
            .unwrap_or_default();
    }
    HardwareReport {
        state,
        readable,
        controls_detected,
    }
}

fn battery_detail(pack: &battery::BatteryPack) -> BatteryDetail {
    let absolute = pack.capacity_unit == battery::CapacityUnit::MilliWattHours;
    BatteryDetail {
        designed_capacity_mwh: pack.designed_capacity.filter(|_| absolute),
        full_charged_capacity_mwh: pack.full_charged_capacity.filter(|_| absolute),
        remaining_capacity_mwh: pack.remaining_capacity.filter(|_| absolute),
        health_tenths_percent: pack
            .health_percent
            .map(|value| (value * 10.0).round())
            .filter(|value| (0.0..=f64::from(u16::MAX)).contains(value))
            .map(|value| value as u16),
        cycle_count: pack.cycle_count,
        voltage_mv: pack.voltage_mv,
        rate_mw: pack
            .power_w
            .map(|value| (value * 1000.0).round())
            .filter(|value| (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(value))
            .map(|value| value as i32),
        charging: pack.charging,
    }
}

fn panel_name(panel: &display::InternalPanel) -> Option<String> {
    let resolution = panel
        .width
        .zip(panel.height)
        .map(|(w, h)| format!("{w}x{h}"));
    match (&panel.edid_id, resolution) {
        (Some(id), Some(resolution)) => Some(format!("{id} · {resolution}")),
        (Some(id), None) => Some(id.clone()),
        (None, resolution) => resolution,
    }
}

fn mux_indicator(raw: u8, panel_vendor: Option<u16>) -> Option<GpuMuxMode> {
    match (raw, panel_vendor) {
        (0, Some(0x8086)) => Some(GpuMuxMode::Hybrid),
        (1, Some(0x10DE)) => Some(GpuMuxMode::Discrete),
        (2, Some(0x8086)) => Some(GpuMuxMode::Integrated),
        _ => None,
    }
}

impl HardwareRead for WindowsHal {
    fn inspect(&mut self) -> HalResult<HardwareReport> {
        let report = self.read_domains(false);
        let acpi = self
            .acpi_enabled
            .then(|| self.read_acpi(Poll::Snapshot, report.sampled_at_unix_ms));
        let hardware = hardware_state(
            &report,
            acpi.as_ref(),
            self.control.map(|_| self.keyboard_zones),
        );
        self.last_diagnostics = report.diagnostics;
        if let Some(acpi) = &acpi {
            self.last_diagnostics.extend(acpi_errors(acpi).cloned());
        }
        if hardware == HardwareReport::default() {
            return Err(self
                .last_diagnostics
                .first()
                .map_or(HalError::Unavailable, HalError::from));
        }
        Ok(hardware)
    }

    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
        self.last_diagnostics.clear();
        let ms = timestamp(&mut self.last_diagnostics);
        let mut sample = TelemetrySample {
            sampled_at_unix_ms: ms,
            ..Default::default()
        };
        if self.acpi_enabled {
            let report = self.read_acpi(Poll::Telemetry, ms);
            self.last_diagnostics.extend(acpi_errors(&report).cloned());
            sample = report.telemetry;
        }
        if let Some(nvml) = self.nvml_sample() {
            if let [gpu] = nvml.devices.as_slice() {
                sample.gpu_power_mw = gpu.power_mw;
                if sample.gpu_temperature_c.is_none() {
                    sample.gpu_temperature_c =
                        gpu.temperature_c.and_then(|t| i16::try_from(t).ok());
                }
            }
            self.last_diagnostics.extend(nvml.diagnostics);
        }
        if sample.cpu_temperature_c.is_none()
            && sample.gpu_temperature_c.is_none()
            && sample.cpu_fan_rpm.is_none()
            && sample.gpu_fan_rpm.is_none()
            && sample.cpu_reported_duty_percent.is_none()
            && sample.gpu_reported_duty_percent.is_none()
            && sample.gpu_power_mw.is_none()
        {
            return Err(self
                .last_diagnostics
                .first()
                .map_or(HalError::Unavailable, HalError::from));
        }
        Ok(sample)
    }
}

impl EcRegisterIo for WindowsHal {
    fn read_byte(&mut self, address: u8) -> HalResult<u8> {
        let result = self
            .acpi_reader()
            .and_then(|reader| reader.read_byte(address));
        self.report_result(result)
    }

    fn write_byte(&mut self, permit: &AuthorizedEcWrite) -> HalResult<()> {
        let result = if self.control.is_some_and(|options| options.acpi) {
            self.acpi_reader()
                .and_then(|reader| reader.write_byte(permit))
        } else {
            Err(Diagnostic::new("Set_Data", DiagnosticKind::Unsupported)
                .detail("EC writes require explicit ACPI control access"))
        };
        self.report_result(result)
    }
}

impl PlatformControl for WindowsHal {
    fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
        self.platform(|_| {
            on_lane(
                &DISPLAY,
                "display refresh",
                Duration::from_secs(5),
                move |deadline| display::set_display_refresh_rate(hz, deadline),
            )
        })
    }
    fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8> {
        self.platform(|_| {
            crate::wmi_acpi::validate_brightness(percent)?;
            on_lane(
                &DISPLAY,
                "display brightness",
                Duration::from_secs(5),
                move |deadline| display::set_display_brightness(percent, deadline),
            )
        })
    }
    fn set_color_profile(&mut self, profile_id: &str) -> HalResult<()> {
        self.platform(|_| {
            display::validate_color_profile_id(profile_id)?;
            let profile_id = profile_id.to_owned();
            on_lane(
                &DISPLAY,
                "display color",
                Duration::from_secs(5),
                move |deadline| display::set_color_profile(&profile_id, deadline),
            )
        })
    }
    fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
        let zones = *zones;
        self.platform(|options| {
            on_lane(
                &HID,
                "keyboard lighting",
                Duration::from_secs(5),
                move |deadline| {
                    hid_rgb::set_keyboard_lighting(
                        &zones,
                        options.allow_oem_lighting_conflict,
                        deadline,
                    )
                },
            )
        })?;
        self.keyboard_zones = zones;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_telemetry_and_mutation_do_not_activate_native_channels() {
        let mut hal = WindowsHal::default();
        assert_eq!(hal.sample_telemetry(), Err(HalError::Unavailable));
        assert_eq!(hal.read_byte(0xD1), Err(HalError::Unavailable));
        assert_eq!(hal.set_display_refresh_rate(60), Err(HalError::Unsupported));
        assert_eq!(hal.set_display_brightness(40), Err(HalError::Unsupported));
        assert_eq!(
            hal.set_color_profile("TrueColor_native"),
            Err(HalError::Unsupported)
        );
        assert_eq!(
            hal.set_keyboard_lighting(&[Rgb8 { r: 0, g: 0, b: 0 }; KEYBOARD_ZONE_COUNT]),
            Err(HalError::Unsupported)
        );
        assert!(hal.acpi.is_none());
        assert!(!hal.nvml_once);
    }

    #[test]
    fn control_constructor_still_rejects_invalid_input_before_native_io() {
        let mut hal = WindowsHal::for_control(ControlOptions::default());
        assert_eq!(hal.set_display_brightness(255), Err(HalError::Io));
        assert_eq!(hal.diagnostics()[0].kind, DiagnosticKind::InvalidData);
        assert_eq!(hal.set_color_profile("../bad.icm"), Err(HalError::Io));
        assert_eq!(hal.diagnostics()[0].kind, DiagnosticKind::InvalidData);
        assert_eq!(hal.read_byte(0xD7), Err(HalError::Unavailable));
        assert!(hal.acpi.is_none());
    }

    #[test]
    fn platform_control_cannot_open_the_ec_write_or_nvml_channel() {
        let hal = WindowsHal::for_platform_control(true);
        assert!(hal.acpi_enabled);
        assert!(!hal.control.unwrap().acpi);
        assert!(!hal.nvml_once);
        assert!(hal.acpi.is_none());
    }

    #[test]
    fn snapshot_uses_physical_capacity_units_and_only_consistent_mux_indicators() {
        use battery::{BatteryPack, CapacityUnit};
        let mut pack = BatteryPack {
            capacity_unit: CapacityUnit::MilliWattHours,
            designed_capacity: Some(90_000),
            full_charged_capacity: Some(81_000),
            remaining_capacity: Some(40_000),
            health_percent: Some(90.0),
            percent: Some(49.4),
            cycle_count: Some(8),
            cycle_count_raw: 8,
            voltage_mv: Some(12_300),
            current_ma: None,
            power_w: Some(-18.423),
            relative_rate: None,
            power_source: Some(msilt_core::PowerSource::Battery),
            charging: Some(false),
            discharging: Some(true),
        };
        let detail = battery_detail(&pack);
        assert_eq!(detail.designed_capacity_mwh, Some(90_000));
        assert_eq!(detail.health_tenths_percent, Some(900));
        assert_eq!(detail.rate_mw, Some(-18_423));
        assert_eq!(detail.charging, Some(false));
        pack.capacity_unit = CapacityUnit::Relative;
        assert_eq!(battery_detail(&pack).designed_capacity_mwh, None);
        assert_eq!(mux_indicator(0, Some(0x10DE)), None);
        assert_eq!(mux_indicator(1, Some(0x10DE)), Some(GpuMuxMode::Discrete));
        assert_eq!(mux_indicator(0xFF, Some(0x8086)), None);
    }
}
