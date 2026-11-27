use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::OwnedHandle,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use msilt_automation::{AutomationRuleSet, RuleEngine};
use msilt_core::{AppConfig, AutomationPreset, FanProfileStore, MAX_CONFIG_BYTES};
use windows::{
    Win32::{
        Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING,
            MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        UI::Shell::FOLDERID_LocalAppData,
    },
    core::PCWSTR,
};

use crate::{
    automation_rules,
    security::{Impersonation, known_folder},
    win::{denied, os_error, raw, wide},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ConfigStore {
    directory: PathBuf,
    token: Option<Arc<OwnedHandle>>,
    persist_diagnostics: bool,
    preserve_corrupt: bool,
    preserve_corrupt_fans: bool,
}

impl ConfigStore {
    pub(crate) fn interactive(
        token: Arc<OwnedHandle>,
        persist_diagnostics: bool,
    ) -> io::Result<Self> {
        let _user = Impersonation::user(&token)?;
        let directory =
            known_folder(&FOLDERID_LocalAppData, Some(raw(&token)))?.join("MSI-Toolkit");
        Ok(Self {
            directory,
            token: Some(token),
            persist_diagnostics,
            preserve_corrupt: false,
            preserve_corrupt_fans: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn isolated(directory: PathBuf) -> Self {
        Self {
            directory,
            token: None,
            persist_diagnostics: true,
            preserve_corrupt: false,
            preserve_corrupt_fans: false,
        }
    }

    pub(crate) fn load(&mut self) -> AppConfig {
        match self.read() {
            Ok(Some(config)) => config,
            Ok(None) => AppConfig::default(),
            Err(error) => {
                self.preserve_corrupt = error.kind() == io::ErrorKind::InvalidData;
                eprintln!("msilt-daemon: app.json could not be loaded: {error}");
                AppConfig::default()
            }
        }
    }

    pub(crate) fn load_fan_profiles(&mut self) -> FanProfileStore {
        match self.read_fan_profiles() {
            Ok(Some(profiles)) => profiles,
            Ok(None) => FanProfileStore::default(),
            Err(error) => {
                self.preserve_corrupt_fans = error.kind() == io::ErrorKind::InvalidData;
                eprintln!("msilt-daemon: fan-profiles.json could not be loaded: {error}");
                FanProfileStore::default()
            }
        }
    }

    pub(crate) fn load_automation(&self) -> RuleEngine {
        let mut engine = RuleEngine::new();
        match self.read_automation() {
            Ok(Some(set)) => {
                let report = engine.load(set);
                self.diagnostic(&format!(
                    "automation.toml: loaded {} rules ({} enabled)",
                    report.rules, report.enabled
                ));
            }
            Ok(None) => self.diagnostic("automation.toml: missing; using safe empty rules"),
            Err(error) => self.diagnostic(&format!(
                "automation.toml: rejected ({:?}); using safe empty rules",
                error.kind()
            )),
        }
        engine
    }

    fn read_automation(&self) -> io::Result<Option<AutomationRuleSet>> {
        let _user = self.impersonate()?;
        match checked_directory(&self.directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(self.directory.join("automation.toml"))
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(denied("automation.toml must be a regular file"));
        }
        let limit = msilt_automation::rule::MAX_CONFIG_BYTES;
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
        parse_automation(&bytes).map(Some)
    }

    pub(crate) fn list_automation(&self) -> io::Result<AutomationRuleSet> {
        Ok(self.read_automation()?.unwrap_or_default())
    }

    pub(crate) fn set_rule_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> io::Result<AutomationRuleSet> {
        let mut set = self.list_automation()?;
        let rule = set
            .rules
            .iter_mut()
            .find(|rule| rule.id == id)
            .ok_or_else(unknown_rule)?;
        if rule.enabled == enabled {
            return Ok(set);
        }
        rule.enabled = enabled;
        self.save_automation(&automation_rules::encode(&set, None)?)
    }

    pub(crate) fn delete_rule(&self, id: &str) -> io::Result<AutomationRuleSet> {
        let mut set = self.list_automation()?;
        let index = set
            .rules
            .iter()
            .position(|rule| rule.id == id)
            .ok_or_else(unknown_rule)?;
        set.rules.remove(index);
        self.save_automation(&automation_rules::encode(&set, None)?)
    }

    pub(crate) fn add_preset(&self, preset: AutomationPreset) -> io::Result<AutomationRuleSet> {
        let set = self.list_automation()?;
        let (id, preset) = automation_rules::preset(preset);
        if set
            .rules
            .iter()
            .any(|rule| rule.id.eq_ignore_ascii_case(id))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "automation preset already exists",
            ));
        }
        self.save_automation(&automation_rules::encode(&set, Some(&preset))?)
    }

    fn save_automation(&self, text: &str) -> io::Result<AutomationRuleSet> {
        let set = parse_automation(text.as_bytes())?;
        let _user = self.impersonate()?;
        self.write_atomic("automation.toml", text.as_bytes(), false)?;
        Ok(set)
    }

    pub(crate) fn oem_backup(&self, create_parent: bool) -> io::Result<PathBuf> {
        if create_parent {
            let _user = self.impersonate()?;
            self.ensure_directory()?;
        }
        Ok(self.directory.join("msi-oem-backup.json"))
    }

    pub(crate) fn diagnostic(&self, message: &str) {
        eprintln!("msilt-daemon: {message}");
        if !self.persist_diagnostics {
            return;
        }
        let record = || -> io::Result<()> {
            let _user = self.impersonate()?;
            match fs::create_dir(&self.directory) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
            checked_directory(&self.directory)?;
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(self.directory.join("daemon-diagnostics.log"))?;
            let meta = file.metadata()?;
            if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(denied("diagnostic log must be a regular file"));
            }
            if meta.len() < 256 * 1024 {
                writeln!(file, "{} {message}", crate::hal_executor::unix_ms())?;
            }
            Ok(())
        };
        if let Err(error) = record() {
            eprintln!("msilt-daemon: local diagnostic log: {error}");
        }
    }

    fn read(&self) -> io::Result<Option<AppConfig>> {
        let _user = self.impersonate()?;
        match checked_directory(&self.directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        }
        let path = self.directory.join("app.json");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(denied("app.json must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        AppConfig::parse_json(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn save(&mut self, config: &AppConfig) -> io::Result<()> {
        let bytes = config
            .encode_json()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "configuration exceeds 16 KiB",
            ));
        }
        let _user = self.impersonate()?;
        self.write_atomic("app.json", &bytes, self.preserve_corrupt)?;
        self.preserve_corrupt = false;
        Ok(())
    }

    fn read_fan_profiles(&self) -> io::Result<Option<FanProfileStore>> {
        let _user = self.impersonate()?;
        match checked_directory(&self.directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        }
        let path = self.directory.join("fan-profiles.json");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(denied("fan-profiles.json must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        FanProfileStore::parse_json(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn save_fan_profiles(&mut self, profiles: &FanProfileStore) -> io::Result<()> {
        let bytes = profiles
            .encode_json()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fan profile configuration exceeds 16 KiB",
            ));
        }
        let _user = self.impersonate()?;
        self.write_atomic("fan-profiles.json", &bytes, self.preserve_corrupt_fans)?;
        self.preserve_corrupt_fans = false;
        Ok(())
    }

    fn ensure_directory(&self) -> io::Result<()> {
        match fs::create_dir(&self.directory) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        checked_directory(&self.directory)
    }

    fn write_atomic(&self, name: &str, bytes: &[u8], preserve_corrupt: bool) -> io::Result<()> {
        self.ensure_directory()?;
        let destination = self.directory.join(name);
        if name == "automation.toml" {
            match fs::symlink_metadata(&destination) {
                Ok(metadata)
                    if !metadata.is_file()
                        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 =>
                {
                    return Err(denied("automation.toml must be a regular file"));
                }
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        let temporary = self.unique(name, "tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut cleanup = Temporary(Some(temporary));
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if preserve_corrupt {
            // A hard link preserves even an oversized original without reading it again.
            let prefix = name.strip_suffix(".json").unwrap_or(name);
            match fs::hard_link(&destination, self.unique(prefix, "corrupt")) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        let source = wide(cleanup.0.as_deref().expect("temporary file exists"));
        let destination = wide(destination);
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(os_error)?;
        cleanup.0 = None;
        Ok(())
    }

    fn unique(&self, name: &str, suffix: &str) -> PathBuf {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        self.directory
            .join(format!("{name}-{}-{serial}.{suffix}", std::process::id()))
    }

    fn impersonate(&self) -> io::Result<Option<Impersonation>> {
        self.token.as_deref().map(Impersonation::user).transpose()
    }
}

fn parse_automation(bytes: &[u8]) -> io::Result<AutomationRuleSet> {
    AutomationRuleSet::parse(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid automation rules"))
}

fn unknown_rule() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "automation rule not found")
}

fn checked_directory(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(denied(
            "configuration directory must not be a reparse point",
        ));
    }
    Ok(())
}

struct Temporary(Option<PathBuf>);

impl Drop for Temporary {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use msilt_core::AutomationEngine;

    struct Fixture(ConfigStore);

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "msilt-automation-store-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&directory).unwrap();
            Self(ConfigStore::isolated(directory))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for entry in fs::read_dir(&self.0.directory).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_dir(&self.0.directory);
        }
    }

    #[test]
    fn fan_profiles_persist_load_and_preserve_corrupt_files() {
        use msilt_core::{FanChannel, FanCurve, FanPoint, PerformanceMode};
        let mut fixture = Fixture::new();
        assert_eq!(fixture.0.load_fan_profiles(), FanProfileStore::default());

        let curve = FanCurve {
            points: [
                FanPoint { temperature_c: 0, duty_percent: 0 },
                FanPoint { temperature_c: 48, duty_percent: 45 },
                FanPoint { temperature_c: 56, duty_percent: 58 },
                FanPoint { temperature_c: 62, duty_percent: 72 },
                FanPoint { temperature_c: 70, duty_percent: 90 },
                FanPoint { temperature_c: 78, duty_percent: 105 },
            ],
        };
        let mut profiles = FanProfileStore::default();
        assert!(profiles.record_custom_curve(
            PerformanceMode::ExtremePerformance,
            FanChannel::Cpu,
            curve,
            None,
        ));
        fixture.0.save_fan_profiles(&profiles).unwrap();
        assert_eq!(fixture.0.load_fan_profiles(), profiles);

        let path = fixture.0.directory.join("fan-profiles.json");
        fs::write(&path, b"{not valid json").unwrap();
        assert_eq!(fixture.0.load_fan_profiles(), FanProfileStore::default());
        fixture.0.save_fan_profiles(&profiles).unwrap();
        let corrupt_backups = fs::read_dir(&fixture.0.directory)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".corrupt")
            })
            .count();
        assert_eq!(corrupt_backups, 1);
    }

    #[test]
    fn missing_invalid_and_oversized_rules_fall_back_and_record_a_local_diagnostic() {
        let fixture = Fixture::new();
        let file = fixture.0.directory.join("automation.toml");
        for data in [
            None,
            Some(b"schema = 1\nunknown_key = true".to_vec()),
            Some(vec![b'x'; msilt_automation::rule::MAX_CONFIG_BYTES + 1]),
            Some(vec![0xFF]),
        ] {
            if let Some(data) = data {
                fs::write(&file, data).unwrap();
            }
            let engine = fixture.0.load_automation();
            assert!(engine.rules().is_empty());
            assert_eq!(engine.next_deadline_monotonic_ms(), None);
        }
        let log = fs::read_to_string(fixture.0.directory.join("daemon-diagnostics.log")).unwrap();
        assert!(log.contains("missing; using safe empty rules"));
        assert_eq!(log.matches("rejected (InvalidData)").count(), 3);
    }

    #[test]
    fn readonly_endpoint_loads_rules_without_writing_a_diagnostic_file() {
        let mut fixture = Fixture::new();
        fixture.0.persist_diagnostics = false;
        assert!(fixture.0.load_automation().rules().is_empty());
        assert!(fs::read_dir(&fixture.0.directory).unwrap().next().is_none());
    }

    #[test]
    fn automation_uses_real_rule_parser_and_engine() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.directory.join("automation.toml"),
            br#"
schema = 1
[[rules]]
id = "readonly-test"
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "displayBrightness"
value = 50
"#,
        )
        .unwrap();
        let mut engine = fixture.0.load_automation();
        assert_eq!(engine.rules().len(), 1);
        let mut actions = Vec::new();
        let snapshot = msilt_core::SystemSnapshot::default();
        engine.on_event(
            &msilt_core::AutomationEvent::RulesReloaded,
            msilt_core::AutomationContext {
                snapshot: &snapshot,
                monotonic_ms: 0,
                unix_ms: 0,
            },
            &mut actions,
        );
        assert_eq!(engine.stats().events, 1);
        assert!(actions.is_empty());
    }

    #[test]
    fn automation_directory_is_not_accepted_as_a_file() {
        let fixture = Fixture::new();
        let path = fixture.0.directory.join("automation.toml");
        fs::create_dir(&path).unwrap();
        assert!(fixture.0.load_automation().rules().is_empty());
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn all_presets_persist_toggle_delete_and_reject_duplicates() {
        let fixture = Fixture::new();
        for preset in [
            AutomationPreset::BatterySilent60Hz,
            AutomationPreset::AcBalanced240Hz,
            AutomationPreset::LowBatterySuperSave,
            AutomationPreset::LockScreenDarkKeyboard,
        ] {
            fixture.0.add_preset(preset).unwrap();
        }
        let path = fixture.0.directory.join("automation.toml");
        let before = fs::read(&path).unwrap();
        assert_eq!(
            fixture
                .0
                .add_preset(AutomationPreset::AcBalanced240Hz)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let changed = fixture
            .0
            .set_rule_enabled("low-battery-super-save", false)
            .unwrap();
        assert_eq!(changed.len(), 4);
        assert!(
            !fixture
                .0
                .list_automation()
                .unwrap()
                .rule_by_id("low-battery-super-save")
                .unwrap()
                .enabled
        );
        fixture.0.delete_rule("battery-silent-60hz").unwrap();
        assert_eq!(fixture.0.load_automation().rules().len(), 3);
        assert!(
            fixture
                .0
                .list_automation()
                .unwrap()
                .rule_by_id("battery-silent-60hz")
                .is_none()
        );
        assert_eq!(fs::read_dir(&fixture.0.directory).unwrap().count(), 2);
    }

    #[test]
    fn failed_validation_leaves_the_original_automation_file_intact() {
        let fixture = Fixture::new();
        let path = fixture.0.directory.join("automation.toml");
        let original = "schema = 1\nrules = []\n[limits]\nmax_rules = 1\n";
        fs::write(&path, original).unwrap();
        fixture
            .0
            .add_preset(AutomationPreset::BatterySilent60Hz)
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(
            fixture
                .0
                .add_preset(AutomationPreset::AcBalanced240Hz)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::write(&path, "schema = 2").unwrap();
        assert!(
            fixture
                .0
                .add_preset(AutomationPreset::BatterySilent60Hz)
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "schema = 2");
    }
}
