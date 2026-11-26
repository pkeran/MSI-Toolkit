use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u16 = 1;
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

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
}
