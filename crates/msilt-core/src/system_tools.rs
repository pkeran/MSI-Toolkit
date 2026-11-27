use serde::{Deserialize, Serialize};

use crate::model::str_enum_serde;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    Disabled,
    ScheduledTask,
    UserRunKey,
}
str_enum_serde!(StartupMode, "startup mode",
    Disabled => "disabled",
    ScheduledTask => "scheduledTask",
    UserRunKey => "userRunKey",
);

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartupStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<StartupMode>,
    pub elevated: bool,
    pub installed_in_program_files: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OemScope {
    All,
    Core,
    Lighting,
    TrueColor,
}
str_enum_serde!(OemScope, "oem scope",
    All => "all",
    Core => "core",
    Lighting => "lighting",
    TrueColor => "trueColor",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OemItemKind {
    Service,
    ScheduledTask,
    RegistryRun,
    StoreStartup,
}
str_enum_serde!(OemItemKind, "oem item kind",
    Service => "service",
    ScheduledTask => "scheduledTask",
    RegistryRun => "registryRun",
    StoreStartup => "storeStartup",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OemPackageId {
    MsiCenterUwp,
    MsiCenterSdk,
    MsiTrueColor,
}
str_enum_serde!(OemPackageId, "oem package id",
    MsiCenterUwp => "msiCenterUwp",
    MsiCenterSdk => "msiCenterSdk",
    MsiTrueColor => "msiTrueColor",
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OemItemStatus {
    pub id: String,
    pub display_name: String,
    pub kind: OemItemKind,
    pub tier: OemScope,
    pub present: bool,
    pub running: bool,
    pub auto_start: bool,
    pub startup_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OemProcessStatus {
    pub name: String,
    pub pid: u32,
    pub memory_kib: u32,
    pub tier: OemScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OemPackageStatus {
    pub id: OemPackageId,
    pub display_name: String,
    pub version: String,
    pub installed: bool,
    pub can_uninstall: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OemInventory {
    pub elevated: bool,
    pub has_backup: bool,
    pub protected_intact: bool,
    pub items: Vec<OemItemStatus>,
    pub processes: Vec<OemProcessStatus>,
    pub packages: Vec<OemPackageStatus>,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum OemAction {
    DisableAutoStart { scope: OemScope },
    StopNow { scope: OemScope },
    DisableAndStop { scope: OemScope },
    RestoreBackup {},
    UninstallPackage { package: OemPackageId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationPreset {
    BatterySilent60Hz,
    AcBalanced240Hz,
    LowBatterySuperSave,
    LockScreenDarkKeyboard,
}
str_enum_serde!(AutomationPreset, "automation preset",
    BatterySilent60Hz => "batterySilent60Hz",
    AcBalanced240Hz => "acBalanced240Hz",
    LowBatterySuperSave => "lowBatterySuperSave",
    LockScreenDarkKeyboard => "lockScreenDarkKeyboard",
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRuleView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub priority: u16,
    pub trigger: String,
    pub actions: String,
    pub diagnostic: String,
    pub active: bool,
}
