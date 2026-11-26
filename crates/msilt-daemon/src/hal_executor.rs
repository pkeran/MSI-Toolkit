use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use msilt_core::*;
use msilt_hal::{Diagnostic, DiagnosticKind, oem_manager, startup};
use windows::Win32::{Devices::Display::*, Foundation::ERROR_INSUFFICIENT_BUFFER};

use crate::{
    automation_rules::dark_keyboard,
    events::Inbox,
    iocp::{Port, WAKE_HAL},
    lease::{TARGETS, observed},
};

pub trait HalBackend: HardwareRead + EcRegisterIo + PlatformControl {}
impl<T: HardwareRead + EcRegisterIo + PlatformControl> HalBackend for T {}

/// Supplies a reviewed mapping only after verifying complete device identity and writer exclusivity.
pub trait EcWritePolicy {
    fn authorize(&mut self, report: &HardwareReport) -> Result<&EcMap, EcWriteStatus>;
    fn writable_targets(&self) -> CapabilityFlags;
    fn register_intent(&self, change: &ControlChange) -> Option<EcWrite>;
    fn register_plan(&self, change: &ControlChange) -> Vec<(EcWrite, u8)> {
        self.register_intent(change)
            .into_iter()
            .map(|write| (write, u8::MAX))
            .collect()
    }
}

pub struct HalBackends {
    device: Box<dyn HalBackend>,
    platform_writable: CapabilityFlags,
    ec: Option<Box<dyn EcWritePolicy>>,
    recovery: bool,
    readonly: bool,
    governor: EcGovernor,
}

impl HalBackends {
    /// Constructs a backend on the HAL thread, so COM and device objects need not implement Send.
    pub fn new(device: impl HalBackend + 'static, platform_writable: CapabilityFlags) -> Self {
        Self {
            device: Box::new(device),
            platform_writable: mask(platform_writable, PLATFORM_BITS),
            ec: None,
            recovery: false,
            readonly: false,
            governor: EcGovernor::default(),
        }
    }

    pub fn with_ec_policy(mut self, policy: impl EcWritePolicy + 'static) -> Self {
        self.ec = Some(Box::new(policy));
        self
    }

    pub fn noop() -> Self {
        Self::new(Noop, CapabilityFlags::NONE)
    }

    pub(crate) fn read_only(mut self) -> Self {
        self.readonly = true;
        self
    }

    fn inspect(&mut self) -> HalResult<Observation> {
        let mut report = self.device.inspect()?;
        normalize(&mut report.state);
        let controls = report.controls_detected.bits() & report.readable.bits();
        let mut allowed = self.platform_writable.bits();
        let mut ec_status = EcWriteStatus::Unavailable;
        if self.recovery {
            ec_status = EcWriteStatus::RecoveryRequired;
        } else if let Some(policy) = &mut self.ec {
            let ec_allowed = policy.writable_targets().bits() & EC_BITS;
            match policy.authorize(&report) {
                Ok(map) if map.matches(&report.state.identity) => {
                    ec_status = EcWriteStatus::Enabled;
                    allowed |= ec_allowed;
                }
                Ok(_) => ec_status = EcWriteStatus::ReadOnlyFirmware,
                Err(EcWriteStatus::Enabled) => ec_status = EcWriteStatus::UnverifiedMapping,
                Err(status) => ec_status = status,
            }
        } else if controls & EC_BITS != 0 {
            ec_status = EcWriteStatus::UnverifiedMapping;
        }
        if self.readonly {
            allowed = 0;
            if ec_status == EcWriteStatus::Enabled {
                ec_status = EcWriteStatus::ReadOnlyFirmware;
            }
        }
        let mut writable = 0;
        for target in TARGETS {
            let capability = target.capability().bits();
            if (controls & allowed & capability) != 0
                && observed(&report.state, target)
                    .is_some_and(|value| value.validate_against(&report.state).is_ok())
            {
                writable |= capability;
            }
        }
        Ok(Observation {
            report,
            writable: CapabilityFlags::from_bits_retain(writable),
            ec_status,
        })
    }

    fn apply(
        &mut self,
        change: &ControlChange,
        expected: &HardwareIdentity,
        mut permit: impl FnMut() -> bool,
    ) -> Applied {
        let before = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => return Applied::failure(error.into(), None),
        };
        if &before.report.state.identity != expected {
            return Applied::failure(ErrorCode::ConflictDetected, Some(before));
        }
        if let Err(error) = validate(
            &before.report.state,
            before.writable,
            before.ec_status,
            change,
        ) {
            return Applied::failure(error, Some(before));
        }
        let original = observed(&before.report.state, change.target());
        if original.as_ref() == Some(change) {
            if !permit() {
                return Applied::failure(ErrorCode::NotReady, Some(before));
            }
            return Applied {
                result: Ok(()),
                before: original,
                observation: Some(before),
            };
        }
        let ec = is_ec(change.target());
        let result = if ec {
            self.apply_ec(&before.report, change, permit)
        } else if permit() {
            self.apply_platform(change)
        } else {
            Err(ErrorCode::NotReady)
        };
        if result == Err(ErrorCode::TimedOut) {
            self.recovery |= ec;
            return Applied::failure(ErrorCode::TimedOut, None);
        }
        if matches!(
            result,
            Err(ErrorCode::Busy
                | ErrorCode::NotReady
                | ErrorCode::Unsupported
                | ErrorCode::ReadOnlyFirmware
                | ErrorCode::WriteNotAllowed
                | ErrorCode::DeniedWrite
                | ErrorCode::ConflictDetected)
        ) {
            return Applied::failure(result.expect_err("filtered error"), Some(before));
        }
        // A failed but completed write can have side effects; publish the new observation as well.
        let mut after = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => {
                self.recovery |= ec;
                return Applied::failure(
                    if error == HalError::TimedOut {
                        ErrorCode::TimedOut
                    } else {
                        result.err().unwrap_or(ErrorCode::BackendUnavailable)
                    },
                    None,
                );
            }
        };
        let verified = &after.report.state.identity == expected
            && observed(&after.report.state, change.target()).as_ref() == Some(change);
        let result = result.and(if verified {
            Ok(())
        } else {
            Err(ErrorCode::VerificationFailed)
        });
        if ec && (!verified && result == Err(ErrorCode::VerificationFailed) || self.recovery) {
            self.recovery = true;
            after.ec_status = EcWriteStatus::RecoveryRequired;
            after.writable = mask(after.writable, !EC_BITS);
        }
        Applied {
            result,
            before: original,
            observation: Some(after),
        }
    }

    fn apply_platform(&mut self, change: &ControlChange) -> Result<(), ErrorCode> {
        match change {
            ControlChange::SetDisplayRefreshRate { hz } => {
                if self
                    .device
                    .set_display_refresh_rate(*hz)
                    .map_err(ErrorCode::from)?
                    != *hz
                {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
            ControlChange::SetDisplayBrightness { percent } => {
                if self
                    .device
                    .set_display_brightness(*percent)
                    .map_err(ErrorCode::from)?
                    != *percent
                {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
            ControlChange::SetColorProfile { profile_id } => {
                self.device.set_color_profile(profile_id)?
            }
            ControlChange::SetKeyboardLighting { zones } => {
                self.device.set_keyboard_lighting(zones)?
            }
            _ => return Err(ErrorCode::Unsupported),
        }
        Ok(())
    }

    fn apply_ec(
        &mut self,
        report: &HardwareReport,
        change: &ControlChange,
        mut permit: impl FnMut() -> bool,
    ) -> Result<(), ErrorCode> {
        let policy = self.ec.as_mut().ok_or(ErrorCode::Unsupported)?;
        let registers = policy.register_plan(change);
        let map = policy.authorize(report).map_err(ec_error)?;
        let mut guard = WriteGuard::new(map, &report.state.identity);
        self.governor.admit(change.target(), Instant::now())?;
        let result = match change {
            ControlChange::SetFanCurve { channel, curve } => {
                match guard.prepare(
                    self.device.as_mut(),
                    EcWriteIntent::FanCurve {
                        channel: *channel,
                        curve,
                    },
                ) {
                    Ok(transaction) if permit() => transaction
                        .commit()
                        .map(|_| ())
                        .map_err(|error| ErrorCode::from(&error)),
                    Ok(_) => Err(ErrorCode::NotReady),
                    Err(error) => Err(ErrorCode::from(&error)),
                }
            }
            _ => guarded_register_plan(
                &mut guard,
                self.device.as_mut(),
                map,
                &registers,
                &mut permit,
            ),
        };
        self.recovery |= guard.recovery_required()
            || matches!(
                result,
                Err(ErrorCode::TimedOut | ErrorCode::RecoveryRequired | ErrorCode::RollbackFailed)
            );
        result
    }
}

fn guarded_register_plan(
    guard: &mut WriteGuard<'_>,
    io: &mut dyn EcRegisterIo,
    map: &EcMap,
    registers: &[(EcWrite, u8)],
    permit: &mut impl FnMut() -> bool,
) -> Result<(), ErrorCode> {
    if registers.is_empty() {
        return Err(ErrorCode::Unsupported);
    }
    let mut before = Vec::<RegisterChange>::with_capacity(registers.len());
    for &(mut write, mask) in registers {
        if map.denies_write(write.address) {
            return Err(ErrorCode::DeniedWrite);
        }
        let raw = if mask != u8::MAX {
            let raw = io.read_byte(write.address).map_err(ErrorCode::from)?;
            write.value = (raw & !mask) | (write.value & mask);
            Some(raw)
        } else {
            None
        };
        let proposed = guard
            .prepare(io, EcWriteIntent::Register(write))
            .map_err(|error| ErrorCode::from(&error))?
            .snapshot()[0];
        if raw.is_some_and(|raw| raw != proposed.before) {
            return Err(ErrorCode::ConflictDetected);
        }
        before.push(proposed);
    }
    let mut completed = Vec::new();
    let result = (|| {
        let mut authorized = false;
        for change in &before {
            let write = EcWrite {
                address: change.address,
                value: change.after,
            };
            let transaction = guard
                .prepare(io, EcWriteIntent::Register(write))
                .map_err(|error| ErrorCode::from(&error))?;
            if transaction.snapshot()[0].before != change.before {
                return Err(ErrorCode::ConflictDetected);
            }
            if !authorized {
                if !permit() {
                    return Err(ErrorCode::NotReady);
                }
                authorized = true;
            }
            transaction
                .commit()
                .map_err(|error| ErrorCode::from(&error))?;
            if change.before != change.after {
                completed.push(*change);
            }
        }
        if before.len() > 1 {
            for change in &before {
                if io.read_byte(change.address).map_err(ErrorCode::from)? != change.after {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        if completed.is_empty() || guard.recovery_required() || error == ErrorCode::TimedOut {
            return Err(error);
        }
        // Core guards each byte and rolls back the failed byte. Restore earlier mode bytes
        // in reverse order only if they still have the values this transaction wrote.
        for change in completed.into_iter().rev() {
            let write = EcWrite {
                address: change.address,
                value: change.before,
            };
            let transaction = guard
                .prepare(io, EcWriteIntent::Register(write))
                .map_err(|_| ErrorCode::RollbackFailed)?;
            if transaction.snapshot()[0].before != change.after {
                return Err(ErrorCode::RollbackFailed);
            }
            transaction
                .commit()
                .map_err(|_| ErrorCode::RollbackFailed)?;
        }
    }
    result
}

const EC_BITS: u32 = CapabilityFlags::PERFORMANCE_MODE.bits()
    | CapabilityFlags::FAN_CURVE.bits()
    | CapabilityFlags::COOLER_BOOST.bits()
    | CapabilityFlags::CHARGE_LIMIT.bits();
const PLATFORM_BITS: u32 = CapabilityFlags::DISPLAY_REFRESH_RATE.bits()
    | CapabilityFlags::DISPLAY_BRIGHTNESS.bits()
    | CapabilityFlags::COLOR_PROFILE.bits()
    | CapabilityFlags::KEYBOARD_LIGHTING.bits();

pub(crate) fn is_ec(target: ControlTarget) -> bool {
    target.capability().bits() & EC_BITS != 0
}
pub(crate) fn mask(flags: CapabilityFlags, bits: u32) -> CapabilityFlags {
    CapabilityFlags::from_bits_retain(flags.bits() & bits)
}
pub(crate) fn ec_off(flags: CapabilityFlags) -> CapabilityFlags {
    mask(flags, !EC_BITS)
}

fn ec_error(status: EcWriteStatus) -> ErrorCode {
    match status {
        EcWriteStatus::ReadOnlyFirmware => ErrorCode::ReadOnlyFirmware,
        EcWriteStatus::ConflictDetected => ErrorCode::ConflictDetected,
        EcWriteStatus::RecoveryRequired => ErrorCode::RecoveryRequired,
        EcWriteStatus::UnverifiedMapping => ErrorCode::WriteNotAllowed,
        EcWriteStatus::Unavailable | EcWriteStatus::Enabled => ErrorCode::Unsupported,
    }
}

pub(crate) fn validate(
    state: &HardwareState,
    writable: CapabilityFlags,
    ec_status: EcWriteStatus,
    change: &ControlChange,
) -> Result<(), ErrorCode> {
    change.validate_parameters()?;
    if is_ec(change.target()) && ec_status != EcWriteStatus::Enabled {
        return Err(ec_error(ec_status));
    }
    if !writable.contains(change.capability()) {
        return Err(ErrorCode::Unsupported);
    }
    change.validate_against(state)?;
    let baseline = observed(state, change.target()).ok_or(ErrorCode::Unsupported)?;
    baseline.validate_against(state)?;
    if state.cooler_boost == Some(true) && matches!(change, ControlChange::SetFanCurve { .. }) {
        return Err(ErrorCode::ConflictDetected);
    }
    Ok(())
}

fn normalize(state: &mut HardwareState) {
    state.supported_performance_modes.truncate(4);
    state.supported_refresh_rates.retain(|&hz| hz != 0);
    state.supported_refresh_rates.sort_unstable();
    state.supported_refresh_rates.dedup();
    state.supported_refresh_rates.truncate(128);
    state
        .supported_charge_limits
        .retain(|v| (1..=100).contains(v));
    state.supported_charge_limits.sort_unstable();
    state.supported_charge_limits.dedup();
    state.supported_color_profiles.retain(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    state.supported_color_profiles.truncate(64);
    for identity in [
        &mut state.identity.board_id,
        &mut state.identity.ec_firmware,
    ] {
        if identity.as_ref().is_some_and(|id| id.len() > 128) {
            *identity = None;
        }
    }
    if state
        .color_profile_id
        .as_ref()
        .is_some_and(|id| id.len() > 64)
    {
        state.color_profile_id = None;
    }
}

pub struct DaemonBackends {
    pub hal: Box<dyn FnOnce() -> HalBackends + Send>,
    pub automation: Box<dyn AutomationEngine + Send>,
}

impl DaemonBackends {
    pub fn new(
        hal: impl FnOnce() -> HalBackends + Send + 'static,
        automation: impl AutomationEngine + Send + 'static,
    ) -> Self {
        Self {
            hal: Box::new(hal),
            automation: Box::new(automation),
        }
    }

    pub fn noop() -> Self {
        Self::new(HalBackends::noop, Noop)
    }
}

impl Default for DaemonBackends {
    fn default() -> Self {
        Self::new(
            || crate::BackendOptions::default().hal(),
            msilt_automation::RuleEngine::new(),
        )
    }
}

struct Noop;
impl HardwareRead for Noop {
    fn inspect(&mut self) -> HalResult<HardwareReport> {
        Ok(HardwareReport::default())
    }
    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
        Err(HalError::Unsupported)
    }
}
impl EcRegisterIo for Noop {
    fn read_byte(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn write_byte(&mut self, _: &AuthorizedEcWrite) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
}
impl PlatformControl for Noop {
    fn set_display_refresh_rate(&mut self, _: u16) -> HalResult<u16> {
        Err(HalError::Unsupported)
    }
    fn set_display_brightness(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn set_color_profile(&mut self, _: &str) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
    fn set_keyboard_lighting(&mut self, _: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
}
impl AutomationEngine for Noop {
    fn on_event(
        &mut self,
        _: &AutomationEvent,
        _: AutomationContext<'_>,
        _: &mut Vec<AutomationAction>,
    ) {
    }
    fn next_deadline_monotonic_ms(&self) -> Option<u64> {
        None
    }
}

#[derive(Default)]
struct EcGovernor {
    recent: Option<Instant>,
    fan: Option<Instant>,
    charge: Option<Instant>,
    bucket: Option<(u8, Instant)>,
}

impl EcGovernor {
    fn admit(&mut self, target: ControlTarget, now: Instant) -> Result<(), ErrorCode> {
        let cooling = matches!(target, ControlTarget::FanCurve { .. });
        let charging = matches!(target, ControlTarget::ChargeLimit {});
        let too_soon = |last: Option<Instant>, ms| {
            last.is_some_and(|last| now.duration_since(last) < Duration::from_millis(ms))
        };
        if too_soon(self.recent, 250)
            || cooling && too_soon(self.fan, 400)
            || charging && too_soon(self.charge, 5000)
        {
            return Err(ErrorCode::Busy);
        }
        let (available, since) = self.bucket.get_or_insert((4, now));
        let refills = (now.duration_since(*since).as_millis() / 2500).min(4) as u8;
        if refills != 0 {
            *available = available.saturating_add(refills).min(4);
            *since = if *available == 4 {
                now
            } else {
                *since + Duration::from_millis(u64::from(refills) * 2500)
            };
        }
        if *available == 0 {
            return Err(ErrorCode::Busy);
        }
        *available -= 1;
        self.recent = Some(now);
        if cooling {
            self.fan = Some(now);
        }
        if charging {
            self.charge = Some(now);
        }
        Ok(())
    }
}

pub(crate) struct Observation {
    pub(crate) report: HardwareReport,
    pub(crate) writable: CapabilityFlags,
    pub(crate) ec_status: EcWriteStatus,
}

pub(crate) struct Applied {
    pub(crate) result: Result<(), ErrorCode>,
    pub(crate) before: Option<ControlChange>,
    pub(crate) observation: Option<Observation>,
}

impl Applied {
    fn failure(error: ErrorCode, observation: Option<Observation>) -> Self {
        Self {
            result: Err(error),
            before: None,
            observation,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SystemTask {
    Inspect,
    Startup(bool),
    Oem(OemAction),
}

impl SystemTask {
    pub(crate) fn mutates(self) -> bool {
        !matches!(self, Self::Inspect)
    }
}

pub(crate) enum Work {
    Inspect {
        topology: bool,
    },
    Apply {
        change: ControlChange,
        expected: HardwareIdentity,
        lock_lighting: bool,
    },
    Sample,
    SystemTools {
        task: SystemTask,
        backup: PathBuf,
    },
}

fn allowed_work(work: &Work, inbox: &Inbox) -> bool {
    match work {
        Work::SystemTools {
            task: SystemTask::Inspect,
            ..
        } => true,
        Work::Apply {
            change,
            lock_lighting: true,
            ..
        } => inbox.can_dim_locked_keyboard() && dark_keyboard(change),
        _ => inbox.can_work(),
    }
}

pub(crate) enum Completion {
    Inspect(HalResult<Observation>, Option<u16>),
    Applied(Applied),
    Sample(HalResult<TelemetrySample>),
    SystemTools(Result<IpcReply, ErrorCode>),
    Skipped(ErrorCode),
}

const PENDING: u8 = 0;
const PREFLIGHT: u8 = 1;
const WRITING: u8 = 2;
const CANCELED: u8 = 3;

pub(crate) struct Ticket(AtomicU8);
impl Ticket {
    fn new() -> Self {
        Self(AtomicU8::new(PENDING))
    }
    fn start(&self) -> bool {
        self.0
            .compare_exchange(PENDING, PREFLIGHT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn start_write(&self) -> bool {
        self.0
            .compare_exchange(PREFLIGHT, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CANCELED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn stop_before_write(&self) -> bool {
        let mut phase = self.0.load(Ordering::Acquire);
        loop {
            if phase == WRITING {
                return false;
            }
            match self
                .0
                .compare_exchange_weak(phase, CANCELED, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(current) => phase = current,
            }
        }
    }
}

struct Job {
    work: Work,
    ticket: Arc<Ticket>,
    queue_by: Option<Instant>,
    budget_by: Instant,
}

pub(crate) struct Executor {
    factory: Option<Box<dyn FnOnce() -> HalBackends + Send>>,
    sender: Option<SyncSender<Job>>,
    receiver: Receiver<Completion>,
    receipts: Option<SyncSender<Completion>>,
    thread: Option<JoinHandle<()>>,
    port: Port,
    inbox: Arc<Inbox>,
}

impl Executor {
    pub(crate) fn new(
        port: &Port,
        inbox: Arc<Inbox>,
        factory: Box<dyn FnOnce() -> HalBackends + Send>,
    ) -> Self {
        let (receipts, receiver) = sync_channel(1);
        Self {
            factory: Some(factory),
            sender: None,
            receiver,
            receipts: Some(receipts),
            thread: None,
            port: port.clone(),
            inbox,
        }
    }

    pub(crate) fn submit(
        &mut self,
        work: Work,
        queue_by: Option<Instant>,
        budget_by: Instant,
    ) -> io::Result<Arc<Ticket>> {
        if self.sender.is_none() {
            let factory = self
                .factory
                .take()
                .ok_or_else(|| io::Error::other("HAL factory unavailable"))?;
            let result = self
                .receipts
                .take()
                .ok_or_else(|| io::Error::other("HAL completion channel unavailable"))?;
            let port = self.port.clone();
            let inbox = self.inbox.clone();
            let (sender, jobs) = sync_channel::<Job>(1);
            self.thread = Some(
                thread::Builder::new()
                    .name("msilt-hal".into())
                    .stack_size(512 * 1024)
                    .spawn(move || {
                        let mut backend = None;
                        let mut factory = Some(factory);
                        while let Ok(Job {
                            work,
                            ticket,
                            queue_by,
                            budget_by,
                        }) = jobs.recv()
                        {
                            let completion = if queue_by.is_some_and(|by| Instant::now() >= by)
                                || !ticket.start()
                            {
                                Completion::Skipped(ErrorCode::RequestExpired)
                            } else if !allowed_work(&work, &inbox) || Instant::now() >= budget_by {
                                Completion::Skipped(ErrorCode::NotReady)
                            } else {
                                let backend = backend.get_or_insert_with(|| {
                                    factory.take().expect("fresh HAL factory")()
                                });
                                match work {
                                    Work::Inspect { topology } => {
                                        let result = backend.inspect();
                                        let count = (topology
                                            && !matches!(result, Err(HalError::TimedOut)))
                                        .then(external_displays)
                                        .flatten();
                                        Completion::Inspect(result, count)
                                    }
                                    Work::Apply {
                                        change,
                                        expected,
                                        lock_lighting,
                                    } => Completion::Applied(backend.apply(
                                        &change,
                                        &expected,
                                        || {
                                            (if lock_lighting {
                                                inbox.can_dim_locked_keyboard()
                                                    && dark_keyboard(&change)
                                            } else {
                                                inbox.can_work()
                                            }) && Instant::now() < budget_by
                                                && ticket.start_write()
                                        },
                                    )),
                                    Work::Sample => {
                                        Completion::Sample(backend.device.sample_telemetry())
                                    }
                                    Work::SystemTools { task, backup } => {
                                        let result = if task.mutates() && backend.readonly {
                                            Err(ErrorCode::WriteNotAllowed)
                                        } else if task.mutates()
                                            && (!inbox.can_work() || !ticket.start_write())
                                        {
                                            Err(ErrorCode::NotReady)
                                        } else {
                                            system_tools(task, &backup)
                                        };
                                        Completion::SystemTools(result)
                                    }
                                }
                            };
                            if result.try_send(completion).is_err() || port.post(WAKE_HAL).is_err()
                            {
                                break;
                            }
                        }
                    })?,
            );
            self.sender = Some(sender);
        }
        let ticket = Arc::new(Ticket::new());
        self.sender
            .as_ref()
            .expect("started executor")
            .try_send(Job {
                work,
                ticket: ticket.clone(),
                queue_by,
                budget_by,
            })
            .map_err(|_| io::Error::other("HAL worker unavailable"))?;
        Ok(ticket)
    }

    pub(crate) fn completion(&self) -> Option<Completion> {
        self.receiver.try_recv().ok()
    }
}

fn system_tools(task: SystemTask, backup: &Path) -> Result<IpcReply, ErrorCode> {
    let (startup, oem) = match task {
        SystemTask::Inspect => (startup::inspect_startup(), oem_manager::inspect_oem(backup)),
        SystemTask::Startup(enabled) => (
            startup::set_startup(enabled).map_err(system_error)?,
            oem_manager::inspect_oem(backup),
        ),
        SystemTask::Oem(action) => {
            let oem = oem_manager::execute_oem_action(action, backup).map_err(system_error)?;
            (startup::inspect_startup(), oem)
        }
    };
    Ok(IpcReply::SystemTools { startup, oem })
}

fn system_error(error: Diagnostic) -> ErrorCode {
    eprintln!("msilt-daemon: system tools: {error}");
    match error.kind {
        DiagnosticKind::AccessDenied => ErrorCode::AccessDenied,
        DiagnosticKind::Unavailable => ErrorCode::BackendUnavailable,
        DiagnosticKind::Busy => ErrorCode::Busy,
        DiagnosticKind::ConflictDetected => ErrorCode::ConflictDetected,
        DiagnosticKind::Unsupported => ErrorCode::Unsupported,
        DiagnosticKind::TimedOut => ErrorCode::TimedOut,
        DiagnosticKind::InvalidData => ErrorCode::ValidationFailed,
        DiagnosticKind::Rejected => ErrorCode::VerificationFailed,
        DiagnosticKind::Io => ErrorCode::WriteFailed,
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take().filter(JoinHandle::is_finished) {
            let _ = thread.join();
        }
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(u64::MAX as u128) as u64
        })
}

fn external_displays() -> Option<u16> {
    for _ in 0..3 {
        let (mut paths, mut modes) = (0, 0);
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) }.0
            != 0
            || paths > 128
            || modes > 512
        {
            return None;
        }
        if paths == 0 {
            return Some(0);
        }
        let mut connectors = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut data = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut paths,
                connectors.as_mut_ptr(),
                &mut modes,
                data.as_mut_ptr(),
                None,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status.0 != 0 {
            return None;
        }
        let mut external = Vec::new();
        for path in connectors.get(..paths as usize)? {
            let target = path.targetInfo;
            if !target.targetAvailable.as_bool() {
                continue;
            }
            match target.outputTechnology {
                DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => continue,
                DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER => return None,
                _ => {}
            }
            let id = (
                target.adapterId.HighPart,
                target.adapterId.LowPart,
                target.id,
            );
            if !external.contains(&id) {
                external.push(id);
            }
        }
        return Some(external.len() as u16);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    const FLAG: CapabilityFlags = CapabilityFlags::COOLER_BOOST;
    const REGISTER: u8 = 0x22;

    fn identity() -> HardwareIdentity {
        HardwareIdentity {
            board_id: Some("TEST-BOARD".into()),
            ec_firmware: Some("TESTEC1.1".into()),
        }
    }

    struct TestEc {
        byte: u8,
        writes: Rc<Cell<u16>>,
    }

    impl HardwareRead for TestEc {
        fn inspect(&mut self) -> HalResult<HardwareReport> {
            Ok(HardwareReport {
                readable: FLAG,
                controls_detected: FLAG,
                state: HardwareState {
                    identity: identity(),
                    cooler_boost: Some(self.byte != 0),
                    ..Default::default()
                },
            })
        }
        fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
            Err(HalError::Unsupported)
        }
    }

    impl EcRegisterIo for TestEc {
        fn read_byte(&mut self, address: u8) -> HalResult<u8> {
            assert_eq!(address, REGISTER);
            Ok(self.byte)
        }
        fn write_byte(&mut self, write: &AuthorizedEcWrite) -> HalResult<()> {
            assert_eq!(write.address(), REGISTER);
            self.writes.set(self.writes.get() + 1);
            self.byte = write.value();
            Ok(())
        }
    }

    impl PlatformControl for TestEc {
        fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
            Noop.set_display_refresh_rate(hz)
        }
        fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8> {
            Noop.set_display_brightness(percent)
        }
        fn set_color_profile(&mut self, id: &str) -> HalResult<()> {
            Noop.set_color_profile(id)
        }
        fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
            Noop.set_keyboard_lighting(zones)
        }
    }

    struct Policy {
        map: EcMap,
        address: u8,
    }
    impl EcWritePolicy for Policy {
        fn authorize(&mut self, _: &HardwareReport) -> Result<&EcMap, EcWriteStatus> {
            Ok(&self.map)
        }
        fn writable_targets(&self) -> CapabilityFlags {
            FLAG
        }
        fn register_intent(&self, _: &ControlChange) -> Option<EcWrite> {
            Some(EcWrite {
                address: self.address,
                value: 1,
            })
        }
    }

    fn policy(address: u8) -> Policy {
        let map = EcMap::parse_json(
            br#"{
            "schemaVersion":1,"boardId":"TEST-BOARD","firmwarePrefixes":["TESTEC1.1"],
            "writeAllowlist":[{"address":34,"values":[0,1]}]
        }"#,
        )
        .unwrap();
        Policy { map, address }
    }

    #[test]
    fn ec_writes_require_policy_and_guard_and_publish_verified_readback() {
        let writes = Rc::new(Cell::new(0));
        let start = || {
            HalBackends::new(
                TestEc {
                    byte: 0,
                    writes: writes.clone(),
                },
                CapabilityFlags::NONE,
            )
        };
        let on = ControlChange::SetCoolerBoost { enabled: true };
        assert_eq!(
            start().apply(&on, &identity(), || true).result,
            Err(ErrorCode::WriteNotAllowed)
        );
        assert_eq!(
            start()
                .with_ec_policy(policy(0x50))
                .apply(&on, &identity(), || true)
                .result,
            Err(ErrorCode::DeniedWrite)
        );
        assert_eq!(
            start()
                .with_ec_policy(policy(REGISTER))
                .apply(&on, &identity(), || false)
                .result,
            Err(ErrorCode::NotReady)
        );
        assert_eq!(writes.get(), 0);
        assert_eq!(
            start()
                .with_ec_policy(policy(REGISTER))
                .read_only()
                .apply(&on, &identity(), || true)
                .result,
            Err(ErrorCode::ReadOnlyFirmware)
        );
        assert_eq!(writes.get(), 0);
        let applied = start()
            .with_ec_policy(policy(REGISTER))
            .apply(&on, &identity(), || true);
        assert_eq!(applied.result, Ok(()));
        assert_eq!(
            applied.before,
            Some(ControlChange::SetCoolerBoost { enabled: false })
        );
        assert_eq!(
            applied.observation.unwrap().report.state.cooler_boost,
            Some(true)
        );
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn explicit_readonly_masks_even_an_injected_platform_writer() {
        let flag = CapabilityFlags::DISPLAY_REFRESH_RATE;
        let mut device = msilt_hal::mock::MockHal::default();
        device.report = HardwareReport {
            readable: flag,
            controls_detected: flag,
            state: HardwareState {
                identity: identity(),
                display_refresh_hz: Some(240),
                supported_refresh_rates: vec![60, 240],
                ..Default::default()
            },
        };
        let mut backend = HalBackends::new(device, flag).read_only();
        let before = backend.inspect().unwrap();
        assert_eq!(before.writable, CapabilityFlags::NONE);
        let result = backend.apply(
            &ControlChange::SetDisplayRefreshRate { hz: 60 },
            &identity(),
            || true,
        );
        assert_eq!(result.result, Err(ErrorCode::Unsupported));
        assert_eq!(backend.inspect().unwrap().report, before.report);
    }

    #[test]
    fn compound_scene_writes_preserve_masks_and_roll_back_on_a_settled_error() {
        use msilt_hal::{
            mock::{MockHal, MockWrite},
            ms17s3::*,
        };
        let identity = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.107".into()),
        };
        let map = write_map().unwrap();
        let mode = [
            (
                EcWrite {
                    address: FAN_MODE,
                    value: 0x0D,
                },
                u8::MAX,
            ),
            (
                EcWrite {
                    address: SHIFT,
                    value: 0xC2,
                },
                u8::MAX,
            ),
            (
                EcWrite {
                    address: BATTERY_MODE,
                    value: 0x0F,
                },
                BATTERY_MODE_MASK,
            ),
        ];
        let mut device = MockHal::default()
            .with_register(FAN_MODE, 0x1D)
            .with_register(SHIFT, 0xC1)
            .with_register(BATTERY_MODE, 0xB0)
            .with_register(COOLER, 0x12);
        device.script_write(MockWrite::Apply);
        device.script_write(MockWrite::FailAfter(HalError::Io));
        let mut guard = WriteGuard::new(map, &identity);
        assert_eq!(
            guarded_register_plan(&mut guard, &mut device, map, &mode, &mut || true),
            Err(ErrorCode::WriteFailed)
        );
        assert_eq!(
            [
                device.register(FAN_MODE),
                device.register(SHIFT),
                device.register(BATTERY_MODE)
            ],
            [Some(0x1D), Some(0xC1), Some(0xB0)]
        );
        assert!(!guard.recovery_required());
        let mut tickets = 0;
        guarded_register_plan(&mut guard, &mut device, map, &mode, &mut || {
            tickets += 1;
            true
        })
        .unwrap();
        assert_eq!(tickets, 1);
        assert_eq!(device.register(BATTERY_MODE), Some(0xBF));
        guarded_register_plan(
            &mut guard,
            &mut device,
            map,
            &[(
                EcWrite {
                    address: COOLER,
                    value: COOLER_MASK,
                },
                COOLER_MASK,
            )],
            &mut || true,
        )
        .unwrap();
        assert_eq!(device.register(COOLER), Some(0x92));
    }
}
