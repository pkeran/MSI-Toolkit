use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{FanChannel, FanCurve, PerformanceMode, model::str_enum_serde};

pub const CONFIG_SCHEMA_VERSION: u16 = 1;
pub const FAN_PROFILES_SCHEMA_VERSION: u16 = 1;
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}
str_enum_serde!(Theme, "theme",
    System => "system",
    Light => "light",
    Dark => "dark",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct OsdConfig {
    pub enabled: bool,
    pub duration_ms: u16,
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self { enabled: true, duration_ms: 1_800 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct AutomationConfig {
    pub enabled: bool,
    pub manual_override: ManualOverride,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self { enabled: false, manual_override: ManualOverride::UntilReleased }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ManualOverride {
    UntilReleased,
    Timed { seconds: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { auto_check: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u16,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub osd: OsdConfig,
    #[serde(default)]
    pub automation: AutomationConfig,
    #[serde(default)]
    pub updates: UpdateConfig,
    #[serde(default)]
    pub onboarding_completed: bool,
    #[serde(default = "default_true")]
    pub restore_gpu_offset_on_boot: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            theme: Theme::default(),
            osd: OsdConfig::default(),
            automation: AutomationConfig::default(),
            updates: UpdateConfig::default(),
            onboarding_completed: false,
            restore_gpu_offset_on_boot: true,
        }
    }
}

impl AppConfig {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let config: Self = serde_json::from_slice(bytes)?;
        config.validate()?;
        Ok(config)
    }

    pub fn encode_json(&self) -> Result<Vec<u8>, ConfigError> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        if !(800..=5_000).contains(&self.osd.duration_ms) {
            return Err(ConfigError::InvalidField("osd.durationMs"));
        }
        if let ManualOverride::Timed { seconds } = self.automation.manual_override {
            if !(1..=86_400).contains(&seconds) {
                return Err(ConfigError::InvalidField("automation.manualOverride.seconds"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModeFanProfile {
    #[serde(default)]
    pub custom_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<FanCurve>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<FanCurve>,
}

impl ModeFanProfile {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(curve) = &self.cpu {
            curve
                .validate_shape()
                .map_err(|_| ConfigError::InvalidField("fanProfiles.cpu"))?;
        }
        if let Some(curve) = &self.gpu {
            curve
                .validate_shape()
                .map_err(|_| ConfigError::InvalidField("fanProfiles.gpu"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FanProfileStore {
    pub schema_version: u16,
    #[serde(default)]
    pub super_battery: ModeFanProfile,
    #[serde(default)]
    pub balanced: ModeFanProfile,
    #[serde(default)]
    pub extreme_performance: ModeFanProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mode: Option<PerformanceMode>,
    #[serde(default)]
    pub gpu_oc_core_mhz: i16,
    #[serde(default)]
    pub gpu_oc_mem_mhz: i16,
    #[serde(default)]
    pub gpu_oc_pending: bool,
}

impl Default for FanProfileStore {
    fn default() -> Self {
        Self {
            schema_version: FAN_PROFILES_SCHEMA_VERSION,
            super_battery: ModeFanProfile::default(),
            balanced: ModeFanProfile::default(),
            extreme_performance: ModeFanProfile::default(),
            last_mode: None,
            gpu_oc_core_mhz: 0,
            gpu_oc_mem_mhz: 0,
            gpu_oc_pending: false,
        }
    }
}

impl FanProfileStore {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let store: Self = serde_json::from_slice(bytes)?;
        store.validate()?;
        Ok(store)
    }

    pub fn encode_json(&self) -> Result<Vec<u8>, ConfigError> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != FAN_PROFILES_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        self.super_battery.validate()?;
        self.balanced.validate()?;
        self.extreme_performance.validate()?;
        if !(0..=200).contains(&self.gpu_oc_core_mhz) {
            return Err(ConfigError::InvalidField("gpuOcCoreMhz"));
        }
        if !(0..=200).contains(&self.gpu_oc_mem_mhz) {
            return Err(ConfigError::InvalidField("gpuOcMemMhz"));
        }
        Ok(())
    }

    pub const fn supports_custom(mode: PerformanceMode) -> bool {
        !matches!(mode, PerformanceMode::Silent)
    }

    pub const fn profile(&self, mode: PerformanceMode) -> Option<&ModeFanProfile> {
        match mode {
            PerformanceMode::SuperBattery => Some(&self.super_battery),
            PerformanceMode::Balanced => Some(&self.balanced),
            PerformanceMode::ExtremePerformance => Some(&self.extreme_performance),
            PerformanceMode::Silent => None,
        }
    }

    pub fn profile_mut(&mut self, mode: PerformanceMode) -> Option<&mut ModeFanProfile> {
        match mode {
            PerformanceMode::SuperBattery => Some(&mut self.super_battery),
            PerformanceMode::Balanced => Some(&mut self.balanced),
            PerformanceMode::ExtremePerformance => Some(&mut self.extreme_performance),
            PerformanceMode::Silent => None,
        }
    }

    pub fn active_profile(&self, mode: PerformanceMode) -> Option<ModeFanProfile> {
        self.profile(mode)
            .copied()
            .filter(|profile| profile.custom_enabled && (profile.cpu.is_some() || profile.gpu.is_some()))
    }

    pub fn custom_modes(&self) -> Vec<PerformanceMode> {
        [
            PerformanceMode::SuperBattery,
            PerformanceMode::Balanced,
            PerformanceMode::ExtremePerformance,
        ]
        .into_iter()
        .filter(|&mode| self.active_profile(mode).is_some())
        .collect()
    }

    pub fn record_custom_curve(
        &mut self,
        mode: PerformanceMode,
        channel: FanChannel,
        curve: FanCurve,
        fallback_other: Option<FanCurve>,
    ) -> bool {
        let Some(profile) = self.profile_mut(mode) else {
            return false;
        };
        let before = *profile;
        profile.custom_enabled = true;
        match channel {
            FanChannel::Cpu => {
                profile.cpu = Some(curve);
                if profile.gpu.is_none() {
                    profile.gpu = fallback_other;
                }
            }
            FanChannel::Gpu => {
                profile.gpu = Some(curve);
                if profile.cpu.is_none() {
                    profile.cpu = fallback_other;
                }
            }
        }
        *profile != before
    }

    pub fn disable_custom(&mut self, mode: PerformanceMode) -> bool {
        let Some(profile) = self.profile_mut(mode) else {
            return false;
        };
        if !profile.custom_enabled {
            return false;
        }
        profile.custom_enabled = false;
        true
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration exceeds the size limit")]
    TooLarge,
    #[error("unsupported configuration schema {0}")]
    UnsupportedSchema(u16),
    #[error("invalid configuration field: {0}")]
    InvalidField(&'static str),
    #[error("invalid configuration JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_persisted_config_round_trip() {
        let minimal = AppConfig::parse_json(br#"{"schemaVersion":1}"#).unwrap();
        assert_eq!(minimal, AppConfig::default());
        assert_eq!(minimal.osd.duration_ms, 1_800);
        assert_eq!(minimal.automation.manual_override, ManualOverride::UntilReleased);
        assert!(minimal.updates.auto_check);
        assert!(!minimal.onboarding_completed);
        let config = AppConfig {
            theme: Theme::Dark,
            updates: UpdateConfig { auto_check: false },
            onboarding_completed: true,
            ..minimal
        };
        assert_eq!(AppConfig::parse_json(&config.encode_json().unwrap()).unwrap(), config);
    }

    #[test]
    fn rejects_future_versions_ranges_and_unknown_fields() {
        assert!(matches!(
            AppConfig::parse_json(br#"{"schemaVersion":2}"#),
            Err(ConfigError::UnsupportedSchema(2)),
        ));
        assert!(matches!(
            AppConfig::parse_json(br#"{"schemaVersion":1,"osd":{"durationMs":799}}"#),
            Err(ConfigError::InvalidField("osd.durationMs")),
        ));
        assert!(matches!(
            AppConfig::parse_json(br#"{"schemaVersion":1,"automation":{"manualOverride":{"kind":"timed","seconds":86401}}}"#),
            Err(ConfigError::InvalidField("automation.manualOverride.seconds")),
        ));
        assert!(matches!(
            AppConfig::parse_json(br#"{"schemaVersion":1,"automation":{"command":"foo"}}"#),
            Err(ConfigError::Json(_)),
        ));
        assert!(matches!(
            AppConfig::parse_json(&vec![b' '; MAX_CONFIG_BYTES + 1]),
            Err(ConfigError::TooLarge),
        ));
    }

    #[test]
    fn fan_profile_store_round_trips_records_pairs_and_validates() {
        use crate::FanPoint;
        let curve = |offset: u8| FanCurve {
            points: [
                FanPoint { temperature_c: 0, duty_percent: 0 },
                FanPoint { temperature_c: 48, duty_percent: 36 + offset },
                FanPoint { temperature_c: 56, duty_percent: 42 + offset },
                FanPoint { temperature_c: 62, duty_percent: 52 + offset },
                FanPoint { temperature_c: 70, duty_percent: 76 + offset },
                FanPoint { temperature_c: 78, duty_percent: 84 + offset },
            ],
        };
        let mut store = FanProfileStore::default();
        assert!(store.custom_modes().is_empty());
        assert!(!store.record_custom_curve(PerformanceMode::Silent, FanChannel::Cpu, curve(5), Some(curve(0))));
        assert!(store.record_custom_curve(PerformanceMode::Balanced, FanChannel::Cpu, curve(10), Some(curve(2))));
        assert_eq!(store.custom_modes(), vec![PerformanceMode::Balanced]);
        let bal = store.active_profile(PerformanceMode::Balanced).unwrap();
        assert_eq!(bal.cpu, Some(curve(10)));
        assert_eq!(bal.gpu, Some(curve(2)));
        let encoded = store.encode_json().unwrap();
        assert_eq!(FanProfileStore::parse_json(&encoded).unwrap(), store);
        assert!(store.disable_custom(PerformanceMode::Balanced));
        assert!(!store.disable_custom(PerformanceMode::Balanced));
        assert!(store.active_profile(PerformanceMode::Balanced).is_none());
        assert!(store.custom_modes().is_empty());
        assert!(matches!(
            FanProfileStore::parse_json(br#"{"schemaVersion":2}"#),
            Err(ConfigError::UnsupportedSchema(2)),
        ));
        store.last_mode = Some(PerformanceMode::ExtremePerformance);
        store.gpu_oc_core_mhz = 150;
        store.gpu_oc_mem_mhz = 200;
        store.gpu_oc_pending = true;
        let encoded = store.encode_json().unwrap();
        assert_eq!(FanProfileStore::parse_json(&encoded).unwrap(), store);
        assert!(FanProfileStore::parse_json(br#"{"schemaVersion":1,"gpuOcCoreMhz":201}"#).is_err());
        assert!(FanProfileStore::parse_json(br#"{"schemaVersion":1,"gpuOcMemMhz":-5}"#).is_err());
    }
}
