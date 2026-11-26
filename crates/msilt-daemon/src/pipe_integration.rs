//! Kernel named-pipe tests with isolated names, profiles and in-process peer authorization.
use std::{
    collections::VecDeque,
    fs, io,
    os::windows::io::OwnedHandle,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering::*},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use msilt_core::*;
use windows::{
    Win32::{
        Foundation::{ERROR_IO_PENDING, ERROR_PIPE_BUSY},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::*,
        System::{
            IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED},
            Pipes::WaitNamedPipeW,
            Threading::CreateEventW,
        },
    },
    core::PCWSTR,
};

use crate::{
    DaemonBackends, HalBackends,
    config_store::ConfigStore,
    coordinator::{self, Diagnostics, Host},
    events::Inbox,
    hal_executor::unix_ms,
    iocp::Port,
    local_pipe_name,
    security::{CLIENT_ACCESS, Security},
    win::{is_error, os_error, own, raw, wide},
};

static NEXT: AtomicU32 = AtomicU32::new(0);
const REFRESH: CapabilityFlags = CapabilityFlags::DISPLAY_REFRESH_RATE;
const TARGET: ControlTarget = ControlTarget::DisplayRefreshRate {};
const WAIT: Duration = Duration::from_millis(800);

#[derive(Default)]
struct Counts {
    calls: AtomicU32,
    samples: AtomicU32,
    keypad_writes: AtomicU32,
    inspect_gate: Mutex<Option<Arc<Gate>>>,
}

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    open: Mutex<bool>,
    ready: Condvar,
}

impl Gate {
    fn block(&self) -> HalResult<()> {
        self.entered.store(true, Release);
        let (open, _) = self
            .ready
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(3), |open| {
                !*open
            })
            .unwrap();
        if *open {
            Ok(())
        } else {
            Err(HalError::TimedOut)
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.ready.notify_all();
    }
}

struct DryRun {
    refresh: u16,
    zones: [Rgb8; KEYBOARD_ZONE_COUNT],
    counts: Arc<Counts>,
    write_gate: Option<Arc<Gate>>,
    sample_gate: Option<Arc<Gate>>,
}

impl HardwareRead for DryRun {
    fn inspect(&mut self) -> HalResult<HardwareReport> {
        let gate = self.counts.inspect_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.block()?;
        }
        Ok(HardwareReport {
            readable: REFRESH | CapabilityFlags::TELEMETRY | CapabilityFlags::KEYBOARD_LIGHTING,
            controls_detected: REFRESH | CapabilityFlags::KEYBOARD_LIGHTING,
            state: HardwareState {
                display_refresh_hz: Some(self.refresh),
                supported_refresh_rates: vec![60, 120, 240],
                keyboard_zones: Some(self.zones),
                ..Default::default()
            },
        })
    }
    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
        self.counts.samples.fetch_add(1, AcqRel);
        if let Some(gate) = self.sample_gate.take() {
            gate.block()?;
        }
        Ok(TelemetrySample {
            sampled_at_unix_ms: unix_ms(),
            cpu_temperature_c: Some(48),
            ..Default::default()
        })
    }
}

impl EcRegisterIo for DryRun {
    fn read_byte(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn write_byte(&mut self, _: &AuthorizedEcWrite) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
}

impl PlatformControl for DryRun {
    fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
        if let Some(gate) = self.write_gate.take() {
            gate.block()?;
        }
        self.counts.calls.fetch_add(1, AcqRel);
        self.refresh = hz;
        Ok(self.refresh)
    }
    fn set_display_brightness(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn set_color_profile(&mut self, _: &str) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
    fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
        self.zones = *zones;
        self.counts.keypad_writes.fetch_add(1, AcqRel);
        Ok(())
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let name = format!(
            "msilt-daemon-{label}-{}-{}-{}",
            std::process::id(),
            unix_ms(),
            NEXT.fetch_add(1, Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        for entry in fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

struct Fixture {
    name: String,
    files: TestDir,
    stats: Arc<Diagnostics>,
    counts: Arc<Counts>,
    inbox: Arc<Inbox>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Fixture {
    fn start(write_gate: Option<Arc<Gate>>, sample_gate: Option<Arc<Gate>>) -> Self {
        Self::start_with_engine(
            write_gate,
            sample_gate,
            DaemonBackends::default().automation,
        )
    }

    fn start_with_engine(
        write_gate: Option<Arc<Gate>>,
        sample_gate: Option<Arc<Gate>>,
        engine: Box<dyn AutomationEngine + Send>,
    ) -> Self {
        let files = TestDir::new("pipe");
        let name = local_pipe_name(files.0.file_name().unwrap().to_str().unwrap()).unwrap();
        let port = Port::new().unwrap();
        let (inbox, window_actions) = Inbox::new(&port);
        let stats = Arc::new(Diagnostics::default());
        let counts = Arc::new(Counts::default());
        let backends = DaemonBackends {
            hal: {
                let counts = counts.clone();
                Box::new(move || {
                    HalBackends::new(
                        DryRun {
                            refresh: 60,
                            zones: [Rgb8 {
                                r: 208,
                                g: 32,
                                b: 48,
                            }; KEYBOARD_ZONE_COUNT],
                            counts,
                            write_gate,
                            sample_gate,
                        },
                        REFRESH | CapabilityFlags::KEYBOARD_LIGHTING,
                    )
                })
            },
            automation: engine,
        };
        let host = Host {
            port,
            name: name.clone(),
            security: Security::isolated().unwrap(),
            store: ConfigStore::isolated(files.0.clone()),
            backends,
            inbox: inbox.clone(),
            window_actions,
            desktop: None,
            process_qos: false,
            readonly: false,
            load_stored_rules: false,
            diagnostics: stats.clone(),
        };
        let handle = thread::spawn(move || coordinator::run(host));
        wait_for(Duration::from_secs(2), || {
            stats.listening.load(Acquire) || handle.is_finished()
        });
        assert!(
            stats.listening.load(Acquire),
            "coordinator startup: {:?}",
            handle.join().unwrap()
        );
        Self {
            name,
            files,
            stats,
            counts,
            inbox,
            thread: Some(handle),
        }
    }

    fn client(&self) -> Client {
        Client::open(&self.name, SECURITY_IDENTIFICATION)
    }
    fn armed(&self) -> bool {
        self.stats.telemetry_armed.load(Acquire)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.inbox.stop();
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap().unwrap();
        }
    }
}

fn wait_for(limit: Duration, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < end, "condition not met within {limit:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

struct Client {
    pipe: OwnedHandle,
    next_id: u64,
    events: VecDeque<IpcEvent>,
}

impl Client {
    fn open(name: &str, sqos: FILE_FLAGS_AND_ATTRIBUTES) -> Self {
        let path = wide(name);
        let name = PCWSTR(path.as_ptr());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match unsafe {
                CreateFileW(
                    name,
                    CLIENT_ACCESS,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | sqos,
                    None,
                )
            } {
                Ok(handle) => {
                    return Self {
                        pipe: unsafe { own(handle) },
                        next_id: 0,
                        events: VecDeque::new(),
                    };
                }
                Err(error) if is_error(&error, ERROR_PIPE_BUSY) && Instant::now() < deadline => unsafe {
                    let _ = WaitNamedPipeW(name, 50);
                },
                Err(error) => panic!("opening test pipe: {error}"),
            }
        }
    }

    fn overlapped(
        &self,
        timeout: u32,
        operation: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        let event = unsafe {
            own(CreateEventW(
                None::<*const SECURITY_ATTRIBUTES>,
                true,
                false,
                PCWSTR::null(),
            )
            .map_err(os_error)?)
        };
        let mut ov = OVERLAPPED {
            hEvent: raw(&event),
            ..Default::default()
        };
        if let Err(error) = operation(&mut ov) {
            if !is_error(&error, ERROR_IO_PENDING) {
                return Err(os_error(error));
            }
        }
        let mut transferred = 0;
        if let Err(error) =
            unsafe { GetOverlappedResultEx(raw(&self.pipe), &ov, &mut transferred, timeout, false) }
        {
            unsafe {
                let _ = CancelIoEx(raw(&self.pipe), Some(&ov));
                let _ = GetOverlappedResult(raw(&self.pipe), &ov, &mut transferred, true);
            }
            return Err(os_error(error));
        }
        Ok(transferred as usize)
    }

    fn write(&self, bytes: &[u8]) {
        let mut left = bytes;
        while !left.is_empty() {
            let count = self
                .overlapped(2500, |ov| unsafe {
                    WriteFile(raw(&self.pipe), Some(left), None, Some(ov))
                })
                .unwrap();
            assert!(count > 0);
            left = &left[count..];
        }
    }

    fn read_exact(&self, mut bytes: &mut [u8], timeout: u32) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.overlapped(timeout, |ov| unsafe {
                ReadFile(raw(&self.pipe), Some(bytes), None, Some(ov))
            })?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            bytes = &mut bytes[count..];
        }
        Ok(())
    }

    fn receive(&self, timeout: u32) -> io::Result<IpcMessage> {
        let mut header = [0; 4];
        self.read_exact(&mut header, timeout)?;
        let len = u32::from_le_bytes(header) as usize;
        assert!((1..=MAX_FRAME_PAYLOAD).contains(&len));
        let mut data = vec![0; len + FRAME_HEADER_LEN];
        data[..4].copy_from_slice(&header);
        self.read_exact(&mut data[4..], timeout)?;
        Ok(decode_frame(&data).unwrap().unwrap().0)
    }

    fn send(&mut self, command: IpcCommand, ttl_ms: Option<u16>, split: bool) -> u64 {
        self.next_id += 1;
        let frame = request_bytes(self.next_id, command, ttl_ms);
        if split {
            self.write(&frame[..2]);
            thread::sleep(Duration::from_millis(30));
            self.write(&frame[2..]);
        } else {
            self.write(&frame);
        }
        self.next_id
    }

    fn reply(&mut self, id: u64) -> Result<IpcReply, ErrorCode> {
        for _ in 0..64 {
            match self.receive(2500).unwrap() {
                IpcMessage::Response(reply) => {
                    assert_eq!(reply.id, id);
                    return match reply.result {
                        IpcResult::Ok { reply } => Ok(reply),
                        IpcResult::Error { error } => Err(error.code),
                    };
                }
                IpcMessage::Event(event) => self.events.push_back(event),
                IpcMessage::Request(_) => panic!("server sent a request"),
            }
        }
        panic!("response was starved by pushes");
    }

    fn call(&mut self, command: IpcCommand) -> Result<IpcReply, ErrorCode> {
        let id = self.send(command, None, false);
        self.reply(id)
    }

    fn hello(&mut self, client: ClientKind) -> SystemSnapshot {
        let id = self.send(
            IpcCommand::Hello {
                version: IPC_VERSION,
                client,
            },
            None,
            true,
        );
        let IpcReply::Hello {
            version: IPC_VERSION,
            snapshot,
        } = self.reply(id).unwrap()
        else {
            panic!("invalid hello");
        };
        snapshot
    }

    fn event(&mut self, matches: impl Fn(&IpcEvent) -> bool) -> IpcEvent {
        if let Some(index) = self.events.iter().position(&matches) {
            return self.events.remove(index).unwrap();
        }
        for _ in 0..32 {
            let IpcMessage::Event(event) = self.receive(2500).unwrap() else {
                panic!("expected a push");
            };
            if matches(&event) {
                return event;
            }
            self.events.push_back(event);
        }
        panic!("no matching push");
    }
}

fn request_bytes(id: u64, command: IpcCommand, ttl_ms: Option<u16>) -> Vec<u8> {
    encode_frame(&IpcMessage::Request(IpcRequest {
        id,
        command,
        ttl_ms,
    }))
    .unwrap()
}

fn set_hz(expected_revision: u64, hz: u16) -> IpcCommand {
    IpcCommand::Control {
        expected_revision,
        change: ControlChange::SetDisplayRefreshRate { hz },
    }
}

fn subscribe(client: &mut Client, topics: Subscriptions) -> SystemSnapshot {
    let IpcReply::Subscriptions {
        topics: actual,
        snapshot,
    } = client
        .call(IpcCommand::SetSubscriptions { topics })
        .unwrap()
    else {
        panic!("invalid subscription reply");
    };
    assert_eq!(actual, topics);
    snapshot
}

#[test]
fn automation_crud_and_dry_run_use_the_current_pipe_snapshot() {
    let host = Fixture::start(None, None);
    let mut client = host.client();
    client.hello(ClientKind::Ui);
    host.inbox.power_source(0);
    let IpcReply::AutomationRules { rules, .. } = client
        .call(IpcCommand::AddAutomationPreset {
            preset: AutomationPreset::AcBalanced240Hz,
        })
        .unwrap()
    else {
        panic!("missing rules reply");
    };
    let id = rules[0].id.clone();
    assert!(rules[0].enabled);
    let IpcReply::AutomationRules { rules, summary } =
        client.call(IpcCommand::DryRunAutomation {}).unwrap()
    else {
        panic!("missing simulated rules reply");
    };
    assert!(summary.contains("1/1"), "{summary}");
    assert!(rules[0].diagnostic.contains("不可写"));
    client
        .call(IpcCommand::SetAutomationRuleEnabled {
            id: id.clone(),
            enabled: false,
        })
        .unwrap();
    let IpcReply::AutomationRules { rules, .. } =
        client.call(IpcCommand::GetAutomationRules {}).unwrap()
    else {
        panic!("missing rules reply");
    };
    assert!(!rules[0].enabled);
    let IpcReply::AutomationRules { rules, .. } = client
        .call(IpcCommand::DeleteAutomationRule { id })
        .unwrap()
    else {
        panic!("missing rules reply");
    };
    assert!(rules.is_empty());
    assert_eq!(host.counts.calls.load(Acquire), 0);
}

#[test]
fn lock_screen_preset_dims_the_mock_keyboard_and_restores_it_on_unlock() {
    let host = Fixture::start(None, None);
    let mut ui = host.client();
    let original = ui.hello(ClientKind::Ui);
    ui.call(IpcCommand::AddAutomationPreset {
        preset: AutomationPreset::LockScreenDarkKeyboard,
    })
    .unwrap();
    let mut config = AppConfig::default();
    config.automation.enabled = true;
    ui.call(IpcCommand::SetConfig {
        expected_revision: 0,
        config,
    })
    .unwrap();
    subscribe(
        &mut ui,
        Subscriptions {
            state: true,
            ..Default::default()
        },
    );
    host.inbox.session(false);
    let IpcEvent::StateChanged { snapshot } = ui.event(|event| {
        matches!(event, IpcEvent::StateChanged { snapshot } if snapshot.writable == CapabilityFlags::NONE && snapshot.hardware.keyboard_zones.is_some_and(|zones| zones.iter().all(|rgb| rgb.r == 0 && rgb.g == 0 && rgb.b == 0)))
    }) else { panic!("missing dark keyboard snapshot"); };
    assert_eq!(
        ui.call(set_hz(snapshot.revision, 240)),
        Err(ErrorCode::NotReady)
    );
    wait_for(Duration::from_secs(2), || {
        host.counts.keypad_writes.load(Acquire) == 1
    });
    host.inbox.session(true);
    wait_for(Duration::from_secs(2), || {
        host.counts.keypad_writes.load(Acquire) == 2
    });
    let IpcEvent::StateChanged { snapshot: restored } = ui.event(|event| {
        matches!(event, IpcEvent::StateChanged { snapshot } if snapshot.hardware.keyboard_zones == original.hardware.keyboard_zones && snapshot.revision > original.revision && snapshot.writable.contains(CapabilityFlags::KEYBOARD_LIGHTING))
    }) else { panic!("missing restored keyboard snapshot"); };
    assert_eq!(
        restored.hardware.keyboard_zones,
        original.hardware.keyboard_zones
    );
    assert_eq!(host.counts.calls.load(Acquire), 0);
}

#[test]
fn real_pipe_v1_round_trip_and_last_subscriber_stops_timer() {
    let host = Fixture::start(None, None);
    let mut ui = host.client();
    let mut second = host.client();
    let mut cli = host.client();
    let initial = ui.hello(ClientKind::Ui);
    assert_eq!(initial.status, DaemonStatus::Ready);
    assert_eq!(initial.hardware.display_refresh_hz, Some(60));
    assert_eq!(second.hello(ClientKind::Ui), initial);
    assert_eq!(cli.hello(ClientKind::Cli), initial);
    let state_only = Subscriptions {
        state: true,
        osd: true,
        telemetry: false,
    };
    let with_telemetry = Subscriptions {
        telemetry: true,
        ..state_only
    };
    assert_eq!(
        cli.call(IpcCommand::SetSubscriptions {
            topics: with_telemetry
        }),
        Err(ErrorCode::Unsupported)
    );
    assert_eq!(subscribe(&mut ui, state_only), initial);
    assert!(!host.armed());
    assert_eq!(subscribe(&mut ui, with_telemetry), initial);
    assert_eq!(subscribe(&mut second, with_telemetry), initial);
    wait_for(WAIT, || host.armed());

    assert_eq!(
        ui.call(set_hz(initial.revision + 1, 120)),
        Err(ErrorCode::RevisionConflict)
    );
    assert_eq!(host.counts.calls.load(Acquire), 0);
    let IpcReply::Applied { snapshot: applied } = ui.call(set_hz(initial.revision, 120)).unwrap()
    else {
        panic!("expected applied");
    };
    assert_eq!(applied.revision, initial.revision + 1);
    assert_eq!(applied.hardware.display_refresh_hz, Some(120));
    assert_eq!(applied.manual_overrides, vec![TARGET]);
    assert_eq!(
        ui.event(|event| matches!(event, IpcEvent::StateChanged { .. })),
        IpcEvent::StateChanged {
            snapshot: applied.clone()
        }
    );
    assert_eq!(
        ui.event(|event| matches!(event, IpcEvent::OsdNotify { .. })),
        IpcEvent::OsdNotify {
            revision: applied.revision,
            kind: OsdKind::DisplayRefreshRate
        }
    );
    let IpcReply::Applied { snapshot: restored } = ui
        .call(IpcCommand::ReleaseManualOverride {
            expected_revision: applied.revision,
            target: TARGET,
        })
        .unwrap()
    else {
        panic!("expected release");
    };
    assert_eq!(restored.hardware.display_refresh_hz, Some(60));
    assert!(restored.manual_overrides.is_empty());
    assert_eq!(restored.revision, applied.revision + 1);

    let IpcReply::Config { config, revision } = cli.call(IpcCommand::GetConfig {}).unwrap() else {
        panic!("expected config");
    };
    assert_eq!(revision, 0);
    let changed = AppConfig {
        theme: Theme::Dark,
        ..config
    };
    let config_path = host.files.0.join("app.json");
    assert_eq!(
        cli.call(IpcCommand::SetConfig {
            expected_revision: 1,
            config: changed
        }),
        Err(ErrorCode::RevisionConflict)
    );
    assert!(!config_path.exists());
    assert_eq!(
        cli.call(IpcCommand::SetConfig {
            expected_revision: 0,
            config: changed
        }),
        Ok(IpcReply::Config {
            config: changed,
            revision: 1
        })
    );
    assert_eq!(
        AppConfig::parse_json(&fs::read(&config_path).unwrap()).unwrap(),
        changed
    );
    assert_eq!(
        cli.call(IpcCommand::SetConfig {
            expected_revision: 0,
            config
        }),
        Err(ErrorCode::RevisionConflict)
    );
    assert_eq!(
        cli.call(IpcCommand::GetSnapshot {}),
        Ok(IpcReply::Snapshot {
            snapshot: restored.clone()
        })
    );
    assert_eq!(fs::read_dir(&host.files.0).unwrap().count(), 1);

    let tick = |event: &IpcEvent| matches!(event, IpcEvent::TelemetryTick { .. });
    let event = ui.event(tick);
    assert!(
        matches!(event, IpcEvent::TelemetryTick { sample } if sample.cpu_temperature_c == Some(48) && sample.gpu_power_mw.is_none())
    );
    assert_eq!(second.event(tick), event);
    assert_eq!(subscribe(&mut ui, state_only), restored);
    assert!(host.armed());
    drop(second);
    wait_for(WAIT, || !host.armed());
    let count = host.counts.samples.load(Acquire);
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(host.counts.samples.load(Acquire), count);
    subscribe(&mut ui, with_telemetry);
    wait_for(WAIT, || host.armed());
    subscribe(&mut ui, state_only);
    wait_for(WAIT, || !host.armed());
}

#[test]
fn queued_ttl_and_disconnect_never_execute_waiting_controls() {
    let gate = Arc::new(Gate::default());
    let host = Fixture::start(Some(gate.clone()), None);
    let mut first = host.client();
    let mut second = host.client();
    let mut departed = host.client();
    let snapshot = first.hello(ClientKind::Cli);
    second.hello(ClientKind::Cli);
    departed.hello(ClientKind::Cli);

    let first_id = first.send(set_hz(snapshot.revision, 120), None, false);
    wait_for(WAIT, || gate.entered.load(Acquire));
    let second_id = second.send(set_hz(snapshot.revision, 240), Some(60), false);
    assert_eq!(second.reply(second_id), Err(ErrorCode::RequestExpired));
    assert_eq!(host.counts.calls.load(Acquire), 0);
    departed.send(set_hz(snapshot.revision + 1, 240), None, false);
    wait_for(WAIT, || host.stats.queued.load(Acquire) == 1);
    drop(departed);
    wait_for(WAIT, || host.stats.queued.load(Acquire) == 0);
    gate.release();

    let IpcReply::Applied { snapshot } = first.reply(first_id).unwrap() else {
        panic!("first control did not complete");
    };
    assert_eq!(snapshot.hardware.display_refresh_hz, Some(120));
    assert_eq!(
        second.call(IpcCommand::GetSnapshot {}),
        Ok(IpcReply::Snapshot { snapshot })
    );
    assert_eq!(host.counts.calls.load(Acquire), 1);
}

#[test]
fn late_sample_does_not_cross_a_subscription_generation() {
    let gate = Arc::new(Gate::default());
    let host = Fixture::start(None, Some(gate.clone()));
    let mut first = host.client();
    let mut second = host.client();
    first.hello(ClientKind::Ui);
    second.hello(ClientKind::Ui);
    let topics = Subscriptions {
        telemetry: true,
        ..Default::default()
    };
    subscribe(&mut first, topics);
    wait_for(Duration::from_secs(2), || gate.entered.load(Acquire));
    drop(first);
    wait_for(WAIT, || !host.armed());

    subscribe(&mut second, topics);
    gate.release();
    let result = second.receive(250);
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(258),
        "no push before the new generation's first tick"
    );
    second.event(|event| matches!(event, IpcEvent::TelemetryTick { .. }));
    assert_eq!(host.counts.samples.load(Acquire), 2);
}

#[test]
fn rejects_anonymous_wrong_version_and_overlapping_requests() {
    let host = Fixture::start(None, None);
    let hello = IpcCommand::Hello {
        version: IPC_VERSION,
        client: ClientKind::Ui,
    };
    let mut anonymous = Client::open(&host.name, SECURITY_ANONYMOUS);
    assert_eq!(anonymous.call(hello.clone()), Err(ErrorCode::AccessDenied));
    let mut version = host.client();
    assert_eq!(
        version.call(IpcCommand::Hello {
            version: IPC_VERSION + 1,
            client: ClientKind::Ui
        }),
        Err(ErrorCode::IncompatibleVersion)
    );
    let mut bad_id = host.client();
    bad_id.hello(ClientKind::Cli);
    bad_id.write(&request_bytes(1, IpcCommand::GetSnapshot {}, None));
    assert_eq!(bad_id.reply(1), Err(ErrorCode::InvalidRequest));
    let mut early = host.client();
    assert_eq!(
        early.call(IpcCommand::GetSnapshot {}),
        Err(ErrorCode::InvalidRequest)
    );
    let glued = host.client();
    let glued_frames = [
        request_bytes(1, hello, None),
        request_bytes(2, IpcCommand::GetConfig {}, None),
    ]
    .concat();
    glued.write(&glued_frames);
    let broken = |result: io::Result<IpcMessage>| {
        [109, 233].contains(&result.unwrap_err().raw_os_error().unwrap_or_default())
    };
    assert!(broken(glued.receive(1000)));

    let mut good = host.client();
    assert_eq!(
        good.hello(ClientKind::Cli).hardware.display_refresh_hz,
        Some(60)
    );
}

#[test]
fn enforces_connection_capacity_and_closes_a_stalled_half_frame() {
    use crate::pipe_server::MAX_CONNECTIONS;
    let host = Fixture::start(None, None);
    let mut clients: Vec<_> = (0..MAX_CONNECTIONS)
        .map(|_| {
            let mut client = host.client();
            client.hello(ClientKind::Cli);
            client
        })
        .collect();
    let path = wide(&host.name);
    let extra = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            CLIENT_ACCESS,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    };
    assert!(extra.is_err_and(|error| is_error(&error, ERROR_PIPE_BUSY)));

    let stalled = clients.pop().unwrap();
    stalled.write(&[0x40, 0x00]);
    let before = Instant::now();
    let error = stalled.receive(6500).unwrap_err();
    assert!(before.elapsed() >= Duration::from_millis(4800));
    assert!(
        [109, 233].contains(&error.raw_os_error().unwrap_or_default()),
        "{error}"
    );
    let mut replacement = host.client();
    replacement.hello(ClientKind::Cli);
    assert!(matches!(
        clients[0].call(IpcCommand::GetConfig {}),
        Ok(IpcReply::Config { revision: 0, .. })
    ));
}

#[test]
fn config_corruption_is_preserved_and_failed_replace_is_atomic() {
    use std::os::windows::fs::OpenOptionsExt;

    let files = TestDir::new("config");
    let path = files.0.join("app.json");
    let damaged = vec![b'?'; MAX_CONFIG_BYTES + 1];
    fs::write(&path, &damaged).unwrap();
    let mut store = ConfigStore::isolated(files.0.clone());
    let fallback = store.load();
    assert_eq!(fallback, AppConfig::default());
    assert_eq!(fs::read(&path).unwrap(), damaged);
    let light = AppConfig {
        theme: Theme::Light,
        ..fallback
    };
    store.save(&light).unwrap();
    assert_eq!(store.load(), light);
    let archived = fs::read_dir(&files.0)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "corrupt")
        })
        .unwrap();
    assert_eq!(fs::read(archived.path()).unwrap(), damaged);

    let ordinary_reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&path)
        .unwrap();
    assert!(store.save(&fallback).is_err());
    assert_eq!(store.load(), light);
    drop(ordinary_reader);
    assert_eq!(fs::read_dir(&files.0).unwrap().count(), 2);
}

fn state_push(client: &mut Client, matches: impl Fn(&SystemSnapshot) -> bool) -> SystemSnapshot {
    let event = client.event(|event| match event {
        IpcEvent::StateChanged { snapshot } => matches(snapshot),
        _ => false,
    });
    let IpcEvent::StateChanged { snapshot } = event else {
        unreachable!()
    };
    snapshot
}

#[test]
fn session_lock_during_preflight_prevents_write_until_new_inspection() {
    let host = Fixture::start(None, None);
    let mut writer = host.client();
    let mut observer = host.client();
    let snapshot = writer.hello(ClientKind::Cli);
    observer.hello(ClientKind::Ui);
    subscribe(
        &mut observer,
        Subscriptions {
            state: true,
            ..Default::default()
        },
    );
    let gate = Arc::new(Gate::default());
    *host.counts.inspect_gate.lock().unwrap() = Some(gate.clone());
    let id = writer.send(set_hz(snapshot.revision, 120), None, false);
    wait_for(WAIT, || gate.entered.load(Acquire));
    host.inbox.session(false);
    state_push(&mut observer, |s| !s.writable.contains(REFRESH));
    gate.release();
    assert_eq!(writer.reply(id), Err(ErrorCode::NotReady));
    assert_eq!(host.counts.calls.load(Acquire), 0);
    host.inbox.session(true);
    let refreshed = state_push(&mut observer, |s| s.writable.contains(REFRESH));
    assert_eq!(refreshed.hardware.display_refresh_hz, Some(60));
    assert!(matches!(
        writer.call(set_hz(refreshed.revision, 120)),
        Ok(IpcReply::Applied { .. })
    ));
}

struct AcRule;

impl AutomationEngine for AcRule {
    fn on_event(
        &mut self,
        event: &AutomationEvent,
        _: AutomationContext<'_>,
        actions: &mut Vec<AutomationAction>,
    ) {
        let owner = LeaseOwner::Rule(1);
        match event {
            AutomationEvent::PowerSourceChanged {
                source: Some(PowerSource::Ac),
            } => actions.push(AutomationAction::Acquire(AutomationLease {
                owner,
                change: ControlChange::SetDisplayRefreshRate { hz: 240 },
                priority: 500,
                specificity: 1,
                expires_at_monotonic_ms: None,
            })),
            AutomationEvent::PowerSourceChanged {
                source: Some(PowerSource::Battery),
            } => actions.push(AutomationAction::Release {
                target: TARGET,
                owner,
            }),
            _ => {}
        }
    }

    fn next_deadline_monotonic_ms(&self) -> Option<u64> {
        None
    }
}

#[test]
fn automation_lease_resumes_after_manual_release_then_restores_baseline() {
    let host = Fixture::start_with_engine(None, None, Box::new(AcRule));
    let mut ui = host.client();
    ui.hello(ClientKind::Ui);
    subscribe(
        &mut ui,
        Subscriptions {
            state: true,
            ..Default::default()
        },
    );
    let config = AppConfig {
        automation: AutomationConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(matches!(
        ui.call(IpcCommand::SetConfig {
            expected_revision: 0,
            config
        }),
        Ok(IpcReply::Config { revision: 1, .. })
    ));
    let enabled = state_push(&mut ui, |s| {
        s.capabilities.contains(CapabilityFlags::AUTOMATION)
    });
    let IpcReply::Applied { snapshot: manual } = ui.call(set_hz(enabled.revision, 120)).unwrap()
    else {
        panic!("expected manual override");
    };
    assert_eq!(manual.manual_overrides, [TARGET]);
    host.inbox.power_source(0);
    let on_ac = state_push(&mut ui, |s| {
        s.hardware.power_source == Some(PowerSource::Ac)
    });
    assert_eq!(on_ac.hardware.display_refresh_hz, Some(120));
    assert_eq!(host.counts.calls.load(Acquire), 1);
    let IpcReply::Applied {
        snapshot: automatic,
    } = ui
        .call(IpcCommand::ReleaseManualOverride {
            expected_revision: on_ac.revision,
            target: TARGET,
        })
        .unwrap()
    else {
        panic!("expected release");
    };
    assert!(automatic.manual_overrides.is_empty());
    assert_eq!(automatic.hardware.display_refresh_hz, Some(240));
    host.inbox.power_source(1);
    let restored = state_push(&mut ui, |s| {
        s.hardware.power_source == Some(PowerSource::Battery)
            && s.hardware.display_refresh_hz == Some(60)
    });
    assert!(restored.manual_overrides.is_empty());
    assert_eq!(host.counts.calls.load(Acquire), 3);
}

#[test]
fn real_rule_engine_drives_coordinator_leases_and_restores_mock_baseline() {
    let mut engine = msilt_automation::RuleEngine::new();
    engine.load(
        msilt_automation::AutomationRuleSet::parse(
            br#"
schema = 1
[[rules]]
id = "ac-panel"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "displayRefreshRate"
value = 240
"#,
        )
        .unwrap(),
    );
    let host = Fixture::start_with_engine(None, None, Box::new(engine));
    let mut ui = host.client();
    ui.hello(ClientKind::Ui);
    subscribe(
        &mut ui,
        Subscriptions {
            state: true,
            ..Default::default()
        },
    );
    ui.call(IpcCommand::SetConfig {
        expected_revision: 0,
        config: AppConfig {
            automation: AutomationConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        },
    })
    .unwrap();
    host.inbox.power_source(0);
    let automatic = state_push(&mut ui, |snapshot| {
        snapshot.hardware.display_refresh_hz == Some(240)
    });
    assert!(automatic.manual_overrides.is_empty());
    host.inbox.power_source(1);
    state_push(&mut ui, |snapshot| {
        snapshot.hardware.power_source == Some(PowerSource::Battery)
            && snapshot.hardware.display_refresh_hz == Some(60)
    });
    assert_eq!(host.counts.calls.load(Acquire), 2);
}
