use std::fmt;
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
    pub const GPU_CLOCK_OFFSET: Self = Self(1 << 12);

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

macro_rules! str_enum_serde {
    ($ty:ident, $name:literal, $( $variant:ident => $wire:literal ),+ $(,)?) => {
        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(match self {
                    $( Self::$variant => $wire, )+
                })
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl ::serde::de::Visitor<'_> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        f.write_str($name)
                    }
                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        match v {
                            $( $wire => Ok($ty::$variant), )+
                            _ => Err(E::custom($name)),
                        }
                    }
                }
                d.deserialize_str(V)
            }
        }
    };
}
pub(crate) use str_enum_serde;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonStatus {
    #[default]
    Starting,
    Ready,
    Degraded,
    Stopping,
}
str_enum_serde!(DaemonStatus, "daemon status",
    Starting => "starting",
    Ready => "ready",
    Degraded => "degraded",
    Stopping => "stopping",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EcWriteStatus {
    #[default]
    Unavailable,
    ReadOnlyFirmware,
    UnverifiedMapping,
    ConflictDetected,
    Enabled,
    RecoveryRequired,
}
str_enum_serde!(EcWriteStatus, "ec write status",
    Unavailable => "unavailable",
    ReadOnlyFirmware => "readOnlyFirmware",
    UnverifiedMapping => "unverifiedMapping",
    ConflictDetected => "conflictDetected",
    Enabled => "enabled",
    RecoveryRequired => "recoveryRequired",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceMode {
    SuperBattery,
    Silent,
    Balanced,
    ExtremePerformance,
}
str_enum_serde!(PerformanceMode, "performance mode",
    SuperBattery => "superBattery",
    Silent => "silent",
    Balanced => "balanced",
    ExtremePerformance => "extremePerformance",
);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
}
str_enum_serde!(PowerSource, "power source",
    Ac => "ac",
    Battery => "battery",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMuxMode {
    Hybrid,
    Discrete,
    Integrated,
}
str_enum_serde!(GpuMuxMode, "gpu mux mode",
    Hybrid => "hybrid",
    Discrete => "discrete",
    Integrated => "integrated",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuMuxState {
    pub current: GpuMuxMode,
    pub pending: Option<GpuMuxMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuClockOffsetState {
    pub core_mhz: i16,
    pub mem_mhz: i16,
    pub core_min_mhz: i16,
    pub core_max_mhz: i16,
    pub mem_min_mhz: i16,
    pub mem_max_mhz: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanChannel {
    Cpu,
    Gpu,
}
str_enum_serde!(FanChannel, "fan channel",
    Cpu => "cpu",
    Gpu => "gpu",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Auto,
    Silent,
    Advanced,
}
str_enum_serde!(FanMode, "fan mode",
    Auto => "auto",
    Silent => "silent",
    Advanced => "advanced",
);

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
    GpuClockOffset {},
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
            Self::GpuClockOffset {} => CapabilityFlags::GPU_CLOCK_OFFSET,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardEffect {
    #[default]
    Static,
    Breathing,
    ColorCycle,
    Wave,
}

impl KeyboardEffect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Breathing => "breathing",
            Self::ColorCycle => "colorCycle",
            Self::Wave => "wave",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "static" => Some(Self::Static),
            "breathing" => Some(Self::Breathing),
            "colorCycle" => Some(Self::ColorCycle),
            "wave" => Some(Self::Wave),
            _ => None,
        }
    }
}

impl Serialize for KeyboardEffect {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KeyboardEffect {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = KeyboardEffect;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("keyboard effect")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                KeyboardEffect::parse(v).ok_or_else(|| E::custom("invalid keyboard effect"))
            }
        }
        d.deserialize_str(V)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardSpeed {
    Slow,
    #[default]
    Medium,
    Fast,
}

impl KeyboardSpeed {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Medium => "medium",
            Self::Fast => "fast",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "slow" => Some(Self::Slow),
            "medium" => Some(Self::Medium),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }
}

impl Serialize for KeyboardSpeed {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KeyboardSpeed {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = KeyboardSpeed;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("keyboard speed")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                KeyboardSpeed::parse(v).ok_or_else(|| E::custom("invalid keyboard speed"))
            }
        }
        d.deserialize_str(V)
    }
}

fn is_default_effect(effect: &KeyboardEffect) -> bool {
    matches!(effect, KeyboardEffect::Static)
}

fn is_default_speed(speed: &KeyboardSpeed) -> bool {
    matches!(speed, KeyboardSpeed::Medium)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_mode: Option<FanMode>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_effect: Option<KeyboardEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_speed: Option<KeyboardSpeed>,
    pub gpu_mux: Option<GpuMuxState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_clock_offset: Option<GpuClockOffsetState>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_fan_modes: Vec<PerformanceMode>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_utilization_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_utilization_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_clock_mhz: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_mem_clock_mhz: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_vram_used_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_vram_total_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_power_limit_mw: Option<u32>,
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
    SetKeyboardLighting {
        zones: [Rgb8; KEYBOARD_ZONE_COUNT],
        #[serde(default, skip_serializing_if = "is_default_effect")]
        effect: KeyboardEffect,
        #[serde(default, skip_serializing_if = "is_default_speed")]
        speed: KeyboardSpeed,
    },
    SetGpuClockOffset { core_mhz: i16, mem_mhz: i16 },
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
            Self::SetGpuClockOffset { .. } => ControlTarget::GpuClockOffset {},
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
            Self::SetGpuClockOffset { core_mhz, mem_mhz } => {
                if !(-1000..=1000).contains(core_mhz) {
                    return Err(ControlError::InvalidField("coreMhz"));
                }
                if !(-1000..=1000).contains(mem_mhz) {
                    return Err(ControlError::InvalidField("memMhz"));
                }
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
            Self::SetGpuClockOffset { core_mhz, mem_mhz } => {
                let offset = state.gpu_clock_offset
                    .ok_or(ControlError::UnsupportedValue("gpuClockOffset"))?;
                if !(offset.core_min_mhz..=offset.core_max_mhz).contains(core_mhz) {
                    return Err(ControlError::UnsupportedValue("coreMhz"));
                }
                if !(offset.mem_min_mhz..=offset.mem_max_mhz).contains(mem_mhz) {
                    return Err(ControlError::UnsupportedValue("memMhz"));
                }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdKind {
    PerformanceMode,
    CoolerBoost,
    DisplayRefreshRate,
    AutomationApplied,
}
str_enum_serde!(OsdKind, "osd kind",
    PerformanceMode => "performanceMode",
    CoolerBoost => "coolerBoost",
    DisplayRefreshRate => "displayRefreshRate",
    AutomationApplied => "automationApplied",
);

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

    #[test]
    fn gpu_offset_parameters_and_verified_bounds_are_independent() {
        let change = |core_mhz, mem_mhz| ControlChange::SetGpuClockOffset { core_mhz, mem_mhz };
        assert_eq!(change(0, 0).capability(), CapabilityFlags::GPU_CLOCK_OFFSET);
        assert_eq!(CapabilityFlags::GPU_CLOCK_OFFSET.bits(), 1 << 12);
        assert_eq!(change(0, 0).target(), ControlTarget::GpuClockOffset {});
        for (core, mem) in [(-1000, 1000), (1000, -1000), (0, 200)] {
            assert_eq!(change(core, mem).validate_parameters(), Ok(()));
        }
        for core in [i16::MIN, -1001, 1001, i16::MAX] {
            assert_eq!(change(core, 0).validate_parameters(), Err(ControlError::InvalidField("coreMhz")));
        }
        for mem in [i16::MIN, -1001, 1001, i16::MAX] {
            assert_eq!(change(0, mem).validate_parameters(), Err(ControlError::InvalidField("memMhz")));
        }
        let mut state = HardwareState::default();
        assert_eq!(change(0, 0).validate_against(&state), Err(ControlError::UnsupportedValue("gpuClockOffset")));
        state.gpu_clock_offset = Some(GpuClockOffsetState {
            core_mhz: 0, mem_mhz: 0,
            core_min_mhz: 0, core_max_mhz: 200,
            mem_min_mhz: 0, mem_max_mhz: 200,
        });
        for (core, mem) in [(0, 0), (200, 200), (0, 200), (200, 0)] {
            assert_eq!(change(core, mem).validate_against(&state), Ok(()));
        }
        for core in [-1, 201] {
            assert_eq!(change(core, 0).validate_against(&state), Err(ControlError::UnsupportedValue("coreMhz")));
        }
        for mem in [-1, 201] {
            assert_eq!(change(0, mem).validate_against(&state), Err(ControlError::UnsupportedValue("memMhz")));
        }
        state.gpu_clock_offset.as_mut().unwrap().core_max_mhz = 100;
        assert_eq!(change(101, 0).validate_against(&state), Err(ControlError::UnsupportedValue("coreMhz")));
        assert_eq!(change(100, 200).validate_against(&state), Ok(()));
    }

    #[test]
    fn optional_gpu_state_and_telemetry_preserve_legacy_wire_shape() {
        let state = HardwareState::default();
        let legacy = serde_json::to_value(&state).unwrap();
        assert!(legacy.get("gpuClockOffset").is_none());
        assert_eq!(serde_json::from_value::<HardwareState>(legacy).unwrap(), state);
        let sample = TelemetrySample::default();
        let legacy = serde_json::to_string(&sample).unwrap();
        assert_eq!(legacy, r#"{"sampledAtUnixMs":0,"cpuTemperatureC":null,"gpuTemperatureC":null,"cpuFanRpm":null,"gpuFanRpm":null,"cpuReportedDutyPercent":null,"gpuReportedDutyPercent":null,"gpuPowerMw":null}"#);
        assert_eq!(serde_json::from_str::<TelemetrySample>(&legacy).unwrap(), sample);
        let sample = TelemetrySample {
            cpu_utilization_percent: Some(0), memory_used_mb: Some(16_384), memory_total_mb: Some(65_536),
            gpu_utilization_percent: Some(100), gpu_clock_mhz: Some(1200), gpu_mem_clock_mhz: Some(405),
            gpu_vram_used_mb: Some(1969), gpu_vram_total_mb: Some(24463), gpu_power_limit_mw: Some(175000),
            ..Default::default()
        };
        let value = serde_json::to_value(sample).unwrap();
        for (field, expected) in [
            ("cpuUtilizationPercent", 0), ("memoryUsedMb", 16_384), ("memoryTotalMb", 65_536),
            ("gpuUtilizationPercent", 100), ("gpuClockMhz", 1200), ("gpuMemClockMhz", 405),
            ("gpuVramUsedMb", 1969), ("gpuVramTotalMb", 24463), ("gpuPowerLimitMw", 175000),
        ] {
            assert_eq!(value[field], expected);
        }
        assert_eq!(serde_json::from_value::<TelemetrySample>(value).unwrap(), sample);
        let offset = GpuClockOffsetState {
            core_mhz: 100, mem_mhz: 200,
            core_min_mhz: 0, core_max_mhz: 200, mem_min_mhz: 0, mem_max_mhz: 200,
        };
        assert_eq!(serde_json::to_string(&offset).unwrap(), r#"{"coreMhz":100,"memMhz":200,"coreMinMhz":0,"coreMaxMhz":200,"memMinMhz":0,"memMaxMhz":200}"#);
    }
}
