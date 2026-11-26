//! Reversible changes to the fixed MSI Center allowlist.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Diagnostic, DiagnosticKind};

mod backup;
mod windows;

pub(crate) use self::backup::lock_parents;
pub use self::windows::WindowsSystemControlOps;
pub(crate) use self::windows::{
    RegKey, current_user_key, reg_open, reg_read, user_sid_string, with_scheduler,
};

const BACKUP_SCHEMA: u16 = 1;
const QUIET_PERIOD: Duration = Duration::from_millis(500);
const MAX_BACKUP_BYTES: usize = 512 * 1024;
const MAX_RUN_BYTES: usize = 16 * 1024;

pub const PROTECTED: &[&str] = &[
    "msiapcfg.dll",
    "msiltcfg.dll",
    "WmiAcpi",
    "MofImagePath",
    "Nahimic",
    "NahimicService",
    "Killer Analytics Service",
    "Killer Network Service",
    "Killer Provider Data Helper Service",
    "RtkAudioUniversalService",
    "msiserver",
    "MSiSCSI",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DisableTier {
    Core,
    Lighting,
    TrueColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MsiObject {
    #[serde(rename = "MSI_Center_Service")]
    Center,
    #[serde(rename = "MSI Foundation Service")]
    Foundation,
    #[serde(rename = "MSI Sendevsvc")]
    Sendev,
    #[serde(rename = "Micro Star SCM")]
    Scm,
    #[serde(rename = "Mystic_Light_Service")]
    MysticLight,
    #[serde(rename = "LightKeeperService")]
    LightKeeper,
    #[serde(rename = "MSITrueColorService")]
    TrueColorService,
    #[serde(rename = r"\OmApSvcBroker")]
    BrokerTask,
    #[serde(rename = r"\OneDC_Updater")]
    UpdaterTask,
    #[serde(rename = "MsiTrueColor")]
    TrueColorRun,
    #[serde(rename = "MSI_Center_UWP")]
    StoreStartup,
}

use MsiObject::*;

const CONSUMERS: &[MsiObject] = &[
    UpdaterTask,
    TrueColorService,
    LightKeeper,
    MysticLight,
    Scm,
    Sendev,
    BrokerTask,
];
const BROKERS: &[MsiObject] = &[Center, Foundation];
const STARTUP: &[MsiObject] = &[TrueColorRun, StoreStartup];
const RESTORE_ORDER: &[MsiObject] = &[
    Foundation,
    Sendev,
    Scm,
    Center,
    MysticLight,
    LightKeeper,
    TrueColorService,
    BrokerTask,
    UpdaterTask,
    TrueColorRun,
    StoreStartup,
];

impl MsiObject {
    pub(crate) const ALL: &'static [Self] = RESTORE_ORDER;

    pub const fn name(self) -> &'static str {
        match self {
            Center => "MSI_Center_Service",
            Foundation => "MSI Foundation Service",
            Sendev => "MSI Sendevsvc",
            Scm => "Micro Star SCM",
            MysticLight => "Mystic_Light_Service",
            LightKeeper => "LightKeeperService",
            TrueColorService => "MSITrueColorService",
            BrokerTask => r"\OmApSvcBroker",
            UpdaterTask => r"\OneDC_Updater",
            TrueColorRun => "MsiTrueColor",
            StoreStartup => "MSI_Center_UWP",
        }
    }

    pub const fn tier(self) -> DisableTier {
        match self {
            Center | Foundation | Sendev | Scm | BrokerTask | UpdaterTask => DisableTier::Core,
            MysticLight | LightKeeper => DisableTier::Lighting,
            TrueColorService | TrueColorRun | StoreStartup => DisableTier::TrueColor,
        }
    }

    pub fn from_name(name: &str) -> Result<Self, DisablerError> {
        if is_protected(name) {
            return Err(DisablerError::Protected);
        }
        if name.eq_ignore_ascii_case("MSI True Color") {
            return Ok(TrueColorRun);
        }
        RESTORE_ORDER
            .iter()
            .copied()
            .find(|item| item.name().eq_ignore_ascii_case(name))
            .ok_or(DisablerError::NotManaged)
    }

    const fn is_service(self) -> bool {
        matches!(
            self,
            Center | Foundation | Sendev | Scm | MysticLight | LightKeeper | TrueColorService
        )
    }

    const fn is_task(self) -> bool {
        matches!(self, BrokerTask | UpdaterTask)
    }
}

pub(crate) fn is_protected(name: &str) -> bool {
    PROTECTED.iter().any(|item| item.eq_ignore_ascii_case(name))
        || ["Nahimic", "Killer"].iter().any(|prefix| {
            name.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisablePlan(BTreeSet<DisableTier>);

impl DisablePlan {
    pub fn new(tiers: impl IntoIterator<Item = DisableTier>) -> Result<Self, DisablerError> {
        let plan = Self(tiers.into_iter().collect());
        plan.validate()?;
        Ok(plan)
    }

    pub fn contains(&self, object: MsiObject) -> bool {
        self.0.contains(&object.tier())
    }

    fn validate(&self) -> Result<(), DisablerError> {
        if self.0.is_empty() {
            Err(DisablerError::EmptyPlan)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceStart {
    Automatic,
    Manual,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceActivity {
    Stopped,
    Running,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceState {
    pub start: ServiceStart,
    pub delayed_auto_start: bool,
    pub activity: ServiceActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskState {
    pub enabled: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistryValue {
    pub value_type: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunState {
    pub name_64: Option<RegistryValue>,
    pub spaced_name_64: Option<RegistryValue>,
    pub name_32: Option<RegistryValue>,
    pub spaced_name_32: Option<RegistryValue>,
}

impl RunState {
    pub(crate) fn is_present(&self) -> bool {
        self.values().into_iter().any(Option::is_some)
    }

    fn values(&self) -> [&Option<RegistryValue>; 4] {
        [
            &self.name_64,
            &self.spaced_name_64,
            &self.name_32,
            &self.spaced_name_32,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ObjectState {
    Missing,
    Service(ServiceState),
    Task(TaskState),
    Run(RunState),
    StoreStartup(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservedObject {
    pub object: MsiObject,
    pub state: ObjectState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MsiBackupSnapshot {
    schema_version: u16,
    created_at_unix_ms: u64,
    binding: String,
    plan: DisablePlan,
    inventory: Vec<ObservedObject>,
}

impl MsiBackupSnapshot {
    pub fn inventory(&self) -> &[ObservedObject] {
        &self.inventory
    }

    pub fn plan(&self) -> &DisablePlan {
        &self.plan
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, DisablerError> {
        if bytes.len() > MAX_BACKUP_BYTES {
            return Err(DisablerError::InvalidBackup("snapshot exceeds size limit"));
        }
        let snapshot: Self = serde_json::from_slice(bytes)
            .map_err(|_| DisablerError::InvalidBackup("invalid snapshot JSON"))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, DisablerError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| DisablerError::InvalidBackup("snapshot encoding failed"))?;
        if bytes.len() > MAX_BACKUP_BYTES {
            return Err(DisablerError::InvalidBackup("snapshot exceeds size limit"));
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), DisablerError> {
        if self.schema_version != BACKUP_SCHEMA
            || self.binding.len() != 64
            || !self.binding.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(DisablerError::InvalidBackup(
                "unsupported schema or invalid machine/user binding",
            ));
        }
        self.plan.validate()?;
        let mut seen = BTreeSet::new();
        for row in &self.inventory {
            if !seen.insert(row.object) || !valid_state(row.object, &row.state) {
                return Err(DisablerError::InvalidBackup(
                    "duplicate object or invalid object state",
                ));
            }
        }
        if seen.len() != RESTORE_ORDER.len() {
            return Err(DisablerError::InvalidBackup("incomplete inventory"));
        }
        Ok(())
    }

    fn original(&self, object: MsiObject) -> &ObjectState {
        &self
            .inventory
            .iter()
            .find(|row| row.object == object)
            .expect("validated inventory")
            .state
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisablerReport {
    pub changed: Vec<MsiObject>,
    pub observed: Vec<ObservedObject>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ObjectFailure {
    pub object: MsiObject,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Error)]
pub enum DisablerError {
    #[error("protected system object")]
    Protected,
    #[error("object is not on the MSI allowlist")]
    NotManaged,
    #[error("select at least one MSI software tier")]
    EmptyPlan,
    #[error("invalid MSI software backup: {0}")]
    InvalidBackup(&'static str),
    #[error("backup belongs to another Windows installation or user")]
    MachineOrUserMismatch,
    #[error("backup path already holds an original snapshot for another plan")]
    PlanMismatch,
    #[error("no MSI software backup at the selected path")]
    NoBackup,
    #[error(transparent)]
    System(#[from] Diagnostic),
    #[error("could not disable {object:?}: {cause}; rollback failures: {rollback_failures:?}")]
    ApplyFailed {
        object: MsiObject,
        cause: Diagnostic,
        rollback_failures: Vec<ObjectFailure>,
    },
    #[error("MSI software restore incomplete: {0:?}")]
    RestoreFailed(Vec<ObjectFailure>),
}

/// Low-level operations are typed so the transaction never consumes a raw service name,
/// task definition, registry key or value name from a backup or caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemChange {
    ServiceStart(MsiObject, ServiceStart, bool),
    ServiceActivity(MsiObject, ServiceActivity),
    StopTask(MsiObject),
    TaskEnabled(MsiObject, bool),
    Run { before: RunState, after: RunState },
    StoreStartup { before: u32, after: u32 },
}

pub trait SystemControlOps {
    type UpdateLock;

    fn lock_update(&mut self) -> Result<Self::UpdateLock, Diagnostic>;
    fn backup_binding(&mut self) -> Result<String, Diagnostic>;
    fn read(&mut self, object: MsiObject) -> Result<ObjectState, Diagnostic>;
    fn preflight(&mut self, objects: &[MsiObject]) -> Result<(), Diagnostic>;
    fn execute(&mut self, change: SystemChange) -> Result<(), Diagnostic>;
    fn wait(&mut self, duration: Duration);
    fn read_backup(&mut self, path: &Path) -> Result<Option<Vec<u8>>, Diagnostic>;
    fn create_backup(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Diagnostic>;
}

pub struct SoftwareDisabler<O = WindowsSystemControlOps> {
    ops: O,
}

impl Default for SoftwareDisabler {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftwareDisabler {
    pub fn new() -> Self {
        Self::with_ops(WindowsSystemControlOps::new())
    }
}

impl<O: SystemControlOps> SoftwareDisabler<O> {
    pub fn with_ops(ops: O) -> Self {
        Self { ops }
    }

    pub fn inspect_msi_software(&mut self) -> Result<Vec<ObservedObject>, DisablerError> {
        Ok(RESTORE_ORDER
            .iter()
            .map(|&object| {
                checked_read(&mut self.ops, object).map(|state| ObservedObject { object, state })
            })
            .collect::<Result<_, _>>()?)
    }

    pub fn disable(
        &mut self,
        plan: &DisablePlan,
        path: &Path,
    ) -> Result<DisablerReport, DisablerError> {
        self.disable_inner(plan, path, Transition::Disable)
    }

    pub(crate) fn disable_autostart(
        &mut self,
        plan: &DisablePlan,
        path: &Path,
    ) -> Result<DisablerReport, DisablerError> {
        self.disable_inner(plan, path, Transition::DisableStartup)
    }

    fn disable_inner(
        &mut self,
        plan: &DisablePlan,
        path: &Path,
        transition: Transition,
    ) -> Result<DisablerReport, DisablerError> {
        plan.validate()?;
        let _lock = self.ops.lock_update()?;
        let binding = self.ops.backup_binding()?;
        let inventory = self.inspect_msi_software()?;
        let (snapshot, new) = match self.ops.read_backup(path)? {
            Some(bytes) => {
                let snapshot = MsiBackupSnapshot::from_json(&bytes)?;
                check_binding(&snapshot, &binding)?;
                if snapshot.plan != *plan {
                    return Err(DisablerError::PlanMismatch);
                }
                (snapshot, false)
            }
            None => (
                MsiBackupSnapshot {
                    schema_version: BACKUP_SCHEMA,
                    created_at_unix_ms: now_ms()?,
                    binding,
                    plan: plan.clone(),
                    inventory: inventory.clone(),
                },
                true,
            ),
        };
        let mut changes = Vec::new();
        for row in inventory.iter().filter(|row| plan.contains(row.object)) {
            let original = snapshot.original(row.object);
            let desired = disabled_state(row.object, original)?;
            check_transition(row.object, &row.state, original, &desired)?;
            if !settled(&row.state, &desired, transition) {
                changes.push(row.object);
            }
        }
        self.ops.preflight(&changes)?;
        if new {
            self.ops.create_backup(path, &snapshot.to_json()?)?;
        }

        let mut touched = Vec::new();
        let mut observed = Vec::new();
        let outcome = (|| {
            for (phase, objects) in [CONSUMERS, BROKERS, STARTUP].into_iter().enumerate() {
                if transition == Transition::Disable
                    && phase == 1
                    && BROKERS.iter().any(|object| changes.contains(object))
                {
                    self.ops.wait(QUIET_PERIOD);
                }
                for &object in objects.iter().filter(|&&object| plan.contains(object)) {
                    let operation = (|| {
                        let original = snapshot.original(object);
                        let desired = disabled_state(object, original)?;
                        let current = checked_read(&mut self.ops, object)?;
                        check_transition(object, &current, original, &desired)?;
                        if !settled(&current, &desired, transition) {
                            if !changes.contains(&object) {
                                return Err(Diagnostic::conflict(
                                    "disable preflight",
                                    object.name(),
                                ));
                            }
                            touched.push(ObservedObject {
                                object,
                                state: current.clone(),
                            });
                            converge(&mut self.ops, object, &current, &desired, transition)?;
                        }
                        verify(&mut self.ops, object, &desired, transition)?;
                        Ok(())
                    })();
                    operation.map_err(|error| (object, error))?;
                }
            }
            for &object in RESTORE_ORDER
                .iter()
                .filter(|&&object| plan.contains(object))
            {
                let desired =
                    disabled_state(object, snapshot.original(object)).map_err(|e| (object, e))?;
                let state =
                    verify(&mut self.ops, object, &desired, transition).map_err(|e| (object, e))?;
                observed.push(ObservedObject { object, state });
            }
            Ok(())
        })();
        if let Err((object, cause)) = outcome {
            let mut rollback_failures = Vec::new();
            for row in touched.iter().rev() {
                let outcome = (|| {
                    let current = checked_read(&mut self.ops, row.object)?;
                    let disabled = disabled_state(row.object, snapshot.original(row.object))?;
                    check_transition(row.object, &current, &row.state, &disabled)?;
                    let rollback = if transition == Transition::DisableStartup {
                        Transition::DisableStartup
                    } else {
                        Transition::Restore
                    };
                    converge(&mut self.ops, row.object, &current, &row.state, rollback)?;
                    verify(&mut self.ops, row.object, &row.state, rollback)?;
                    Ok(())
                })();
                if let Err(diagnostic) = outcome {
                    rollback_failures.push(ObjectFailure {
                        object: row.object,
                        diagnostic,
                    });
                }
            }
            return Err(DisablerError::ApplyFailed {
                object,
                cause,
                rollback_failures,
            });
        }
        Ok(DisablerReport {
            changed: touched.into_iter().map(|row| row.object).collect(),
            observed,
        })
    }

    pub(crate) fn stop_services(&mut self, plan: &DisablePlan) -> Result<(), DisablerError> {
        let _lock = self.ops.lock_update()?;
        let mut running = Vec::new();
        for &object in CONSUMERS.iter().chain(BROKERS) {
            if object.is_service()
                && plan.contains(object)
                && matches!(
                    checked_read(&mut self.ops, object)?,
                    ObjectState::Service(state) if state.activity != ServiceActivity::Stopped
                )
            {
                running.push(object);
            }
        }
        for (phase, objects) in [CONSUMERS, BROKERS].into_iter().enumerate() {
            if phase == 1 && BROKERS.iter().any(|object| running.contains(object)) {
                self.ops.wait(QUIET_PERIOD);
            }
            for &object in objects.iter().filter(|object| running.contains(object)) {
                self.ops.execute(SystemChange::ServiceActivity(
                    object,
                    ServiceActivity::Stopped,
                ))?;
                if !matches!(checked_read(&mut self.ops, object)?, ObjectState::Service(state) if state.activity == ServiceActivity::Stopped)
                {
                    return Err(Diagnostic::new(
                        "MSI service stop readback",
                        DiagnosticKind::Rejected,
                    )
                    .detail(object.name())
                    .into());
                }
            }
        }
        Ok(())
    }

    pub fn restore_from_backup(&mut self, path: &Path) -> Result<DisablerReport, DisablerError> {
        let _lock = self.ops.lock_update()?;
        let bytes = self.ops.read_backup(path)?.ok_or(DisablerError::NoBackup)?;
        let snapshot = MsiBackupSnapshot::from_json(&bytes)?;
        check_binding(&snapshot, &self.ops.backup_binding()?)?;
        let mut changed = Vec::new();
        let mut observed = Vec::new();
        let mut failures = Vec::new();
        for &object in RESTORE_ORDER
            .iter()
            .filter(|&&object| snapshot.plan.contains(object))
        {
            let result = (|| {
                let original = snapshot.original(object);
                let desired = disabled_state(object, original)?;
                let current = checked_read(&mut self.ops, object)?;
                check_transition(object, &current, original, &desired)?;
                if !settled(&current, original, Transition::Restore) {
                    self.ops.preflight(&[object])?;
                    converge(
                        &mut self.ops,
                        object,
                        &current,
                        original,
                        Transition::Restore,
                    )?;
                    changed.push(object);
                }
                let state = verify(&mut self.ops, object, original, Transition::Restore)?;
                Ok(ObservedObject { object, state })
            })();
            match result {
                Ok(row) => observed.push(row),
                Err(diagnostic) => failures.push(ObjectFailure { object, diagnostic }),
            }
        }
        if failures.is_empty() {
            Ok(DisablerReport { changed, observed })
        } else {
            Err(DisablerError::RestoreFailed(failures))
        }
    }
}

fn now_ms() -> Result<u64, Diagnostic> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| Diagnostic::invalid("MSI software backup clock", "invalid Unix time"))
}

fn check_binding(snapshot: &MsiBackupSnapshot, binding: &str) -> Result<(), DisablerError> {
    if snapshot.binding == binding {
        Ok(())
    } else {
        Err(DisablerError::MachineOrUserMismatch)
    }
}

fn valid_state(object: MsiObject, state: &ObjectState) -> bool {
    match state {
        ObjectState::Missing => object != TrueColorRun,
        ObjectState::Service(_) => object.is_service(),
        ObjectState::Task(_) => object.is_task(),
        ObjectState::Run(run) => {
            object == TrueColorRun
                && run
                    .values()
                    .into_iter()
                    .flatten()
                    .all(|value| value.value_type <= 11 && value.bytes.len() <= MAX_RUN_BYTES)
        }
        ObjectState::StoreStartup(state) => object == StoreStartup && *state <= 4,
    }
}

fn checked_read(
    ops: &mut impl SystemControlOps,
    object: MsiObject,
) -> Result<ObjectState, Diagnostic> {
    let state = ops.read(object)?;
    if !valid_state(object, &state) {
        return Err(Diagnostic::invalid("MSI software inventory", object.name()));
    }
    Ok(state)
}

fn disabled_state(object: MsiObject, original: &ObjectState) -> Result<ObjectState, Diagnostic> {
    Ok(match original {
        ObjectState::Missing => ObjectState::Missing,
        ObjectState::Service(original) => ObjectState::Service(ServiceState {
            start: ServiceStart::Disabled,
            activity: ServiceActivity::Stopped,
            ..*original
        }),
        ObjectState::Task(_) => ObjectState::Task(TaskState {
            enabled: false,
            active: false,
        }),
        ObjectState::Run(_) => ObjectState::Run(RunState::default()),
        ObjectState::StoreStartup(2) => ObjectState::StoreStartup(0),
        ObjectState::StoreStartup(4) => {
            return Err(Diagnostic::conflict("Store startup policy", object.name()));
        }
        ObjectState::StoreStartup(value) => ObjectState::StoreStartup(*value),
    })
}

fn check_transition(
    object: MsiObject,
    current: &ObjectState,
    original: &ObjectState,
    disabled: &ObjectState,
) -> Result<(), Diagnostic> {
    let compatible = match (current, original, disabled) {
        (ObjectState::Service(c), ObjectState::Service(o), ObjectState::Service(d)) => {
            (c.start == o.start || c.start == d.start)
                && c.delayed_auto_start == o.delayed_auto_start
        }
        (ObjectState::Task(c), ObjectState::Task(o), ObjectState::Task(d)) => {
            c.enabled == o.enabled || c.enabled == d.enabled
        }
        (ObjectState::Run(c), ObjectState::Run(o), ObjectState::Run(d)) => c
            .values()
            .into_iter()
            .zip(o.values())
            .zip(d.values())
            .all(|((c, o), d)| c == o || c == d),
        _ => current == original || current == disabled,
    };
    if compatible {
        Ok(())
    } else {
        Err(Diagnostic::conflict(
            "MSI software state diverged from backup",
            object.name(),
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transition {
    Disable,
    DisableStartup,
    Restore,
}

fn settled(observed: &ObjectState, expected: &ObjectState, transition: Transition) -> bool {
    if let (Transition::DisableStartup, ObjectState::Service(a), ObjectState::Service(b)) =
        (transition, observed, expected)
    {
        return a.start == b.start && a.delayed_auto_start == b.delayed_auto_start;
    }
    if transition != Transition::Disable {
        if let (ObjectState::Task(a), ObjectState::Task(b)) = (observed, expected) {
            // A stopped task instance cannot be resumed; restoring its trigger is reversible.
            return a.enabled == b.enabled;
        }
    }
    observed == expected
}

fn verify(
    ops: &mut impl SystemControlOps,
    object: MsiObject,
    desired: &ObjectState,
    transition: Transition,
) -> Result<ObjectState, Diagnostic> {
    let observed = checked_read(ops, object)?;
    if settled(&observed, desired, transition) {
        Ok(observed)
    } else {
        Err(
            Diagnostic::new("MSI software readback", DiagnosticKind::Rejected)
                .detail(object.name()),
        )
    }
}

fn converge(
    ops: &mut impl SystemControlOps,
    object: MsiObject,
    current: &ObjectState,
    desired: &ObjectState,
    transition: Transition,
) -> Result<(), Diagnostic> {
    if settled(current, desired, transition) {
        return Ok(());
    }
    match (current, desired) {
        (ObjectState::Service(current), ObjectState::Service(desired)) => {
            let config = |start, delayed| SystemChange::ServiceStart(object, start, delayed);
            let activity = |activity| SystemChange::ServiceActivity(object, activity);
            if transition == Transition::DisableStartup {
                return ops.execute(config(desired.start, desired.delayed_auto_start));
            }
            if desired.activity == ServiceActivity::Stopped
                && current.activity != ServiceActivity::Stopped
            {
                ops.execute(activity(ServiceActivity::Stopped))?;
            }
            let start_before_disabling = desired.start == ServiceStart::Disabled
                && desired.activity != ServiceActivity::Stopped;
            let mut start = current.start;
            if start_before_disabling
                && start == ServiceStart::Disabled
                && current.activity == ServiceActivity::Stopped
            {
                ops.execute(config(ServiceStart::Manual, desired.delayed_auto_start))?;
                start = ServiceStart::Manual;
            }
            if !start_before_disabling
                && (start != desired.start
                    || current.delayed_auto_start != desired.delayed_auto_start)
            {
                ops.execute(config(desired.start, desired.delayed_auto_start))?;
            }
            if desired.activity != ServiceActivity::Stopped && current.activity != desired.activity
            {
                ops.execute(activity(desired.activity))?;
            }
            if start_before_disabling
                && (start != desired.start
                    || current.delayed_auto_start != desired.delayed_auto_start)
            {
                ops.execute(config(desired.start, desired.delayed_auto_start))?;
            }
        }
        (ObjectState::Task(current), ObjectState::Task(desired)) => {
            if transition == Transition::Disable && current.active {
                ops.execute(SystemChange::StopTask(object))?;
            }
            if current.enabled != desired.enabled {
                ops.execute(SystemChange::TaskEnabled(object, desired.enabled))?;
            }
        }
        (ObjectState::Run(current), ObjectState::Run(desired)) => {
            ops.execute(SystemChange::Run {
                before: current.clone(),
                after: desired.clone(),
            })?
        }
        (ObjectState::StoreStartup(current), ObjectState::StoreStartup(desired)) => {
            ops.execute(SystemChange::StoreStartup {
                before: *current,
                after: *desired,
            })?
        }
        _ => {
            return Err(Diagnostic::conflict(
                "MSI software object changed type or disappeared",
                object.name(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
