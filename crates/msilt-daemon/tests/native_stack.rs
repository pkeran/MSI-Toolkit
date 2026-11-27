#![cfg(windows)]

use std::{
    collections::VecDeque,
    io,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use msilt_core::*;
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{
            Diagnostics::ToolHelp::*,
            IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED},
            Pipes::GetNamedPipeServerProcessId,
            Threading::{CREATE_NO_WINDOW, CreateEventW},
        },
        UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE},
    },
    core::{HRESULT, PCWSTR},
};

fn own(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

struct DaemonProcess(Child);

impl DaemonProcess {
    fn start(pipe: &str, pinned: bool, noop: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_msilt-daemon"));
        command.args(["--no-tray", "--readonly", "--pipe", pipe]);
        if pinned {
            command.args(["--test-client-pid", &std::process::id().to_string()]);
        }
        if noop {
            command.arg("--noop-backend");
        }
        Self(
            command
                .creation_flags(CREATE_NO_WINDOW.0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }

    fn stop(&mut self) {
        unsafe extern "system" fn close(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
            }
            if pid == context.0 as u32 {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            true.into()
        }
        if let Some(status) = self.0.try_wait().unwrap() {
            if !thread::panicking() {
                assert!(status.success(), "daemon exited with {status}");
            }
            return;
        }
        unsafe {
            EnumWindows(Some(close), LPARAM(self.0.id() as isize)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
            assert!(thread::panicking(), "daemon did not shut down gracefully");
        } else {
            assert!(self.0.wait().unwrap().success());
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Client {
    pipe: OwnedHandle,
    id: u64,
    events: VecDeque<IpcEvent>,
}

impl Client {
    fn connect(name: &str, child: &mut DaemonProcess) -> Self {
        let client = Self::connect_alive(name, || child.0.try_wait().unwrap().is_none());
        let mut pid = 0;
        unsafe {
            GetNamedPipeServerProcessId(raw(&client.pipe), &mut pid).unwrap();
        }
        assert_eq!(pid, child.0.id(), "test endpoint is not the spawned daemon");
        client
    }

    fn connect_alive(name: &str, mut alive: impl FnMut() -> bool) -> Self {
        let name: Vec<_> = format!(r"\\.\pipe\{name}")
            .encode_utf16()
            .chain([0])
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    0x0010_0083,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            } {
                Ok(pipe) => {
                    return Self {
                        pipe: own(pipe),
                        id: 0,
                        events: VecDeque::new(),
                    };
                }
                Err(error) if Instant::now() < deadline => {
                    assert!(alive(), "daemon startup: {error}");
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connecting to daemon: {error}"),
            }
        }
    }

    fn io(
        &self,
        timeout: u32,
        op: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        let event =
            own(unsafe { CreateEventW(None, true, false, None) }.map_err(io::Error::other)?);
        let mut ov = OVERLAPPED {
            hEvent: raw(&event),
            ..Default::default()
        };
        if let Err(error) = op(&mut ov) {
            if error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(io::Error::other(error));
            }
        }
        let mut count = 0;
        if let Err(error) =
            unsafe { GetOverlappedResultEx(raw(&self.pipe), &ov, &mut count, timeout, false) }
        {
            unsafe {
                let _ = CancelIoEx(raw(&self.pipe), Some(&ov));
                let _ = GetOverlappedResult(raw(&self.pipe), &ov, &mut count, true);
            }
            return Err(io::Error::other(error));
        }
        Ok(count as usize)
    }

    fn send(&mut self, command: IpcCommand) -> u64 {
        self.id += 1;
        let frame = encode_frame(&IpcMessage::Request(IpcRequest {
            id: self.id,
            ttl_ms: Some(MAX_QUEUE_TTL_MS),
            command,
        }))
        .unwrap();
        let mut bytes = frame.as_slice();
        while !bytes.is_empty() {
            let count = self
                .io(5000, |ov| unsafe {
                    WriteFile(raw(&self.pipe), Some(bytes), None, Some(ov))
                })
                .unwrap();
            assert!(count > 0);
            bytes = &bytes[count..];
        }
        self.id
    }

    fn exact(&self, mut bytes: &mut [u8], timeout: u32) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.io(timeout, |ov| unsafe {
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
        let mut header = [0; FRAME_HEADER_LEN];
        self.exact(&mut header, timeout)?;
        let len = u32::from_le_bytes(header) as usize;
        assert!((1..=MAX_FRAME_PAYLOAD).contains(&len));
        let mut frame = vec![0; FRAME_HEADER_LEN + len];
        frame[..FRAME_HEADER_LEN].copy_from_slice(&header);
        self.exact(&mut frame[FRAME_HEADER_LEN..], timeout)?;
        Ok(decode_frame(&frame).unwrap().unwrap().0)
    }

    fn call(&mut self, command: IpcCommand) -> Result<IpcReply, ErrorCode> {
        let id = self.send(command);
        for _ in 0..64 {
            match self.receive(10000).unwrap() {
                IpcMessage::Response(reply) => {
                    assert_eq!(reply.id, id);
                    return match reply.result {
                        IpcResult::Ok { reply } => Ok(reply),
                        IpcResult::Error { error } => Err(error.code),
                    };
                }
                IpcMessage::Event(event) => self.events.push_back(event),
                _ => panic!("daemon sent a request"),
            }
        }
        panic!("response starved by events");
    }

    fn hello(&mut self, kind: ClientKind) -> SystemSnapshot {
        let IpcReply::Hello {
            version: IPC_VERSION,
            snapshot,
        } = self
            .call(IpcCommand::Hello {
                version: IPC_VERSION,
                client: kind,
            })
            .unwrap()
        else {
            panic!("invalid hello");
        };
        snapshot
    }

    fn snapshot(&mut self) -> SystemSnapshot {
        let IpcReply::Snapshot { snapshot } = self.call(IpcCommand::GetSnapshot {}).unwrap() else {
            panic!("invalid snapshot");
        };
        snapshot
    }

    fn subscribe(&mut self, topics: Subscriptions) {
        let IpcReply::Subscriptions { topics: actual, .. } =
            self.call(IpcCommand::SetSubscriptions { topics }).unwrap()
        else {
            panic!("invalid subscriptions");
        };
        assert_eq!(actual, topics);
    }
}

fn modules(pid: u32) -> Vec<String> {
    let snapshot =
        own(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }
                .unwrap(),
        );
    let mut entry = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut result = Vec::new();
    unsafe {
        Module32FirstW(raw(&snapshot), &mut entry).unwrap();
    }
    loop {
        let len = entry.szModule.iter().position(|&c| c == 0).unwrap();
        result.push(String::from_utf16_lossy(&entry.szModule[..len]).to_ascii_lowercase());
        if unsafe { Module32NextW(raw(&snapshot), &mut entry) }.is_err() {
            break;
        }
    }
    result
}

#[test]
fn native_readonly_hal_and_rule_engine_cross_process() {
    let name = format!("MSIToolkit-native-e2e-{}", std::process::id());
    let mut daemon = DaemonProcess::start(&name, true, false);
    let mut client = Client::connect(&name, &mut daemon);
    let initial = client.hello(ClientKind::Ui);
    let probe = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "unauthorized_process_probe", "--nocapture"])
        .env("MSILT_UNAUTHORIZED_TEST_PIPE", &name)
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "unauthorized peer test: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    println!("PINNED_PEER=separate same-logon process rejected with AccessDenied");
    assert_eq!(initial.status, DaemonStatus::Ready);
    assert_eq!(initial.writable, CapabilityFlags::NONE);
    assert_eq!(initial.ec_write_status, EcWriteStatus::UnverifiedMapping);
    assert!(initial.hardware.identity.board_id.is_some());
    assert_eq!(client.snapshot(), initial);
    let IpcReply::Config { config, revision } = client.call(IpcCommand::GetConfig {}).unwrap()
    else {
        panic!("invalid config");
    };
    config.validate().unwrap();
    assert_eq!(revision, 0);
    if std::env::var_os("MSILT_EXPECT_MS17S3").is_some() {
        assert_eq!(
            initial.hardware.identity.board_id.as_deref(),
            Some("MS-17S3")
        );
        assert_eq!(initial.hardware.display_refresh_hz, Some(240));
        assert!(initial.hardware.display_brightness_percent.is_some());
        assert!(initial.hardware.battery_percent.is_some());
        assert!(!initial.hardware.supported_color_profiles.is_empty());
    }
    println!("LIVE_READONLY_SNAPSHOT={:?}", initial.hardware);
    let startup_modules = modules(daemon.0.id());
    assert!(
        !startup_modules
            .iter()
            .any(|name| name == "nvml.dll" || name.starts_with("nvapi"))
    );
    client.subscribe(Subscriptions {
        state: true,
        ..Default::default()
    });
    client.subscribe(Subscriptions {
        state: true,
        telemetry: true,
        osd: false,
        gpu_telemetry: false,
    });
    let mut ticks = Vec::new();
    while ticks.len() < 3 {
        if let IpcMessage::Event(IpcEvent::TelemetryTick { sample }) = client.receive(4000).unwrap()
        {
            ticks.push(sample.sampled_at_unix_ms);
        }
    }
    assert!(
        ticks
            .windows(2)
            .all(|pair| pair[1] > pair[0] && pair[1] - pair[0] >= 900)
    );
    let sampled_modules = modules(daemon.0.id());
    assert!(
        !sampled_modules
            .iter()
            .any(|name| name == "nvml.dll" || name.starts_with("nvapi"))
    );
    println!("TELEMETRY_TICKS={ticks:?}; NVML/NVAPI absent at startup and after 3 ticks");
    client.subscribe(Subscriptions::default());
    client.events.clear();
    assert!(
        client.receive(1400).is_err(),
        "push continued after unsubscribe"
    );
    for (change, error) in [
        (
            ControlChange::SetDisplayRefreshRate { hz: 60 },
            ErrorCode::Unsupported,
        ),
        (
            ControlChange::SetDisplayBrightness { percent: 50 },
            ErrorCode::Unsupported,
        ),
        (
            ControlChange::SetCoolerBoost { enabled: true },
            ErrorCode::WriteNotAllowed,
        ),
    ] {
        let revision = client.snapshot().revision;
        assert_eq!(
            client.call(IpcCommand::Control {
                expected_revision: revision,
                change
            }),
            Err(error)
        );
    }
    assert_eq!(client.snapshot().hardware, initial.hardware);
    println!("READONLY_WRITES=Unsupported/Unsupported/WriteNotAllowed; hardware unchanged");
    drop(client);
    daemon.stop();
    println!(
        "DAEMON_EXIT={}; graceful WM_CLOSE; no remaining child",
        daemon.0.wait().unwrap()
    );
}

#[test]
fn unpinned_custom_endpoint_does_not_grant_ui_telemetry() {
    let name = format!("MSIToolkit-unpinned-e2e-{}", std::process::id());
    let mut daemon = DaemonProcess::start(&name, false, true);
    let mut client = Client::connect(&name, &mut daemon);
    client.hello(ClientKind::Ui);
    assert_eq!(
        client.call(IpcCommand::SetSubscriptions {
            topics: Subscriptions {
                telemetry: true,
                ..Default::default()
            },
        }),
        Err(ErrorCode::Unsupported)
    );
}

#[test]
fn native_readonly_system_tools_can_inspect_all_eleven_objects() {
    let name = format!("MSIToolkit-tools-e2e-{}", std::process::id());
    let mut daemon = DaemonProcess::start(&name, true, true);
    let mut client = Client::connect(&name, &mut daemon);
    client.hello(ClientKind::Ui);
    let IpcReply::SystemTools { startup, oem } =
        client.call(IpcCommand::GetSystemTools {}).unwrap()
    else {
        panic!("missing system tools reply");
    };
    assert!(!startup.detail.is_empty());
    assert_eq!(oem.items.len(), 11);
    assert_eq!(oem.packages.len(), 3);
    assert_eq!(oem.elevated, startup.elevated);
}

#[test]
fn diagnostic_flags_cannot_reclassify_the_production_endpoint() {
    for pipe in [PIPE_NAME, r"\\.\pipe\msitoolkit-ipc-v1"] {
        let status = Command::new(env!("CARGO_BIN_EXE_msilt-daemon"))
            .args([
                "--readonly",
                "--no-tray",
                "--pipe",
                pipe,
                "--test-client-pid",
                &std::process::id().to_string(),
            ])
            .creation_flags(CREATE_NO_WINDOW.0)
            .status()
            .unwrap();
        assert!(!status.success());
    }
    let status = Command::new(env!("CARGO_BIN_EXE_msilt-daemon"))
        .args(["--readonly", "--allow-platform-control"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .unwrap();
    assert!(!status.success());
    for args in [
        vec!["--enable-ec-writes"],
        vec!["--readonly", "--acpi", "--enable-ec-writes"],
    ] {
        let status = Command::new(env!("CARGO_BIN_EXE_msilt-daemon"))
            .args(args)
            .creation_flags(CREATE_NO_WINDOW.0)
            .status()
            .unwrap();
        assert!(!status.success());
    }
}

#[test]
fn unauthorized_process_probe() {
    let Ok(name) = std::env::var("MSILT_UNAUTHORIZED_TEST_PIPE") else {
        return;
    };
    let mut client = Client::connect_alive(&name, || true);
    assert_eq!(
        client.call(IpcCommand::Hello {
            version: IPC_VERSION,
            client: ClientKind::Ui
        }),
        Err(ErrorCode::AccessDenied)
    );
}
