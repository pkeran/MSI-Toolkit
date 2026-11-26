use std::ops::{BitOr, BitOrAssign};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityFlags(u32);

impl CapabilityFlags {
    pub const NONE: Self = Self(0);
    pub const PERFORMANCE_MODE: Self = Self(1 << 0);
    pub const FAN_CURVE: Self = Self(1 << 1);
    pub const COOLER_BOOST: Self = Self(1 << 2);
    pub const CHARGE_LIMIT: Self = Self(1 << 3);
    pub const DISPLAY_REFRESH_RATE: Self = Self(1 << 4);
    pub const DISPLAY_BRIGHTNESS: Self = Self(1 << 5);
    pub const COLOR_PROFILE: Self = Self(1 << 6);
    pub const KEYBOARD_LIGHTING: Self = Self(1 << 7);
    pub const GPU_MUX: Self = Self(1 << 8);
    pub const TELEMETRY: Self = Self(1 << 9);
    pub const OSD: Self = Self(1 << 10);
    pub const AUTOMATION: Self = Self(1 << 11);

    pub const fn from_bits_retain(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

impl BitOr for CapabilityFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CapabilityFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DaemonStatus {
    #[default]
    Starting,
    Ready,
    Degraded,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EcWriteStatus {
    #[default]
    Unavailable,
    ReadOnlyFirmware,
    UnverifiedMapping,
    ConflictDetected,
    Enabled,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PerformanceMode {
    SuperBattery,
    Silent,
    Balanced,
    ExtremePerformance,
}

impl PerformanceMode {
    pub const fn protocol_name(self) -> &'static str {
        match self {
            Self::SuperBattery => "superBattery",
            Self::Silent => "silent",
            Self::Balanced => "balanced",
            Self::ExtremePerformance => "extremePerformance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PowerSource {
    Ac,
    Battery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GpuMuxMode {
    Hybrid,
    Discrete,
    Integrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuMuxState {
    pub current: GpuMuxMode,
    pub pending: Option<GpuMuxMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FanChannel {
    Cpu,
    Gpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ControlTarget {
    PerformanceMode {},
    FanCurve { channel: FanChannel },
    CoolerBoost {},
    ChargeLimit {},
    DisplayRefreshRate {},
    DisplayBrightness {},
    ColorProfile {},
    KeyboardLighting {},
}

impl ControlTarget {
    pub const fn capability(self) -> CapabilityFlags {
        match self {
            Self::PerformanceMode {} => CapabilityFlags::PERFORMANCE_MODE,
            Self::FanCurve { .. } => CapabilityFlags::FAN_CURVE,
            Self::CoolerBoost {} => CapabilityFlags::COOLER_BOOST,
            Self::ChargeLimit {} => CapabilityFlags::CHARGE_LIMIT,
            Self::DisplayRefreshRate {} => CapabilityFlags::DISPLAY_REFRESH_RATE,
            Self::DisplayBrightness {} => CapabilityFlags::DISPLAY_BRIGHTNESS,
            Self::ColorProfile {} => CapabilityFlags::COLOR_PROFILE,
            Self::KeyboardLighting {} => CapabilityFlags::KEYBOARD_LIGHTING,
        }
    }
}

pub const FAN_POINT_COUNT: usize = 6;
pub const MAX_FAN_TEMPERATURE_C: u8 = 100;
pub const MAX_FAN_DUTY_PERCENT: u8 = 150;
pub const DEFAULT_MAX_FAN_DUTY_PERCENT: u8 = 100;
pub const KEYBOARD_ZONE_COUNT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FanPoint {
    pub temperature_c: u8,
    pub duty_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FanCurve {
    pub points: [FanPoint; FAN_POINT_COUNT],
}

impl FanCurve {
    pub fn validate_shape(&self) -> Result<(), FanCurveError> {
        for (index, point) in self.points.iter().enumerate() {
            if point.temperature_c > MAX_FAN_TEMPERATURE_C
                || point.duty_percent > MAX_FAN_DUTY_PERCENT
            {
                return Err(FanCurveError::OutOfRange { index });
            }
            if index > 0 {
                let previous = self.points[index - 1];
                if previous.temperature_c >= point.temperature_c {
                    return Err(FanCurveError::TemperatureOrder { index });
                }
                if previous.duty_percent > point.duty_percent {
                    return Err(FanCurveError::DutyOrder { index });
                }
            }
        }
        Ok(())
    }

    pub fn validate_against(&self, floor: &Self) -> Result<(), FanCurveError> {
        self.validate_with_limits(&FanCurveLimits {
            floor: *floor,
            max_duty_percent: DEFAULT_MAX_FAN_DUTY_PERCENT,
            fixed_anchor: false,
        })
    }

    pub fn validate_with_limits(&self, limits: &FanCurveLimits) -> Result<(), FanCurveError> {
        limits.floor.validate_shape()?;
        if limits.max_duty_percent == 0 || limits.max_duty_percent > MAX_FAN_DUTY_PERCENT {
            return Err(FanCurveError::InvalidDutyCeiling);
        }
        if limits.floor.points[FAN_POINT_COUNT - 1].duty_percent == 0 {
            return Err(FanCurveError::MissingSafetyFloor);
        }
        self.validate_shape()?;
        for (index, (point, minimum)) in self.points.iter().zip(limits.floor.points).enumerate() {
            if point.temperature_c != minimum.temperature_c {
                return Err(FanCurveError::UnsupportedTemperature { index });
            }
            if point.duty_percent > limits.max_duty_percent {
                return Err(FanCurveError::OutOfRange { index });
            }
            if limits.fixed_anchor && index == 0 && point.duty_percent != minimum.duty_percent {
                return Err(FanCurveError::FixedAnchor);
            }
            if point.duty_percent < minimum.duty_percent {
                return Err(FanCurveError::BelowSafetyFloor {
                    index,
                    minimum: minimum.duty_percent,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FanCurveLimits {
    pub floor: FanCurve,
    pub max_duty_percent: u8,
    pub fixed_anchor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FanCurveError {
    #[error("fan point {index} is out of range")]
    OutOfRange { index: usize },
    #[error("fan temperatures must strictly increase at point {index}")]
    TemperatureOrder { index: usize },
    #[error("fan duty must not decrease at point {index}")]
    DutyOrder { index: usize },
    #[error("the hottest fan point must have a nonzero safety floor")]
    MissingSafetyFloor,
    #[error("mapped fan duty ceiling is out of range")]
    InvalidDutyCeiling,
    #[error("the verified fan anchor is fixed")]
    FixedAnchor,
    #[error("temperature at fan point {index} does not match a verified knot")]
    UnsupportedTemperature { index: usize },
    #[error("fan duty at point {index} is below the {minimum}% safety floor")]
    BelowSafetyFloor { index: usize, minimum: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FanState {
    pub curve: FanCurve,
    pub limits: Option<FanCurveLimits>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareIdentity {
    pub board_id: Option<String>,
    pub ec_firmware: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatteryDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub designed_capacity_mwh: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_charged_capacity_mwh: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_capacity_mwh: Option<u32>,
    /// Health in tenths of a percent (e.g. 998 = 99.8%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_tenths_percent: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voltage_mv: Option<u32>,
    /// Signed charge/discharge rate in mW (+ charging, - discharging).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_mw: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charging: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareState {
    pub identity: HardwareIdentity,
    pub supported_performance_modes: Vec<PerformanceMode>,
    pub performance_mode: Option<PerformanceMode>,
    pub cpu_fan: Option<FanState>,
    pub gpu_fan: Option<FanState>,
    pub cooler_boost: Option<bool>,
    pub power_source: Option<PowerSource>,
    pub battery_percent: Option<u8>,
    pub charge_limit_percent: Option<u8>,
    pub supported_charge_limits: Vec<u8>,
    pub display_refresh_hz: Option<u16>,
    pub supported_refresh_rates: Vec<u16>,
    pub display_brightness_percent: Option<u8>,
    pub color_profile_id: Option<String>,
    pub supported_color_profiles: Vec<String>,
    pub keyboard_zones: Option<[Rgb8; KEYBOARD_ZONE_COUNT]>,
    pub gpu_mux: Option<GpuMuxState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_detail: Option<BatteryDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_conflict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSnapshot {
    pub revision: u64,
    pub status: DaemonStatus,
    pub ec_write_status: EcWriteStatus,
    pub capabilities: CapabilityFlags,
    pub writable: CapabilityFlags,
    pub hardware: HardwareState,
    pub manual_overrides: Vec<ControlTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySample {
    pub sampled_at_unix_ms: u64,
    pub cpu_temperature_c: Option<i16>,
    pub gpu_temperature_c: Option<i16>,
    pub cpu_fan_rpm: Option<u16>,
    pub gpu_fan_rpm: Option<u16>,
    pub cpu_reported_duty_percent: Option<u8>,
    pub gpu_reported_duty_percent: Option<u8>,
    pub gpu_power_mw: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ControlChange {
    SetPerformanceMode { mode: PerformanceMode },
    SetFanCurve { channel: FanChannel, curve: FanCurve },
    SetCoolerBoost { enabled: bool },
    SetChargeLimit { percent: u8 },
    SetDisplayRefreshRate { hz: u16 },
    SetDisplayBrightness { percent: u8 },
    SetColorProfile { profile_id: String },
    SetKeyboardLighting { zones: [Rgb8; KEYBOARD_ZONE_COUNT] },
}

impl ControlChange {
    pub const fn target(&self) -> ControlTarget {
        match self {
            Self::SetPerformanceMode { .. } => ControlTarget::PerformanceMode {},
            Self::SetFanCurve { channel, .. } => ControlTarget::FanCurve { channel: *channel },
            Self::SetCoolerBoost { .. } => ControlTarget::CoolerBoost {},
            Self::SetChargeLimit { .. } => ControlTarget::ChargeLimit {},
            Self::SetDisplayRefreshRate { .. } => ControlTarget::DisplayRefreshRate {},
            Self::SetDisplayBrightness { .. } => ControlTarget::DisplayBrightness {},
            Self::SetColorProfile { .. } => ControlTarget::ColorProfile {},
            Self::SetKeyboardLighting { .. } => ControlTarget::KeyboardLighting {},
        }
    }

    pub const fn capability(&self) -> CapabilityFlags {
        self.target().capability()
    }

    pub fn validate_parameters(&self) -> Result<(), ControlError> {
        match self {
            Self::SetFanCurve { curve, .. } => curve.validate_shape()?,
            Self::SetChargeLimit { percent } if !(1..=100).contains(percent) => {
                return Err(ControlError::InvalidField("percent"));
            }
            Self::SetDisplayBrightness { percent } if *percent > 100 => {
                return Err(ControlError::InvalidField("percent"));
            }
            Self::SetDisplayRefreshRate { hz } if *hz == 0 => {
                return Err(ControlError::InvalidField("hz"));
            }
            Self::SetColorProfile { profile_id }
                if profile_id.is_empty()
                    || profile_id.len() > 64
                    || !profile_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) =>
            {
                return Err(ControlError::InvalidField("profileId"));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn validate_against(&self, state: &HardwareState) -> Result<(), ControlError> {
        self.validate_parameters()?;
        match self {
            Self::SetPerformanceMode { mode } if !state.supported_performance_modes.contains(mode) => {
                return Err(ControlError::UnsupportedValue("mode"));
            }
            Self::SetFanCurve { channel, curve } => {
                let fan = match channel {
                    FanChannel::Cpu => state.cpu_fan,
                    FanChannel::Gpu => state.gpu_fan,
                };
                let limits = fan.and_then(|fan| fan.limits)
                    .ok_or(ControlError::UnsupportedValue("channel"))?;
                curve.validate_with_limits(&limits)?;
            }
            Self::SetChargeLimit { percent } if !state.supported_charge_limits.contains(percent) => {
                return Err(ControlError::UnsupportedValue("percent"));
            }
            Self::SetDisplayRefreshRate { hz } if !state.supported_refresh_rates.contains(hz) => {
                return Err(ControlError::UnsupportedValue("hz"));
            }
            Self::SetColorProfile { profile_id } if !state.supported_color_profiles.contains(profile_id) => {
                return Err(ControlError::UnsupportedValue("profileId"));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ControlError {
    #[error("invalid field: {0}")]
    InvalidField(&'static str),
    #[error("value for {0} is not in the verified capability set")]
    UnsupportedValue(&'static str),
    #[error(transparent)]
    FanCurve(#[from] FanCurveError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OsdKind {
    PerformanceMode,
    CoolerBoost,
    DisplayRefreshRate,
    AutomationApplied,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_flags_preserve_unknown_bits_on_wire() {
        let unknown = CapabilityFlags::from_bits_retain(1 << 31);
        let mut flags = CapabilityFlags::FAN_CURVE | unknown;
        flags |= CapabilityFlags::TELEMETRY;
        assert!(flags.contains(CapabilityFlags::FAN_CURVE | CapabilityFlags::TELEMETRY));
        assert!(!flags.contains(CapabilityFlags::GPU_MUX));
        let json = serde_json::to_string(&flags).unwrap();
        assert_eq!(json, flags.bits().to_string());
        assert_eq!(serde_json::from_str::<CapabilityFlags>(&json).unwrap(), flags);
    }

    #[test]
    fn color_profile_id_is_not_a_path() {
        for profile_id in ["../factory.icm", r"C:\factory.icm", ""] {
            assert_eq!(
                ControlChange::SetColorProfile { profile_id: profile_id.into() }
                    .validate_parameters(),
                Err(ControlError::InvalidField("profileId")),
            );
        }
        assert!(ControlChange::SetColorProfile { profile_id: "factory-srgb".into() }
            .validate_parameters()
            .is_ok());
    }

    #[test]
    fn no_values_are_fabricated_in_the_initial_snapshot() {
        let snapshot = SystemSnapshot::default();
        assert_eq!(snapshot.status, DaemonStatus::Starting);
        assert_eq!(snapshot.ec_write_status, EcWriteStatus::Unavailable);
        assert_eq!(snapshot.capabilities, CapabilityFlags::NONE);
        assert_eq!(snapshot.writable, CapabilityFlags::NONE);
        assert_eq!(snapshot.hardware.performance_mode, None);
        assert_eq!(snapshot.hardware.cpu_fan, None);
    }

    #[test]
    fn state_based_validation_uses_verified_choices() {
        let state = HardwareState {
            supported_charge_limits: vec![60, 80, 100],
            supported_refresh_rates: vec![60, 240],
            supported_performance_modes: vec![PerformanceMode::Balanced],
            ..HardwareState::default()
        };
        assert_eq!(ControlChange::SetChargeLimit { percent: 1 }.validate_against(&state), Err(ControlError::UnsupportedValue("percent")));
        assert!(ControlChange::SetChargeLimit { percent: 100 }.validate_against(&state).is_ok());
        assert_eq!(ControlChange::SetDisplayRefreshRate { hz: 144 }.validate_against(&state), Err(ControlError::UnsupportedValue("hz")));
        assert!(ControlChange::SetDisplayRefreshRate { hz: 240 }.validate_against(&state).is_ok());
        assert_eq!(ControlChange::SetPerformanceMode { mode: PerformanceMode::Silent }.validate_against(&state), Err(ControlError::UnsupportedValue("mode")));
    }

    #[test]
    fn mode_wire_text_matches_environment_value() {
        for mode in [PerformanceMode::SuperBattery, PerformanceMode::Silent, PerformanceMode::Balanced, PerformanceMode::ExtremePerformance] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.protocol_name());
        }
    }
}
