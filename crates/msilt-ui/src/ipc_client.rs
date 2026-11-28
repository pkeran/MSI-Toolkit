//! Authenticated, cancellable named-pipe transport. No pipe I/O runs on the UI thread.
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use msilt_core::{ClientKind, IPC_VERSION, IpcCommand, IpcMessage, IpcRequest, PIPE_NAME};

#[path = "pipe_windows.rs"]
mod platform;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub name: String,
    pub diagnostic: bool,
}

impl Endpoint {
    pub fn new(name: Option<String>) -> io::Result<Self> {
        let name = name.unwrap_or_else(|| PIPE_NAME.to_owned());
        let prefix = r"\\.\pipe\";
        let Some(suffix) = name.get(prefix.len()..) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "管道必须使用本地 \\\\.\\pipe\\name"));
        };
        if !name[..prefix.len()].eq_ignore_ascii_case(prefix)
            || suffix.is_empty() || suffix.len() > 180
            || !suffix.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_ .".contains(&c))
            || suffix.ends_with([' ', '.'])
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "管道名必须是本地 ASCII 名称，不能含路径或 NUL"));
        }
        let diagnostic = !name.eq_ignore_ascii_case(PIPE_NAME);
        Ok(Self { name: if diagnostic { name } else { PIPE_NAME.to_owned() }, diagnostic })
    }
}

#[derive(Debug)]
pub enum Incoming { Message(IpcMessage), Lost(String) }

type Inbox = Arc<Mutex<VecDeque<Incoming>>>;
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct IpcClient {
    inbox: Inbox,
    next_id: AtomicU64,
    sender: mpsc::SyncSender<IpcRequest>,
    signals: Arc<platform::Signals>,
    thread: Option<JoinHandle<()>>,
    diagnostic: bool,
}

impl IpcClient {
    pub fn connect(endpoint: Endpoint, waker: Waker) -> io::Result<Self> {
        let signals = Arc::new(platform::Signals::new()?);
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        let (sender, receiver) = mpsc::sync_channel(16);
        let worker_signals = signals.clone();
        let worker_inbox = inbox.clone();
        let diagnostic = endpoint.diagnostic;
        let worker = thread::Builder::new().name("msilt-ipc".into()).spawn(move || {
            let result = platform::run(&endpoint, &worker_signals, receiver, |message| {
                push(&worker_inbox, &waker, Incoming::Message(message));
            });
            if !worker_signals.stopped() {
                let reason = result.err().map_or_else(|| "daemon 已关闭管道".into(), |e| e.to_string());
                push(&worker_inbox, &waker, Incoming::Lost(reason));
            }
        })?;
        Ok(Self { inbox, next_id: AtomicU64::new(1), sender, signals, thread: Some(worker), diagnostic })
    }

    pub fn hello(&self) -> io::Result<u64> {
        self.send(IpcCommand::Hello { version: IPC_VERSION, client: ClientKind::Ui })
    }

    pub fn send(&self, command: IpcCommand) -> io::Result<u64> {
        if self.diagnostic
            && matches!(
                command,
                IpcCommand::Control { .. }
                    | IpcCommand::SetConfig { .. }
                    | IpcCommand::ReleaseManualOverride { .. }
                    | IpcCommand::ResetFanMode { .. }
                    | IpcCommand::SetStartup { .. }
                    | IpcCommand::ManageOem { .. }
                    | IpcCommand::SetAutomationRuleEnabled { .. }
                    | IpcCommand::AddAutomationPreset { .. }
                    | IpcCommand::DeleteAutomationRule { .. }
            )
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "诊断管道只允许读取与订阅"));
        }
        if self.signals.stopped() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "连接已关闭"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sender.try_send(IpcRequest { id, ttl_ms: None, command }).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => io::Error::new(io::ErrorKind::WouldBlock, "IPC 请求队列已满"),
            mpsc::TrySendError::Disconnected(_) => io::Error::new(io::ErrorKind::NotConnected, "连接已关闭"),
        })?;
        self.signals.wake();
        Ok(id)
    }

    pub fn drain(&self) -> Vec<Incoming> {
        self.inbox.lock().map(|mut inbox| inbox.drain(..).collect()).unwrap_or_default()
    }

    pub fn shutdown(&mut self) {
        // Stop is latched before CancelIoEx, closing the race with the next ReadFile.
        self.signals.cancel();
        if let Some(worker) = self.thread.take() { let _ = worker.join(); }
    }
}

impl Drop for IpcClient {
    fn drop(&mut self) { self.shutdown(); }
}

fn push(inbox: &Inbox, waker: &Waker, item: Incoming) {
    if let Ok(mut queue) = inbox.lock() {
        if matches!(item, Incoming::Message(IpcMessage::Event(msilt_core::IpcEvent::TelemetryTick { .. }))) {
            queue.retain(|old| !matches!(old, Incoming::Message(IpcMessage::Event(msilt_core::IpcEvent::TelemetryTick { .. }))));
        } else if matches!(item, Incoming::Message(IpcMessage::Event(msilt_core::IpcEvent::BatteryTick { .. }))) {
            queue.retain(|old| !matches!(old, Incoming::Message(IpcMessage::Event(msilt_core::IpcEvent::BatteryTick { .. }))));
        }
        if queue.len() >= 64 {
            queue.clear();
            queue.push_back(Incoming::Lost("IPC 事件队列超限".into()));
        } else { queue.push_back(item); }
    }
    waker();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pipe_never_becomes_diagnostic() {
        for name in [None, Some(PIPE_NAME.into()), Some(PIPE_NAME.to_ascii_lowercase())] {
            let endpoint = Endpoint::new(name).unwrap();
            assert!(!endpoint.diagnostic);
            assert_eq!(endpoint.name, PIPE_NAME);
        }
    }

    #[test]
    fn diagnostic_pipe_is_local_and_explicit() {
        assert!(Endpoint::new(Some(r"\\.\pipe\MSIToolkit-ui-audit".into())).unwrap().diagnostic);
        for name in [r"\\server\pipe\test", r"\\.\pipe\", r"\\.\pipe\x\y", "\\\\.\\pipe\\x\0", "ééé"] {
            assert!(Endpoint::new(Some(name.into())).is_err(), "{name:?}");
        }
    }
}
