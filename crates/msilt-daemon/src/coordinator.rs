use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        mpsc::{Receiver, sync_channel},
    },
    time::{Duration, Instant},
};

use msilt_automation::{AutomationRuleSet, RuleEngine};
use msilt_core::*;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use crate::{
    automation_rules,
    config_store::ConfigStore,
    desktop::UiSender,
    events::*,
    hal_executor::{
        self as hal, Applied, Completion, Executor, Observation, SystemTask, Ticket, Work, unix_ms,
    },
    iocp::*,
    lease::{LeaseArbiter, observed},
    pipe_server::{Peer, PipeServer, Request},
    qos::Qos,
    security::Security,
    tray::mode_name,
};

const MAX_TRANSACTIONS: usize = 32;
const MAX_ACTIONS: usize = 32;
const MAX_ENGINE_EVENTS: usize = 64;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const INSPECT_BUDGET: Duration = Duration::from_secs(10);
const SAMPLE_BUDGET: Duration = Duration::from_secs(3);
const CONTROL_BUDGET: Duration = Duration::from_secs(12);
const SYSTEM_READ_BUDGET: Duration = Duration::from_secs(10);
const SYSTEM_WRITE_BUDGET: Duration = Duration::from_secs(120);
const INSPECT_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
const MAX_CONSECUTIVE_SAMPLE_TIMEOUTS: u8 = 3;

pub(crate) struct Host {
    pub(crate) port: Port,
    pub(crate) security: Security,
    pub(crate) name: String,
    pub(crate) store: ConfigStore,
    pub(crate) backends: hal::DaemonBackends,
    pub(crate) inbox: Arc<Inbox>,
    pub(crate) window_actions: Receiver<UserEvent>,
    pub(crate) desktop: Option<Arc<UiSender>>,
    pub(crate) process_qos: bool,
    pub(crate) readonly: bool,
    pub(crate) load_stored_rules: bool,
    #[cfg(test)]
    pub(crate) diagnostics: Arc<Diagnostics>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct Diagnostics {
    pub(crate) listening: std::sync::atomic::AtomicBool,
    pub(crate) telemetry_armed: std::sync::atomic::AtomicBool,
    pub(crate) queued: std::sync::atomic::AtomicUsize,
}

pub(crate) fn run(
    Host {
        port,
        security,
        name,
        mut store,
        backends,
        inbox,
        window_actions,
        desktop,
        process_qos,
        readonly,
        load_stored_rules,
        #[cfg(test)]
        diagnostics,
    }: Host,
) -> io::Result<()> {
    let config = store.load();
    let fan_profiles = store.load_fan_profiles();
    let engine = if load_stored_rules {
        store.load_automation()
    } else {
        RuleEngine::new()
    };
    let pipe = PipeServer::new(port.clone(), security, &name)?;
    let timer = DeadlineTimer::new(&port)?;
    let coordinator = Coordinator {
        port: port.clone(),
        pipe,
        timer,
        store,
        config,
        fan_profiles,
        config_revision: 0,
        snapshot: Arc::default(),
        observation: None,
        qos: Qos::new(process_qos),
        boost_until: None,
        pending_fan_restore_at: None,
        executor: Executor::new(&port, inbox.clone(), backends.hal),
        flight: None,
        late_read: None,
        quarantine: false,
        leases: LeaseArbiter::default(),
        engine,
        alternate_engine: (!load_stored_rules).then_some(backends.automation),
        readonly,
        queue: VecDeque::new(),
        greetings: Vec::new(),
        engine_events: VecDeque::new(),
        actions: Vec::new(),
        processed_engine_deadline: None,
        started: Instant::now(),
        telemetry: Telemetry::default(),
        gpu_telemetry_enabled: false,
        inbox,
        window_actions,
        desktop,
        suspended: false,
        unlocked: true,
        display_on: true,
        power_source: None,
        battery: None,
        saver: None,
        effective: None,
        inspect_requested: false,
        inspect_retry_at: None,
        inspect_retry_failures: 0,
        sample_timeouts: 0,
        topology_due: None,
        topology_requested: false,
        external_count: None,
        epoch: 0,
        ec_recovery: false,
        initialized: false,
        lock_lighting_pending: false,
        lock_lighting_owner: None,
        #[cfg(test)]
        diagnostics,
    };
    #[cfg(test)]
    coordinator
        .diagnostics
        .listening
        .store(true, std::sync::atomic::Ordering::Release);
    coordinator.loop_()
}

#[derive(Clone, Copy)]
enum Origin {
    Pipe { peer: Peer, id: u64 },
    Tray,
    Automatic,
    LockLighting,
    InternalRestore,
}

impl Origin {
    fn automatic(self) -> bool {
        matches!(
            self,
            Self::Automatic | Self::LockLighting | Self::InternalRestore
        )
    }
}

enum Command {
    Change(ControlChange),
    Release(ControlTarget),
    ResetFanMode,
    System(SystemTask),
}
struct Job {
    source: Origin,
    expected: u64,
    until: Instant,
    command: Command,
}

struct Greeting {
    peer: Peer,
    id: u64,
    until: Instant,
}

enum Inflight {
    Inspect,
    System {
        source: Origin,
        write: bool,
    },
    Control {
        source: Origin,
        change: ControlChange,
        release: bool,
    },
    ResetFanMode {
        source: Origin,
    },
    Sample(u64),
}

struct Flight {
    kind: Inflight,
    by: Instant,
    epoch: u64,
    ticket: Arc<Ticket>,
}

struct LateRead {
    epoch: u64,
    inspect: bool,
}

#[derive(Default)]
struct Telemetry {
    next: Option<Instant>,
    generation: u64,
}

impl Telemetry {
    fn set(&mut self, active: bool, now: Instant) -> bool {
        if self.next.is_some() == active {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        self.next = active.then(|| now + SAMPLE_INTERVAL);
        true
    }

    fn due(&mut self, now: Instant) -> bool {
        if self.next.is_none_or(|next| now < next) {
            return false;
        }
        self.next = Some(now + SAMPLE_INTERVAL);
        true
    }
}

struct Coordinator {
    port: Port,
    pipe: PipeServer,
    timer: DeadlineTimer,
    store: ConfigStore,
    config: AppConfig,
    fan_profiles: FanProfileStore,
    config_revision: u64,
    snapshot: Arc<SystemSnapshot>,
    observation: Option<Observation>,
    qos: Qos,
    boost_until: Option<Instant>,
    pending_fan_restore_at: Option<Instant>,
    executor: Executor,
    flight: Option<Flight>,
    late_read: Option<LateRead>,
    quarantine: bool,
    leases: LeaseArbiter,
    engine: RuleEngine,
    alternate_engine: Option<Box<dyn AutomationEngine + Send>>,
    readonly: bool,
    queue: VecDeque<Job>,
    greetings: Vec<Greeting>,
    engine_events: VecDeque<AutomationEvent>,
    actions: Vec<AutomationAction>,
    processed_engine_deadline: Option<u64>,
    started: Instant,
    telemetry: Telemetry,
    gpu_telemetry_enabled: bool,
    inbox: Arc<Inbox>,
    window_actions: Receiver<UserEvent>,
    desktop: Option<Arc<UiSender>>,
    suspended: bool,
    unlocked: bool,
    display_on: bool,
    power_source: Option<Option<PowerSource>>,
    battery: Option<Option<u8>>,
    saver: Option<bool>,
    effective: Option<Option<EffectivePowerMode>>,
    inspect_requested: bool,
    inspect_retry_at: Option<Instant>,
    inspect_retry_failures: u32,
    sample_timeouts: u8,
    topology_due: Option<Instant>,
    topology_requested: bool,
    external_count: Option<u16>,
    epoch: u64,
    ec_recovery: bool,
    initialized: bool,
    lock_lighting_pending: bool,
    lock_lighting_owner: Option<LeaseOwner>,
    #[cfg(test)]
    diagnostics: Arc<Diagnostics>,
}

impl Coordinator {
    fn loop_(mut self) -> io::Result<()> {
        self.qos.set_interactive(false);
        self.process_window(Instant::now());
        if self.config.automation.enabled {
            self.seed_automation();
        }
        let result = self.dispatch_loop();
        self.timer.set(None);
        self.telemetry.set(false, Instant::now());
        self.executor.set_gpu_telemetry(false);
        #[cfg(test)]
        self.diagnostics
            .telemetry_armed
            .store(false, std::sync::atomic::Ordering::Release);
        self.queue.clear();
        self.greetings.clear();
        Arc::make_mut(&mut self.snapshot).status = DaemonStatus::Stopping;
        let flight = self.flight.take();
        drop(self.pipe);
        if let Some(flight) = flight {
            // An accepted hardware transaction owns its rollback; stop taking work, not its worker.
            flight.ticket.stop_before_write();
            self.timer.set(Some(flight.by));
            while Instant::now() < flight.by && self.executor.completion().is_none() {
                if self.port.wait().is_err() {
                    break;
                }
            }
        }
        result
    }

    fn dispatch_loop(&mut self) -> io::Result<()> {
        loop {
            let now = Instant::now();
            self.pipe.expire(now);
            self.prune(now);
            if self.flight.as_ref().is_some_and(|flight| now >= flight.by) {
                if let Some(completion) = self.executor.completion() {
                    self.on_completion(completion, now);
                } else {
                    self.watchdog(now);
                }
            }
            self.sync_telemetry(now);
            let sample_due = self.telemetry.due(now);
            self.flush_greetings(now);
            self.process_deadlines(now);
            self.run_engine(now);
            self.schedule(now, sample_due);
            self.pipe.service(now)?;
            let now = Instant::now();
            self.sync_telemetry(now);
            self.update_qos(now);
            self.timer.set(self.next_deadline());
            #[cfg(test)]
            self.diagnostics
                .queued
                .store(self.queue.len(), std::sync::atomic::Ordering::Release);

            let packet = self.port.wait()?;
            let now = Instant::now();
            match packet.key {
                WAKE_STOP => return Ok(()),
                WAKE_WINDOW => self.process_window(now),
                WAKE_HAL => {
                    if let Some(completion) = self.executor.completion() {
                        self.on_completion(completion, now);
                    }
                }
                WAKE_TIMER => self.timer.fired(),
                _ => {
                    if let Some(request) = self.pipe.completed(packet, now) {
                        self.request(request);
                    }
                }
            }
        }
    }

    fn request(
        &mut self,
        Request {
            peer,
            received,
            data,
        }: Request,
    ) {
        let id = data.id;
        let authenticated = self.pipe.client(peer);
        if !self.pipe.check_id(peer, id) {
            self.pipe
                .reply(peer, id, Err(ErrorCode::InvalidRequest), true);
            return;
        }
        let ttl = match data.queue_ttl_ms() {
            Ok(ttl) => ttl,
            Err(error) => {
                self.pipe
                    .reply(peer, id, Err(error), authenticated.is_none());
                return;
            }
        };
        let until = received + Duration::from_millis(u64::from(ttl));
        let command = data.command;
        if let (None, IpcCommand::Hello { version, client }) = (authenticated, &command) {
            let verified = match self.pipe.authenticate(peer, *client) {
                Ok(verified) => verified,
                Err(error) => {
                    self.pipe.reply(peer, id, Err(error), true);
                    return;
                }
            };
            if *version != IPC_VERSION {
                self.pipe
                    .reply(peer, id, Err(ErrorCode::IncompatibleVersion), true);
                return;
            }
            self.pipe.authenticated(peer, *client, verified);
            if self.snapshot.status == DaemonStatus::Starting && self.active() && !self.quarantine {
                if self.flight.is_none() && self.late_read.is_none() {
                    self.inspect_requested = true;
                    self.inspect_retry_at = None;
                }
                self.greetings.push(Greeting { peer, id, until });
            } else {
                if (self.observation.is_none() || self.snapshot.status == DaemonStatus::Degraded)
                    && self.active()
                    && !self.quarantine
                    && self.flight.is_none()
                    && self.late_read.is_none()
                {
                    self.inspect_requested = true;
                    self.inspect_retry_at = None;
                }
                self.pipe.reply(peer, id, Ok(self.hello()), false);
            }
            return;
        }
        let Some((client, verified_ui)) = authenticated else {
            self.pipe
                .reply(peer, id, Err(ErrorCode::InvalidRequest), true);
            return;
        };
        if Instant::now() >= until {
            self.pipe
                .reply(peer, id, Err(ErrorCode::RequestExpired), false);
            return;
        }
        let source = Origin::Pipe { peer, id };
        let reply = match command {
            IpcCommand::Hello { .. } => {
                self.pipe
                    .reply(peer, id, Err(ErrorCode::InvalidRequest), true);
                return;
            }
            IpcCommand::GetSnapshot {} => {
                if (self.observation.is_none() || self.snapshot.status == DaemonStatus::Degraded)
                    && self.active()
                    && !self.quarantine
                    && self.flight.is_none()
                    && self.late_read.is_none()
                {
                    self.inspect_requested = true;
                    self.inspect_retry_at = None;
                }
                Ok(IpcReply::Snapshot {
                    snapshot: (*self.snapshot).clone(),
                })
            }
            IpcCommand::SetSubscriptions { topics } => {
                if topics.validate_for(client).is_err() || (topics.telemetry || topics.gpu_telemetry) && !verified_ui {
                    Err(ErrorCode::Unsupported)
                } else {
                    self.pipe.subscriptions(peer, topics);
                    Ok(IpcReply::Subscriptions {
                        topics,
                        snapshot: (*self.snapshot).clone(),
                    })
                }
            }
            IpcCommand::GetConfig {} => Ok(self.config_reply()),
            IpcCommand::SetConfig {
                expected_revision,
                config,
            } => self.set_config(expected_revision, config),
            IpcCommand::GetSystemTools {} => {
                self.enqueue(Job {
                    source,
                    expected: 0,
                    until,
                    command: Command::System(SystemTask::Inspect),
                });
                return;
            }
            IpcCommand::SetStartup { enabled } => {
                if self.readonly {
                    Err(ErrorCode::WriteNotAllowed)
                } else {
                    self.enqueue(Job {
                        source,
                        expected: 0,
                        until,
                        command: Command::System(SystemTask::Startup(enabled)),
                    });
                    return;
                }
            }
            IpcCommand::ManageOem { action } => {
                if self.readonly {
                    Err(ErrorCode::WriteNotAllowed)
                } else {
                    self.enqueue(Job {
                        source,
                        expected: 0,
                        until,
                        command: Command::System(SystemTask::Oem(action)),
                    });
                    return;
                }
            }
            IpcCommand::GetAutomationRules {} => self.automation_reply(false),
            IpcCommand::DryRunAutomation {} => self.automation_reply(true),
            IpcCommand::SetAutomationRuleEnabled { id, enabled } => {
                self.update_automation(|store| store.set_rule_enabled(&id, enabled))
            }
            IpcCommand::AddAutomationPreset { preset } => {
                self.update_automation(|store| store.add_preset(preset))
            }
            IpcCommand::DeleteAutomationRule { id } => {
                self.update_automation(|store| store.delete_rule(&id))
            }
            IpcCommand::TestOsd {} => self.test_osd(),
            IpcCommand::Control {
                expected_revision,
                change,
            } => {
                self.enqueue(Job {
                    source,
                    expected: expected_revision,
                    until,
                    command: Command::Change(change),
                });
                return;
            }
            IpcCommand::ReleaseManualOverride {
                expected_revision,
                target,
            } => {
                self.enqueue(Job {
                    source,
                    expected: expected_revision,
                    until,
                    command: Command::Release(target),
                });
                return;
            }
            IpcCommand::ResetFanMode { expected_revision } => {
                self.enqueue(Job {
                    source,
                    expected: expected_revision,
                    until,
                    command: Command::ResetFanMode,
                });
                return;
            }
        };
        self.pipe.reply(peer, id, reply, false);
    }

    fn config_reply(&self) -> IpcReply {
        IpcReply::Config {
            config: self.config,
            revision: self.config_revision,
        }
    }
    fn hello(&self) -> IpcReply {
        IpcReply::Hello {
            version: IPC_VERSION,
            snapshot: (*self.snapshot).clone(),
        }
    }

    fn set_config(&mut self, expected: u64, config: AppConfig) -> Result<IpcReply, ErrorCode> {
        if self.readonly {
            return Err(ErrorCode::WriteNotAllowed);
        }
        if expected != self.config_revision {
            return Err(ErrorCode::RevisionConflict);
        }
        config.validate().map_err(|_| ErrorCode::ValidationFailed)?;
        let revision = self
            .config_revision
            .checked_add(1)
            .ok_or(ErrorCode::Internal)?;
        self.qos.set_interactive(true);
        self.store.save(&config).map_err(|error| {
            eprintln!("msilt-daemon: could not save app.json: {error}");
            ErrorCode::BackendUnavailable
        })?;
        let old = self.config;
        self.config = config;
        self.config_revision = revision;
        if !config.osd.enabled {
            self.ui(UiEvent::HideOsd);
        }
        if old.automation.enabled != config.automation.enabled {
            self.processed_engine_deadline = None;
            if config.automation.enabled {
                self.seed_automation();
            } else {
                self.engine_events.clear();
                self.release_lock_lighting();
                self.leases.disable_automation();
            }
        }
        if old.osd.enabled != config.osd.enabled
            || old.automation.enabled != config.automation.enabled
        {
            self.publish();
        }
        Ok(self.config_reply())
    }

    fn update_automation(
        &mut self,
        edit: impl FnOnce(&ConfigStore) -> io::Result<AutomationRuleSet>,
    ) -> Result<IpcReply, ErrorCode> {
        if self.readonly {
            return Err(ErrorCode::WriteNotAllowed);
        }
        let set = edit(&self.store).map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => ErrorCode::ConflictDetected,
            io::ErrorKind::NotFound | io::ErrorKind::InvalidData => ErrorCode::ValidationFailed,
            io::ErrorKind::PermissionDenied => ErrorCode::AccessDenied,
            _ => ErrorCode::WriteFailed,
        })?;
        if self.alternate_engine.take().is_some() {
            self.leases.disable_automation();
        }
        self.engine.load(set);
        if !self
            .engine
            .rules()
            .iter()
            .any(automation_rules::is_lock_screen_preset)
        {
            self.release_lock_lighting();
        }
        self.processed_engine_deadline = None;
        self.seed_automation();
        self.run_engine(Instant::now());
        self.automation_reply(false)
    }

    fn automation_reply(&self, dry_run: bool) -> Result<IpcReply, ErrorCode> {
        let now = Instant::now();
        automation_rules::view_reply(
            &self.engine,
            automation_rules::RuleViewContext {
                snapshot: &self.snapshot,
                enabled: self.config.automation.enabled,
                dry_run,
                monotonic_ms: self.now_ms(now),
                unix_ms: unix_ms(),
                unlocked: self.unlocked,
                saver: self.saver,
                effective: self.effective.flatten(),
                external_count: self.external_count,
            },
        )
    }

    fn test_osd(&self) -> Result<IpcReply, ErrorCode> {
        if !self.active() || !self.display_on {
            return Err(ErrorCode::NotReady);
        }
        let desktop = self.desktop.as_ref().ok_or(ErrorCode::BackendUnavailable)?;
        let (done, confirmation) = sync_channel(1);
        if !desktop.send(UiEvent::ShowOsd {
            title: "MSI-Toolkit",
            detail: "屏幕悬浮提示测试".into(),
            duration_ms: self.config.osd.duration_ms,
            done: Some(done),
        }) {
            return Err(ErrorCode::Busy);
        }
        confirmation
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| ErrorCode::TimedOut)??;
        Ok(IpcReply::OsdShown {})
    }

    fn flush_greetings(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.greetings.len() {
            let greeting = &self.greetings[index];
            let reply = if now >= greeting.until {
                Err(ErrorCode::RequestExpired)
            } else if self.snapshot.status != DaemonStatus::Starting
                || !self.active()
                || self.quarantine
            {
                Ok(self.hello())
            } else {
                index += 1;
                continue;
            };
            let greeting = self.greetings.swap_remove(index);
            self.pipe.reply(greeting.peer, greeting.id, reply, false);
        }
    }

    fn enqueue(&mut self, request: Job) {
        if self.queue.len() + self.greetings.len() + usize::from(self.flight.is_some())
            >= MAX_TRANSACTIONS
        {
            self.control_reply(request.source, Err(ErrorCode::Busy));
        } else {
            self.queue.push_back(request);
        }
    }

    fn prune(&mut self, now: Instant) {
        self.greetings
            .retain(|request| self.pipe.alive(request.peer));
        let mut index = 0;
        while index < self.queue.len() {
            let request = &self.queue[index];
            let alive = match request.source {
                Origin::Pipe { peer, .. } => self.pipe.alive(peer),
                _ => true,
            };
            if alive && now < request.until {
                index += 1;
                continue;
            }
            let request = self.queue.remove(index).expect("indexed request");
            if alive {
                self.control_reply(request.source, Err(ErrorCode::RequestExpired));
            }
        }
        if let Some(flight) = &self.flight {
            let source = match flight.kind {
                Inflight::Control { source, .. }
                | Inflight::ResetFanMode { source }
                | Inflight::System { source, .. } => Some(source),
                _ => None,
            };
            if let Some(Origin::Pipe { peer, .. }) = source {
                if !self.pipe.alive(peer) {
                    flight.ticket.stop_before_write();
                }
            }
        }
    }

    fn active(&self) -> bool {
        self.unlocked && !self.suspended && self.inbox.can_work()
    }

    fn schedule(&mut self, now: Instant, sample_due: bool) {
        if self.flight.is_some() || self.late_read.is_some() {
            return;
        }
        if self.active() && !self.quarantine && self.inspect_requested {
            let topology = std::mem::take(&mut self.topology_requested);
            self.inspect_requested = false;
            self.inspect_retry_at = None;
            if self.submit(
                Work::Inspect { topology },
                Inflight::Inspect,
                now + INSPECT_BUDGET,
                None,
            ) {
                return;
            }
        }
        while let Some(request) = self.queue.pop_front() {
            if self.begin_control(request, Instant::now()) {
                return;
            }
        }
        if self.inbox.can_dim_locked_keyboard() && self.schedule_lock_lighting(now) {
            return;
        }
        if !self.active() || self.quarantine {
            return;
        }
        if self
            .pending_fan_restore_at
            .is_some_and(|at| now >= at)
        {
            self.pending_fan_restore_at = None;
            if self.snapshot.hardware.cooler_boost != Some(true) {
                if let Some(mode) = self.snapshot.hardware.performance_mode {
                    if self.fan_profiles.active_profile(mode).is_some()
                        && self.submit_control(
                            Origin::InternalRestore,
                            ControlChange::SetPerformanceMode { mode },
                            false,
                            now,
                            None,
                        )
                    {
                        return;
                    }
                }
            }
        }
        while let Some(change) = self
            .leases
            .next_automatic(self.snapshot.hardware.cooler_boost == Some(true))
        {
            let target = change.target();
            if let Err(error) = self.validate(&change) {
                self.engine_event(AutomationEvent::ControlSettled {
                    target,
                    result: Err(error),
                });
                continue;
            }
            if observed(&self.snapshot.hardware, target).as_ref() == Some(&change) {
                // No hardware was overlaid, but this verified observation can be restored later.
                self.leases.auto_applied(&change, Some(change.clone()));
                continue;
            }
            if self.submit_control(Origin::Automatic, change, false, now, None) {
                return;
            }
        }
        if sample_due {
            self.submit(
                Work::Sample,
                Inflight::Sample(self.telemetry.generation),
                now + SAMPLE_BUDGET,
                None,
            );
        }
    }

    fn schedule_lock_lighting(&mut self, now: Instant) -> bool {
        if !std::mem::take(&mut self.lock_lighting_pending)
            || self.readonly
            || self.quarantine
            || !self.config.automation.enabled
        {
            return false;
        }
        let Some(rule) = self
            .engine
            .rules()
            .iter()
            .find(|rule| automation_rules::is_lock_screen_preset(rule))
        else {
            return false;
        };
        let target = ControlTarget::KeyboardLighting {};
        if self.leases.has_manual(target)
            || self.snapshot.hardware.keyboard_conflict == Some(true)
            || !self.observation.as_ref().is_some_and(|observation| {
                observation
                    .writable
                    .contains(CapabilityFlags::KEYBOARD_LIGHTING)
            })
        {
            return false;
        }
        let owner = LeaseOwner::Rule(rule.owner);
        let lease = AutomationLease {
            owner,
            priority: rule.priority,
            specificity: rule.specificity,
            change: rule.actions[0].clone(),
            expires_at_monotonic_ms: None,
        };
        if !self.leases.acquire(lease, self.now_ms(now)) {
            return false;
        }
        self.lock_lighting_owner = Some(owner);
        if let Some(change) = self.leases.take_locked_keyboard() {
            if self.submit_control(Origin::LockLighting, change, false, now, None) {
                return true;
            }
        }
        self.release_lock_lighting();
        false
    }

    fn release_lock_lighting(&mut self) {
        self.lock_lighting_pending = false;
        if let Some(owner) = self.lock_lighting_owner.take() {
            self.leases
                .release(ControlTarget::KeyboardLighting {}, owner);
        }
    }

    fn begin_control(&mut self, request: Job, now: Instant) -> bool {
        if let Command::System(task) = request.command {
            return self.begin_system(request.source, task, request.until, now);
        }
        let result = if now >= request.until {
            Err(ErrorCode::RequestExpired)
        } else if request.expected != self.snapshot.revision {
            Err(ErrorCode::RevisionConflict)
        } else if !self.active() || self.snapshot.status == DaemonStatus::Starting {
            Err(ErrorCode::NotReady)
        } else if self.quarantine {
            Err(ErrorCode::BackendUnavailable)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            self.control_reply(request.source, Err(error));
            return false;
        }
        let source = request.source;
        let (change, release) = match request.command {
            Command::System(_) => unreachable!("system work dispatched separately"),
            Command::ResetFanMode => {
                return self.begin_reset_fan_mode(source, now, Some(request.until));
            }
            Command::Change(change) => (change, false),
            Command::Release(target) => match self.leases.manual_destination(target) {
                Some(change) => (change, true),
                None => {
                    if self.leases.has_manual(target) {
                        self.leases.release_manual(target, None);
                        self.publish();
                    }
                    self.control_reply(source, Ok(()));
                    return false;
                }
            },
        };
        if let Err(error) = self.validate(&change) {
            self.control_reply(source, Err(error));
            return false;
        }
        if matches!(
            change,
            ControlChange::SetPerformanceMode { .. }
                | ControlChange::SetFanCurve { .. }
                | ControlChange::SetCoolerBoost { enabled: true }
        ) {
            self.pending_fan_restore_at = None;
        }
        self.submit_control(source, change, release, now, Some(request.until))
    }

    fn begin_reset_fan_mode(
        &mut self,
        source: Origin,
        now: Instant,
        until: Option<Instant>,
    ) -> bool {
        if self.snapshot.ec_write_status != EcWriteStatus::Enabled {
            let error = match self.snapshot.ec_write_status {
                EcWriteStatus::ReadOnlyFirmware => ErrorCode::ReadOnlyFirmware,
                EcWriteStatus::ConflictDetected => ErrorCode::ConflictDetected,
                EcWriteStatus::RecoveryRequired => ErrorCode::RecoveryRequired,
                EcWriteStatus::UnverifiedMapping => ErrorCode::WriteNotAllowed,
                EcWriteStatus::Unavailable | EcWriteStatus::Enabled => ErrorCode::Unsupported,
            };
            self.control_reply(source, Err(error));
            return false;
        }
        if !self.snapshot.writable.contains(CapabilityFlags::FAN_CURVE)
            || self.snapshot.hardware.performance_mode.is_none()
        {
            self.control_reply(source, Err(ErrorCode::Unsupported));
            return false;
        }
        if self.snapshot.hardware.cooler_boost == Some(true) {
            self.control_reply(source, Err(ErrorCode::ConflictDetected));
            return false;
        }
        self.pending_fan_restore_at = None;
        if !source.automatic() {
            self.qos.set_interactive(true);
        }
        let work = Work::ResetFanMode {
            expected: self.snapshot.hardware.identity.clone(),
        };
        if !self.submit(
            work,
            Inflight::ResetFanMode { source },
            now + CONTROL_BUDGET,
            until,
        ) {
            self.control_reply(source, Err(ErrorCode::BackendUnavailable));
            return false;
        }
        true
    }

    fn begin_system(
        &mut self,
        source: Origin,
        task: SystemTask,
        until: Instant,
        now: Instant,
    ) -> bool {
        let outcome = if now >= until {
            Err(ErrorCode::RequestExpired)
        } else if task.mutates() && self.readonly {
            Err(ErrorCode::WriteNotAllowed)
        } else if task.mutates() && !self.active() {
            Err(ErrorCode::NotReady)
        } else if self.quarantine {
            Err(ErrorCode::BackendUnavailable)
        } else {
            let create = matches!(
                task,
                SystemTask::Oem(
                    OemAction::DisableAutoStart { .. } | OemAction::DisableAndStop { .. }
                )
            );
            self.store
                .oem_backup(create)
                .map_err(|_| ErrorCode::BackendUnavailable)
        };
        match outcome {
            Ok(backup) => {
                let write = task.mutates();
                let budget = if write {
                    SYSTEM_WRITE_BUDGET
                } else {
                    SYSTEM_READ_BUDGET
                };
                if self.submit(
                    Work::SystemTools { task, backup },
                    Inflight::System { source, write },
                    now + budget,
                    Some(until),
                ) {
                    return true;
                }
                self.system_reply(source, Err(ErrorCode::BackendUnavailable));
            }
            Err(error) => self.system_reply(source, Err(error)),
        }
        false
    }

    fn system_reply(&mut self, source: Origin, result: Result<IpcReply, ErrorCode>) {
        if let Origin::Pipe { peer, id } = source {
            self.pipe.reply(peer, id, result, false);
        }
    }

    fn validate(&self, change: &ControlChange) -> Result<(), ErrorCode> {
        hal::validate(
            &self.snapshot.hardware,
            self.snapshot.writable,
            self.snapshot.ec_write_status,
            change,
        )
    }

    fn submit_control(
        &mut self,
        source: Origin,
        change: ControlChange,
        release: bool,
        now: Instant,
        until: Option<Instant>,
    ) -> bool {
        if !source.automatic() {
            self.qos.set_interactive(true);
        }
        let fan_profile = match &change {
            ControlChange::SetPerformanceMode { mode } => self.fan_profiles.active_profile(*mode),
            _ => None,
        };
        let work = Work::Apply {
            change: change.clone(),
            expected: self.snapshot.hardware.identity.clone(),
            lock_lighting: matches!(source, Origin::LockLighting),
            fan_profile,
        };
        if !self.submit(
            work,
            Inflight::Control {
                source,
                change,
                release,
            },
            now + CONTROL_BUDGET,
            until,
        ) {
            self.control_reply(source, Err(ErrorCode::BackendUnavailable));
            return false;
        }
        true
    }

    fn submit(&mut self, work: Work, kind: Inflight, by: Instant, until: Option<Instant>) -> bool {
        match self.executor.submit(work, until, by) {
            Ok(ticket) => {
                self.flight = Some(Flight {
                    kind,
                    by,
                    epoch: self.epoch,
                    ticket,
                });
                true
            }
            Err(error) => {
                eprintln!("msilt-daemon: HAL worker: {error}");
                self.quarantine = true;
                self.observation = None;
                self.publish();
                false
            }
        }
    }

    fn arm_inspect_retry(&mut self, now: Instant) {
        let index = (self.inspect_retry_failures as usize).min(INSPECT_RETRY_DELAYS.len() - 1);
        self.inspect_retry_failures = self.inspect_retry_failures.saturating_add(1);
        self.inspect_retry_at = Some(now + INSPECT_RETRY_DELAYS[index]);
    }

    fn apply_inspection(&mut self, observation: Observation, topology: Option<u16>) {
        self.initialized = true;
        self.inspect_retry_failures = 0;
        self.inspect_retry_at = None;
        self.sample_timeouts = 0;
        self.observation = Some(observation);
        self.publish();
        self.leases.reconcile_observation();
        if let Some(count) = topology {
            self.update_topology(count);
        }
    }

    fn on_completion(&mut self, completion: Completion, now: Instant) {
        let Some(flight) = self.flight.take() else {
            if let Some(late) = self.late_read.take() {
                self.store
                    .diagnostic("watchdog: late read-only HAL worker finished; resuming");
                match (late.inspect, completion) {
                    (true, Completion::Inspect(Ok(observation), topology))
                        if late.epoch == self.epoch && self.active() && !self.quarantine =>
                    {
                        self.apply_inspection(observation, topology);
                    }
                    (true, Completion::Inspect(Err(error), _)) => {
                        self.store.diagnostic(&format!(
                            "hardware inspect failed after budget: {error:?}; scheduling retry"
                        ));
                        if self.active() && !self.quarantine {
                            self.arm_inspect_retry(now);
                        }
                    }
                    (true, _) => {
                        if self.observation.is_none() && self.active() && !self.quarantine {
                            self.inspect_requested = true;
                        }
                    }
                    (false, Completion::Sample(Ok(_))) => {
                        self.sample_timeouts = 0;
                    }
                    (false, Completion::Sample(Err(error))) => {
                        self.store
                            .diagnostic(&format!("late telemetry sample failed: {error:?}"));
                    }
                    (false, _) => {}
                }
            }
            return;
        };
        let same_epoch = flight.epoch == self.epoch;
        match (flight.kind, completion) {
            (Inflight::System { source, write }, Completion::SystemTools(result)) => {
                self.inspect_requested |= write;
                self.system_reply(source, result);
            }
            (Inflight::System { source, .. }, Completion::Skipped(error)) => {
                self.system_reply(source, Err(error))
            }
            (Inflight::Inspect, Completion::Inspect(result, topology)) => {
                if same_epoch {
                    self.initialized = true;
                    match result {
                        Ok(observation) => self.apply_inspection(observation, topology),
                        Err(error) => {
                            self.store.diagnostic(&format!(
                                "hardware inspect failed: {error:?}; scheduling retry"
                            ));
                            self.observation = None;
                            self.publish();
                            if self.active() && !self.quarantine {
                                self.arm_inspect_retry(now);
                            }
                        }
                    }
                } else if self.observation.is_none() && self.active() && !self.quarantine {
                    self.inspect_requested = true;
                }
            }
            (Inflight::Sample(generation), Completion::Sample(sample)) => {
                match &sample {
                    Ok(_) => self.sample_timeouts = 0,
                    Err(HalError::TimedOut) => {
                        self.sample_timeouts = self.sample_timeouts.saturating_add(1);
                        if self.sample_timeouts >= MAX_CONSECUTIVE_SAMPLE_TIMEOUTS {
                            self.sample_timeouts = 0;
                            self.store.diagnostic(
                                "telemetry timed out repeatedly; refreshing hardware snapshot",
                            );
                            self.inspect_requested = true;
                        }
                    }
                    Err(_) => {}
                }
                if self.telemetry.next.is_some()
                    && generation == self.telemetry.generation
                    && same_epoch
                {
                    self.pipe
                        .send_sample(sample.unwrap_or_else(|_| TelemetrySample {
                            sampled_at_unix_ms: unix_ms(),
                            ..Default::default()
                        }));
                }
            }
            (
                Inflight::Control {
                    source,
                    change,
                    release,
                },
                Completion::Applied(Applied {
                    result,
                    before,
                    observation,
                }),
            ) => {
                let target = change.target();
                if result == Err(ErrorCode::TimedOut) {
                    self.store.diagnostic(
                        "control transaction timed out; entering fail-closed quarantine",
                    );
                    self.quarantine = true;
                }
                self.ec_recovery |= hal::is_ec(target)
                    && matches!(
                        result,
                        Err(ErrorCode::TimedOut
                            | ErrorCode::RollbackFailed
                            | ErrorCode::RecoveryRequired)
                    );
                let boost_released =
                    matches!(change, ControlChange::SetCoolerBoost { enabled: false })
                        && result.is_ok();
                if same_epoch {
                    self.observation = observation;
                }
                if result.is_ok() {
                    if !matches!(source, Origin::InternalRestore) {
                        if release {
                            self.leases.release_manual(target, Some(&change));
                        } else if source.automatic() {
                            self.leases.auto_applied(&change, before);
                        } else {
                            let until = match self.config.automation.manual_override {
                                ManualOverride::Timed { seconds } => {
                                    Some(self.now_ms(now).saturating_add(u64::from(seconds) * 1000))
                                }
                                ManualOverride::UntilReleased => None,
                            };
                            self.leases.manual_applied(target, before, until);
                        }
                    }
                    if same_epoch && !source.automatic() && !release {
                        if let ControlChange::SetFanCurve { channel, curve } = change {
                            let mode = self
                                .observation
                                .as_ref()
                                .and_then(|obs| obs.report.state.performance_mode);
                            let other_curve = self.observation.as_ref().and_then(|obs| match channel {
                                FanChannel::Cpu => obs.report.state.gpu_fan.map(|f| f.curve),
                                FanChannel::Gpu => obs.report.state.cpu_fan.map(|f| f.curve),
                            });
                            if let Some(mode) = mode {
                                if self
                                    .fan_profiles
                                    .record_custom_curve(mode, channel, curve, other_curve)
                                    && !self.readonly
                                {
                                    if let Err(error) =
                                        self.store.save_fan_profiles(&self.fan_profiles)
                                    {
                                        self.store.diagnostic(&format!(
                                            "could not save fan-profiles.json: {error}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                if result.is_err() && matches!(source, Origin::LockLighting) {
                    self.release_lock_lighting();
                }
                if boost_released {
                    self.leases.reconcile_observation();
                    if same_epoch {
                        if let Some(obs) = &self.observation {
                            if let Some(mode) = obs.report.state.performance_mode {
                                if let Some(profile) = self.fan_profiles.active_profile(mode) {
                                    let needs_restore =
                                        obs.report.state.fan_mode != Some(FanMode::Advanced)
                                            || profile.cpu.is_some_and(|c| {
                                                obs.report
                                                    .state
                                                    .cpu_fan
                                                    .is_none_or(|f| f.curve != c)
                                            })
                                            || profile.gpu.is_some_and(|g| {
                                                obs.report
                                                    .state
                                                    .gpu_fan
                                                    .is_none_or(|f| f.curve != g)
                                            });
                                    if needs_restore {
                                        self.pending_fan_restore_at =
                                            Some(now + Duration::from_millis(300));
                                    }
                                }
                            }
                        }
                    }
                }
                self.publish();
                let result = result.and(if same_epoch {
                    Ok(())
                } else {
                    Err(ErrorCode::ConflictDetected)
                });
                self.control_reply(source, result);
                if !matches!(source, Origin::InternalRestore) {
                    self.engine_event(AutomationEvent::ControlSettled { target, result });
                    if result.is_ok() {
                        if source.automatic() {
                            self.notify(OsdKind::AutomationApplied);
                        } else if let Some(kind) = osd_kind(target) {
                            self.notify(kind);
                        }
                    }
                }
            }
            (
                Inflight::ResetFanMode { source },
                Completion::Applied(Applied {
                    result,
                    observation,
                    ..
                }),
            ) => {
                if result == Err(ErrorCode::TimedOut) {
                    self.store.diagnostic(
                        "fan mode reset transaction timed out; entering fail-closed quarantine",
                    );
                    self.quarantine = true;
                }
                self.ec_recovery |= matches!(
                    result,
                    Err(ErrorCode::TimedOut
                        | ErrorCode::RollbackFailed
                        | ErrorCode::RecoveryRequired)
                );
                if same_epoch {
                    self.observation = observation;
                }
                if result.is_ok() && same_epoch {
                    if let Some(mode) = self
                        .observation
                        .as_ref()
                        .and_then(|obs| obs.report.state.performance_mode)
                    {
                        if self.fan_profiles.disable_custom(mode) && !self.readonly {
                            if let Err(error) = self.store.save_fan_profiles(&self.fan_profiles) {
                                self.store.diagnostic(&format!(
                                    "could not save fan-profiles.json: {error}"
                                ));
                            }
                        }
                    }
                }
                self.publish();
                let result = result.and(if same_epoch {
                    Ok(())
                } else {
                    Err(ErrorCode::ConflictDetected)
                });
                self.control_reply(source, result);
            }
            (Inflight::Control { source, .. }, Completion::Skipped(error)) => {
                if matches!(source, Origin::LockLighting) {
                    self.release_lock_lighting();
                }
                self.control_reply(source, Err(error))
            }
            (Inflight::ResetFanMode { source }, Completion::Skipped(error)) => {
                self.control_reply(source, Err(error))
            }
            (Inflight::Inspect, Completion::Skipped(_)) => {
                if self.observation.is_none() && self.active() && !self.quarantine {
                    self.arm_inspect_retry(now);
                }
            }
            (Inflight::Sample(_), Completion::Skipped(_)) => {}
            _ => {
                self.store
                    .diagnostic("unexpected HAL completion mismatch; entering quarantine");
                self.quarantine = true;
                self.observation = None;
                self.publish();
            }
        }
    }

    fn watchdog(&mut self, _now: Instant) {
        let Some(flight) = self.flight.take() else {
            return;
        };
        let canceled = flight.ticket.stop_before_write();
        match flight.kind {
            Inflight::Inspect => {
                self.store.diagnostic(
                    "watchdog: hardware inspect exceeded budget; awaiting worker before retry",
                );
                self.late_read = Some(LateRead {
                    epoch: flight.epoch,
                    inspect: true,
                });
                self.initialized = true;
                self.observation = None;
                self.publish();
                return;
            }
            Inflight::Sample(_) => {
                self.store.diagnostic(
                    "watchdog: telemetry sample exceeded budget; awaiting worker",
                );
                self.late_read = Some(LateRead {
                    epoch: flight.epoch,
                    inspect: false,
                });
                self.sample_timeouts = self.sample_timeouts.saturating_add(1);
                if self.sample_timeouts >= MAX_CONSECUTIVE_SAMPLE_TIMEOUTS {
                    self.sample_timeouts = 0;
                    self.inspect_requested = true;
                }
                return;
            }
            Inflight::System { source, .. } => {
                self.store.diagnostic(
                    "watchdog: system tools transaction exceeded budget; entering quarantine",
                );
                self.quarantine = true;
                self.system_reply(
                    source,
                    Err(if canceled {
                        ErrorCode::Busy
                    } else {
                        ErrorCode::TimedOut
                    }),
                );
            }
            Inflight::Control { change, source, .. } => {
                self.store.diagnostic(
                    "watchdog: control transaction exceeded budget; entering quarantine",
                );
                self.quarantine = true;
                self.ec_recovery |= hal::is_ec(change.target()) && !canceled;
                let error = if canceled {
                    ErrorCode::Busy
                } else {
                    ErrorCode::TimedOut
                };
                self.control_reply(source, Err(error));
                if !matches!(source, Origin::InternalRestore) {
                    self.engine_event(AutomationEvent::ControlSettled {
                        target: change.target(),
                        result: Err(error),
                    });
                }
            }
            Inflight::ResetFanMode { source } => {
                self.store.diagnostic(
                    "watchdog: fan mode reset transaction exceeded budget; entering quarantine",
                );
                self.quarantine = true;
                self.ec_recovery |= !canceled;
                let error = if canceled {
                    ErrorCode::Busy
                } else {
                    ErrorCode::TimedOut
                };
                self.control_reply(source, Err(error));
            }
        }
        self.observation = None;
        self.publish();
    }

    fn control_reply(&mut self, source: Origin, result: Result<(), ErrorCode>) {
        if let Origin::Pipe { peer, id } = source {
            self.pipe.reply(
                peer,
                id,
                result.map(|()| IpcReply::Applied {
                    snapshot: (*self.snapshot).clone(),
                }),
                false,
            );
        } else if matches!(source, Origin::Tray) {
            if let Err(error) = result {
                eprintln!("msilt-daemon: tray control: {error:?}");
            }
        }
    }

    fn publish(&mut self) {
        let old = &self.snapshot;
        let mut snapshot = SystemSnapshot {
            revision: old.revision,
            manual_overrides: self.leases.manual_targets().collect(),
            custom_fan_modes: self.fan_profiles.custom_modes(),
            ..Default::default()
        };
        if let Some(observed) = &self.observation {
            snapshot.hardware = observed.report.state.clone();
            snapshot.capabilities = observed.report.readable;
            snapshot.writable = if self.active() && !self.quarantine {
                observed.writable
            } else {
                CapabilityFlags::NONE
            };
            snapshot.ec_write_status = observed.ec_status;
        }
        if self.ec_recovery {
            snapshot.ec_write_status = EcWriteStatus::RecoveryRequired;
            snapshot.writable = hal::ec_off(snapshot.writable);
        }
        snapshot.status = if self.quarantine
            || self.suspended
            || self
                .observation
                .as_ref()
                .is_some_and(|observed| observed.report.readable == CapabilityFlags::NONE)
        {
            DaemonStatus::Degraded
        } else if self.observation.is_some() {
            DaemonStatus::Ready
        } else if !self.initialized {
            DaemonStatus::Starting
        } else {
            DaemonStatus::Degraded
        };
        if self.observation.is_none() && self.quarantine {
            snapshot.writable = CapabilityFlags::NONE;
        }
        snapshot.hardware.power_source =
            self.power_source.unwrap_or(snapshot.hardware.power_source);
        snapshot.hardware.battery_percent =
            self.battery.unwrap_or(snapshot.hardware.battery_percent);
        if self.config.osd.enabled {
            snapshot.capabilities |= CapabilityFlags::OSD;
        }
        if self.config.automation.enabled {
            snapshot.capabilities |= CapabilityFlags::AUTOMATION;
        }
        if &snapshot != old.as_ref() {
            snapshot.revision = snapshot.revision.saturating_add(1);
            self.snapshot = Arc::new(snapshot);
            self.pipe.send_state(&self.snapshot);
            self.engine_event(AutomationEvent::StateChanged {
                revision: self.snapshot.revision,
            });
        }
    }

    fn process_window(&mut self, now: Instant) {
        let events = self.inbox.take();
        let was_active = self.unlocked && !self.suspended;
        if let Some(suspended) = events.suspended {
            if suspended != self.suspended {
                self.suspended = suspended;
                self.epoch = self.epoch.wrapping_add(1);
                self.observation = None;
                self.inspect_requested = !suspended;
            }
        }
        if let Some(unlocked) = events.unlocked {
            if self.unlocked && !unlocked {
                self.lock_lighting_pending = true;
            }
            if unlocked {
                self.release_lock_lighting();
            }
            self.unlocked = unlocked;
        }
        let available = self.unlocked && !self.suspended;
        if available != was_active {
            self.engine_event(AutomationEvent::SessionChanged {
                unlocked: available,
            });
            self.processed_engine_deadline = None;
            if available {
                self.epoch = self.epoch.wrapping_add(1);
                self.observation = None;
                self.inspect_requested = true;
                self.inspect_retry_at = None;
                self.inspect_retry_failures = 0;
            } else {
                self.inspect_retry_at = None;
                if let Some(flight) = &self.flight {
                    flight.ticket.stop_before_write();
                }
            }
            self.publish();
        }
        if let Some(on) = events.display_on {
            self.display_on = on;
        }
        if !self.active() || !self.display_on {
            self.ui(UiEvent::HideOsd);
        }
        if events.flags & BITS_TRAY_RESTART != 0 {
            self.ui(UiEvent::RestoreTray);
        }
        if events.flags & BITS_CLOCK != 0 {
            self.engine_event(AutomationEvent::ClockChanged);
            self.processed_engine_deadline = None;
        }
        if events.flags & BITS_DISPLAY != 0 {
            self.topology_due = Some(now + Duration::from_millis(400));
        }
        let mut power = SYSTEM_POWER_STATUS::default();
        let status_read = events.flags & BITS_POWER_STATUS != 0
            && unsafe { GetSystemPowerStatus(&mut power) }.is_ok();
        let source = events.source.or_else(|| {
            status_read.then_some(match power.ACLineStatus {
                0 => Some(PowerSource::Battery),
                1 => Some(PowerSource::Ac),
                _ => None,
            })
        });
        let battery = events.battery.or_else(|| {
            status_read
                .then(|| (power.BatteryLifePercent <= 100).then_some(power.BatteryLifePercent))
        });
        let saver = events
            .saver
            .or_else(|| status_read.then_some(power.SystemStatusFlag != 0));
        self.update_power(source, battery, saver);
        if let Some(mode) = events
            .effective
            .filter(|mode| Some(*mode) != self.effective)
        {
            self.effective = Some(mode);
            self.engine_event(AutomationEvent::EffectivePowerModeChanged { mode });
        }
        for _ in 0..16 {
            let Ok(event) = self.window_actions.try_recv() else {
                break;
            };
            match event {
                UserEvent::Menu { x, y } => {
                    if self.snapshot.status == DaemonStatus::Starting {
                        self.inspect_requested = true;
                    }
                    self.ui(UiEvent::Menu {
                        x,
                        y,
                        snapshot: self.snapshot.clone(),
                    });
                }
                UserEvent::OpenUi if self.active() => {
                    self.qos.set_interactive(true);
                    self.boost_until = Some(now + Duration::from_secs(1));
                    self.ui(UiEvent::OpenUi);
                }
                UserEvent::OpenUi => {}
                UserEvent::Control { expected, change } => self.enqueue(Job {
                    source: Origin::Tray,
                    expected,
                    until: now + Duration::from_millis(u64::from(DEFAULT_QUEUE_TTL_MS)),
                    command: Command::Change(change),
                }),
            }
        }
    }

    fn update_power(
        &mut self,
        source: Option<Option<PowerSource>>,
        battery: Option<Option<u8>>,
        saver: Option<bool>,
    ) {
        let changed = source.is_some_and(|new| Some(new) != self.power_source)
            || battery.is_some_and(|new| Some(new) != self.battery);
        if let Some(source) = source.filter(|new| Some(*new) != self.power_source) {
            self.power_source = Some(source);
            self.engine_event(AutomationEvent::PowerSourceChanged { source });
        }
        if let Some(percent) = battery.filter(|new| Some(*new) != self.battery) {
            self.battery = Some(percent);
            self.engine_event(AutomationEvent::BatteryPercentChanged { percent });
        }
        if let Some(enabled) = saver.filter(|&new| Some(new) != self.saver) {
            self.saver = Some(enabled);
            self.engine_event(AutomationEvent::PowerSavingChanged { enabled });
        }
        if changed {
            self.publish();
        }
    }

    fn update_topology(&mut self, count: u16) {
        if self.external_count.replace(count) != Some(count) {
            self.engine_event(AutomationEvent::DisplayTopologyChanged {
                external_display_count: count,
            });
        }
    }

    fn sync_telemetry(&mut self, now: Instant) {
        let active = self.active() && !self.quarantine && self.pipe.wants_telemetry();
        if self.telemetry.set(active, now) && !active {
            self.pipe.clear_samples();
            if let Some(Flight {
                kind: Inflight::Sample(_),
                ticket,
                ..
            }) = &self.flight
            {
                ticket.cancel();
            }
        }
        let discrete = self.snapshot.hardware.gpu_mux.is_some_and(|mux| mux.current == GpuMuxMode::Discrete);
        let want_gpu_telemetry = active && (discrete || self.pipe.wants_gpu_telemetry());
        if self.gpu_telemetry_enabled != want_gpu_telemetry {
            self.gpu_telemetry_enabled = want_gpu_telemetry;
            self.telemetry.generation = self.telemetry.generation.wrapping_add(1);
            self.pipe.clear_samples();
            self.executor.set_gpu_telemetry(want_gpu_telemetry);
            if want_gpu_telemetry && self.snapshot.hardware.gpu_clock_offset.is_none() {
                self.inspect_requested = true;
            }
        }
        #[cfg(test)]
        self.diagnostics
            .telemetry_armed
            .store(active, std::sync::atomic::Ordering::Release);
    }

    fn now_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started)
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
    fn stamp(&self, ms: u64) -> Option<Instant> {
        self.started.checked_add(Duration::from_millis(ms))
    }

    fn engine_deadline(&self) -> Option<u64> {
        if !self.config.automation.enabled || !self.active() {
            return None;
        }
        let engine: &dyn AutomationEngine = self
            .alternate_engine
            .as_deref()
            .map_or(&self.engine, |engine| engine);
        engine
            .next_deadline_monotonic_ms()
            .filter(|due| Some(*due) != self.processed_engine_deadline)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let worker_idle = self.flight.is_none() && self.late_read.is_none();
        [
            self.pipe.next_deadline(),
            self.telemetry.next,
            self.inspect_retry_at,
            self.pending_fan_restore_at,
            self.topology_due,
            self.boost_until,
            self.qos.trim_deadline(),
            self.flight.as_ref().map(|flight| flight.by),
            self.engine_deadline().and_then(|ms| self.stamp(ms)),
            self.leases
                .next_deadline(worker_idle)
                .and_then(|ms| self.stamp(ms)),
        ]
        .into_iter()
        .flatten()
        .chain(self.queue.iter().map(|job| job.until))
        .chain(self.greetings.iter().map(|greeting| greeting.until))
        .min()
    }

    fn process_deadlines(&mut self, now: Instant) {
        let ms = self.now_ms(now);
        let worker_idle = self.flight.is_none() && self.late_read.is_none();
        if self.inspect_retry_at.is_some_and(|until| now >= until) {
            self.inspect_retry_at = None;
            if self.observation.is_none() && self.active() && !self.quarantine {
                self.inspect_requested = true;
            }
        }
        if self.topology_due.is_some_and(|until| now >= until) {
            self.topology_due = None;
            self.topology_requested = true;
            self.inspect_requested = true;
        }
        if let Some(due) = self.engine_deadline().filter(|&due| due <= ms) {
            self.processed_engine_deadline = Some(due);
            self.engine_event(AutomationEvent::TimerElapsed);
        }
        if self
            .leases
            .next_deadline(worker_idle)
            .is_some_and(|due| due <= ms)
        {
            self.leases.expire(ms, worker_idle);
            if !self
                .leases
                .manual_targets()
                .eq(self.snapshot.manual_overrides.iter().copied())
            {
                self.publish();
            }
        }
    }

    fn seed_automation(&mut self) {
        self.engine_event(AutomationEvent::RulesReloaded);
        self.engine_event(AutomationEvent::SessionChanged {
            unlocked: self.active(),
        });
        self.engine_event(AutomationEvent::PowerSourceChanged {
            source: self.snapshot.hardware.power_source,
        });
        self.engine_event(AutomationEvent::BatteryPercentChanged {
            percent: self.snapshot.hardware.battery_percent,
        });
        if let Some(enabled) = self.saver {
            self.engine_event(AutomationEvent::PowerSavingChanged { enabled });
        }
        if let Some(mode) = self.effective {
            self.engine_event(AutomationEvent::EffectivePowerModeChanged { mode });
        }
        self.inspect_requested = true;
        self.topology_requested = true;
    }

    fn engine_event(&mut self, event: AutomationEvent) {
        if self.config.automation.enabled && self.engine_events.len() < MAX_ENGINE_EVENTS {
            self.engine_events.push_back(event);
        }
    }

    fn run_engine(&mut self, now: Instant) {
        let ms = self.now_ms(now);
        while let Some(event) = self.engine_events.pop_front() {
            if !self.config.automation.enabled
                || !self.active() && !matches!(event, AutomationEvent::SessionChanged { .. })
            {
                continue;
            }
            self.actions.clear();
            let engine: &mut dyn AutomationEngine = match &mut self.alternate_engine {
                Some(engine) => engine.as_mut(),
                None => &mut self.engine,
            };
            engine.on_event(
                &event,
                AutomationContext {
                    snapshot: &self.snapshot,
                    monotonic_ms: ms,
                    unix_ms: unix_ms(),
                },
                &mut self.actions,
            );
            self.actions.truncate(MAX_ACTIONS);
            let mut actions = std::mem::take(&mut self.actions);
            for action in actions.drain(..) {
                match action {
                    AutomationAction::Acquire(lease)
                        if self.active() && self.validate(&lease.change).is_ok() =>
                    {
                        self.leases.acquire(lease, ms);
                    }
                    AutomationAction::Release { target, owner } => {
                        self.leases.release(target, owner)
                    }
                    AutomationAction::Notify(kind) => self.notify(kind),
                    _ => {}
                }
            }
            self.actions = actions;
        }
    }

    fn notify(&mut self, kind: OsdKind) {
        if !self.config.osd.enabled || !self.active() || !self.display_on {
            return;
        }
        self.pipe.send_osd(self.snapshot.revision, kind);
        let hw = &self.snapshot.hardware;
        let text = match kind {
            OsdKind::PerformanceMode => hw
                .performance_mode
                .map(|mode| ("性能模式", mode_name(mode).into())),
            OsdKind::CoolerBoost => hw
                .cooler_boost
                .map(|on| ("强冷散热", if on { "已开启" } else { "已关闭" }.into())),
            OsdKind::DisplayRefreshRate => hw
                .display_refresh_hz
                .map(|hz| ("屏幕刷新率", format!("{hz} Hz"))),
            OsdKind::AutomationApplied => Some(("MSI-Toolkit", "已应用自动化规则".into())),
        };
        if let Some((title, detail)) = text {
            self.ui(UiEvent::ShowOsd {
                title,
                detail,
                duration_ms: self.config.osd.duration_ms,
                done: None,
            });
        }
    }

    fn ui(&self, event: UiEvent) {
        if let Some(desktop) = &self.desktop {
            let _ = desktop.send(event);
        }
    }

    fn update_qos(&mut self, now: Instant) {
        if self.boost_until.is_some_and(|until| now >= until) {
            self.boost_until = None;
        }
        self.qos.set_interactive(
            self.boost_until.is_some()
                || self.flight.as_ref().is_some_and(|flight| {
                    matches!(
                        flight.kind,
                        Inflight::Control {
                            source: Origin::Pipe { .. } | Origin::Tray,
                            ..
                        } | Inflight::ResetFanMode {
                            source: Origin::Pipe { .. } | Origin::Tray,
                        }
                    )
                }),
        );
        // A watchdog can retire a flight while the native worker is still unwinding.
        self.qos.update_idle(
            !self.quarantine
                && self.flight.is_none()
                && self.late_read.is_none()
                && self.inspect_retry_at.is_none()
                && self.pending_fan_restore_at.is_none()
                && self.queue.is_empty()
                && self.greetings.is_empty()
                && self.topology_due.is_none()
                && !self.pipe.has_clients(),
            now,
        );
    }
}

fn osd_kind(target: ControlTarget) -> Option<OsdKind> {
    match target {
        ControlTarget::PerformanceMode {} => Some(OsdKind::PerformanceMode),
        ControlTarget::CoolerBoost {} => Some(OsdKind::CoolerBoost),
        ControlTarget::DisplayRefreshRate {} => Some(OsdKind::DisplayRefreshRate),
        _ => None,
    }
}
