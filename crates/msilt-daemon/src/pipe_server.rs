use std::{
    cell::UnsafeCell,
    io,
    os::windows::io::OwnedHandle,
    sync::Arc,
    time::{Duration, Instant},
};

use msilt_core::{
    BatterySample, ClientKind, ErrorCode, FRAME_HEADER_LEN, IpcError, IpcEvent, IpcMessage,
    IpcReply, IpcRequest, IpcResponse, IpcResult, MAX_FRAME_PAYLOAD, OsdKind, Subscriptions,
    SystemSnapshot, TelemetrySample,
};
use msilt_hal::daemon_wire::{Outgoing, decode_request, encode_reply};
use windows::{
    Win32::{
        Foundation::{ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED},
        Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile,
            WriteFile,
        },
        System::{
            IO::{CancelIoEx, OVERLAPPED},
            Pipes::*,
        },
    },
    core::{Error, PCWSTR},
};

use crate::{
    iocp::{PIPE_KEY, Packet, Port},
    security::{Security, SecurityDescriptor},
    win::{is_error, os_error, own, raw, wide},
};

pub(crate) const MAX_CONNECTIONS: usize = 8;
const PROGRESS: Duration = Duration::from_secs(5);
const CHUNK: usize = 4096;
const RETAIN_BYTES: usize = 8192;
const MAX_RECEIVE: usize = FRAME_HEADER_LEN + MAX_FRAME_PAYLOAD + CHUNK;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    index: usize,
    generation: u64,
}

pub(crate) struct Request {
    pub(crate) peer: Peer,
    pub(crate) received: Instant,
    pub(crate) data: IpcRequest,
}

struct Connection {
    generation: u64,
    hello_by: Option<Instant>,
    client: Option<(ClientKind, bool)>,
    last_id: u64,
    awaiting: Option<u64>,
    ending: bool,
    linger_by: Option<Instant>,
    rx: Vec<u8>,
    frame_by: Option<Instant>,
    topics: Subscriptions,
    response: Option<IpcResponse>,
    state: Option<Arc<SystemSnapshot>>,
    sample: Option<TelemetrySample>,
    battery: Option<BatterySample>,
    osd: Option<(u64, OsdKind)>,
}

impl Connection {
    fn new(generation: u64, now: Instant) -> Self {
        Self {
            generation,
            hello_by: Some(now + PROGRESS),
            client: None,
            last_id: 0,
            awaiting: None,
            ending: false,
            linger_by: None,
            rx: Vec::new(),
            frame_by: None,
            topics: Subscriptions::default(),
            response: None,
            state: None,
            sample: None,
            battery: None,
            osd: None,
        }
    }

    fn request(&mut self, bytes: &[u8], now: Instant) -> Result<Option<IpcRequest>, ()> {
        if bytes.len() > MAX_RECEIVE - self.rx.len() {
            return Err(());
        }
        self.rx.extend_from_slice(bytes);
        let Some((request, consumed)) = decode_request(&self.rx).map_err(|_| ())? else {
            self.frame_by = Some(now + PROGRESS);
            return Ok(None);
        };
        // Any trailing bytes would start a second request before the first response.
        if consumed != self.rx.len() || self.awaiting.is_some() || request.id == 0 {
            return Err(());
        }
        self.awaiting = Some(request.id);
        self.frame_by = None;
        clear_buffer(&mut self.rx);
        Ok(Some(request))
    }

    fn next_message(&mut self) -> Option<IpcMessage> {
        if let Some(reply) = self.response.take() {
            self.awaiting = None;
            return Some(IpcMessage::Response(reply));
        }
        if self.ending {
            return None;
        }
        self.state
            .take()
            .map(|snapshot| IpcEvent::StateChanged {
                snapshot: (*snapshot).clone(),
            })
            .or_else(|| {
                self.osd
                    .take()
                    .map(|(revision, kind)| IpcEvent::OsdNotify { revision, kind })
            })
            .or_else(|| {
                self.sample
                    .take()
                    .map(|sample| IpcEvent::TelemetryTick { sample })
            })
            .or_else(|| {
                self.battery
                    .take()
                    .map(|sample| IpcEvent::BatteryTick { sample })
            })
            .map(IpcMessage::Event)
    }
}

fn clear_buffer(buffer: &mut Vec<u8>) {
    if buffer.capacity() > RETAIN_BYTES {
        *buffer = Vec::new();
    } else {
        buffer.clear();
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Reading {
    Accept,
    Data,
}

#[derive(Default)]
struct Overlapped {
    read: UnsafeCell<OVERLAPPED>,
    write: UnsafeCell<OVERLAPPED>,
}

struct Slot {
    handle: OwnedHandle,
    ops: Box<Overlapped>,
    read_bytes: Option<Box<UnsafeCell<[u8; CHUNK]>>>,
    read: Option<Reading>,
    write: bool,
    write_by: Option<Instant>,
    tx: Vec<u8>,
    written: usize,
    connection: Option<Connection>,
    closing: bool,
    generation: u64,
}

impl Slot {
    fn create(handle: OwnedHandle) -> Self {
        Self {
            handle,
            ops: Box::default(),
            read_bytes: None,
            read: None,
            write: false,
            write_by: None,
            tx: Vec::new(),
            written: 0,
            connection: None,
            closing: false,
            generation: 0,
        }
    }

    fn pending(&self) -> bool {
        self.read.is_some() || self.write
    }

    fn listen(&mut self, now: Instant) -> io::Result<()> {
        let ov = self.ops.read.get();
        unsafe {
            ov.write(OVERLAPPED::default());
        }
        match unsafe { ConnectNamedPipe(raw(&self.handle), Some(ov)) } {
            Err(error) if is_error(&error, ERROR_PIPE_CONNECTED) => self.accept(now),
            Err(error) if is_error(&error, ERROR_NO_DATA) => self.close(),
            result => {
                overlapped(result)?;
                self.read = Some(Reading::Accept);
            }
        }
        Ok(())
    }

    fn accept(&mut self, now: Instant) {
        self.generation = self.generation.wrapping_add(1);
        self.connection = Some(Connection::new(self.generation, now));
        self.read_bytes = Some(Box::new(UnsafeCell::new([0; CHUNK])));
        self.read();
    }

    fn read(&mut self) {
        if self.read.is_some() || self.closing || self.connection.is_none() {
            return;
        }
        let Some(buffer) = &self.read_bytes else {
            return;
        };
        let operation = self.ops.read.get();
        let result = unsafe {
            operation.write(OVERLAPPED::default());
            ReadFile(
                raw(&self.handle),
                Some(&mut *buffer.get()),
                None,
                Some(operation),
            )
        };
        if overlapped(result).is_ok() {
            self.read = Some(Reading::Data);
        } else {
            self.close();
        }
    }

    fn write(&mut self, now: Instant) {
        if self.write || self.closing {
            return;
        }
        let Some(connection) = &mut self.connection else {
            return;
        };
        if self.tx.is_empty() {
            let Some(message) = connection.next_message() else {
                if connection.ending {
                    connection.linger_by.get_or_insert(now + PROGRESS);
                }
                return;
            };
            let outgoing = match &message {
                IpcMessage::Response(response) => Outgoing::Response(response),
                IpcMessage::Event(event) => Outgoing::Event(event),
                IpcMessage::Request(_) => unreachable!("the server sends only replies and events"),
            };
            if encode_reply(outgoing, &mut self.tx).is_err() {
                self.close();
                return;
            }
            self.written = 0;
        }
        let operation = self.ops.write.get();
        let result = unsafe {
            operation.write(OVERLAPPED::default());
            WriteFile(
                raw(&self.handle),
                Some(&self.tx[self.written..]),
                None,
                Some(operation),
            )
        };
        if overlapped(result).is_ok() {
            self.write = true;
            self.write_by = Some(now + PROGRESS);
        } else {
            self.close();
        }
    }

    fn completed(&mut self, index: usize, packet: Packet, now: Instant) -> Option<Request> {
        if packet.overlapped == self.ops.read.get() {
            let reading = self.read.take()?;
            if self.closing || !packet.ok {
                self.close();
            } else if reading == Reading::Accept {
                self.accept(now);
            } else if packet.bytes == 0 || packet.bytes > CHUNK {
                self.close();
            } else if let Some(connection) = &mut self.connection {
                if connection.ending {
                    self.close();
                    return None;
                }
                let buffer = self.read_bytes.as_ref().expect("active read buffer");
                let bytes = unsafe { &(&*buffer.get())[..packet.bytes] };
                match connection.request(bytes, now) {
                    Ok(Some(data)) => {
                        return Some(Request {
                            peer: Peer {
                                index,
                                generation: connection.generation,
                            },
                            received: now,
                            data,
                        });
                    }
                    Ok(None) => self.read(),
                    Err(()) => self.close(),
                }
            }
        } else if packet.overlapped == self.ops.write.get() && self.write {
            self.write = false;
            self.write_by = None;
            if self.closing
                || !packet.ok
                || packet.bytes == 0
                || packet.bytes > self.tx.len() - self.written
            {
                self.close();
            } else {
                self.written += packet.bytes;
                if self.written == self.tx.len() {
                    clear_buffer(&mut self.tx);
                }
            }
        }
        None
    }

    fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            self.connection = None;
            self.write_by = None;
            if self.pending() {
                unsafe {
                    let _ = CancelIoEx(raw(&self.handle), None);
                }
            }
        }
        if !self.pending() {
            unsafe {
                let _ = DisconnectNamedPipe(raw(&self.handle));
            }
            self.closing = false;
            self.read_bytes = None;
            self.tx = Vec::new();
        }
    }

    fn deadline(&self) -> Option<Instant> {
        let connection = self.connection.as_ref()?;
        [
            connection.hello_by,
            connection.frame_by,
            connection.linger_by,
            self.write_by,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn overlapped(result: windows::core::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if is_error(&error, ERROR_IO_PENDING) => Ok(()),
        result => result.map_err(os_error),
    }
}

pub(crate) struct PipeServer {
    port: Port,
    name: Vec<u16>,
    security: Security,
    descriptor: SecurityDescriptor,
    slots: [Option<Slot>; MAX_CONNECTIONS],
    listener_retry: Option<Instant>,
}

impl PipeServer {
    pub(crate) fn new(port: Port, security: Security, name: &str) -> io::Result<Self> {
        let descriptor = security.pipe_descriptor()?;
        let mut server = Self {
            port,
            name: wide(name),
            security,
            descriptor,
            slots: std::array::from_fn(|_| None),
            listener_retry: None,
        };
        server.listen(Instant::now())?;
        Ok(server)
    }

    fn listen(&mut self, now: Instant) -> io::Result<()> {
        if self.listener_retry.is_some_and(|retry| now < retry) {
            return Ok(());
        }
        self.listener_retry = None;
        for _ in 0..MAX_CONNECTIONS {
            if self
                .slots
                .iter()
                .flatten()
                .any(|slot| slot.read == Some(Reading::Accept))
            {
                return Ok(());
            }
            let vacant = self.slots.iter().position(|slot| {
                slot.as_ref().is_some_and(|slot| {
                    !slot.closing && !slot.pending() && slot.connection.is_none()
                })
            });
            let Some(index) = vacant.or_else(|| self.slots.iter().position(Option::is_none)) else {
                return Ok(());
            };
            if self.slots[index].is_none() {
                let first = self.slots.iter().all(Option::is_none);
                let mut flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
                if first {
                    flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
                }
                let attributes = self.descriptor.attributes();
                let handle = unsafe {
                    CreateNamedPipeW(
                        PCWSTR(self.name.as_ptr()),
                        flags,
                        PIPE_TYPE_BYTE
                            | PIPE_READMODE_BYTE
                            | PIPE_WAIT
                            | PIPE_REJECT_REMOTE_CLIENTS,
                        MAX_CONNECTIONS as u32,
                        CHUNK as u32,
                        CHUNK as u32,
                        0,
                        Some(&attributes),
                    )
                };
                if handle.is_invalid() {
                    return Err(os_error(Error::from_thread()));
                }
                let handle = unsafe { own(handle) };
                self.security
                    .verify_pipe(&self.descriptor, raw(&handle), first)?;
                self.port.associate(raw(&handle), PIPE_KEY + index)?;
                self.slots[index] = Some(Slot::create(handle));
            }
            self.slots[index]
                .as_mut()
                .expect("created slot")
                .listen(now)?;
        }
        // A racing peer can connect and vanish before ConnectNamedPipe. Bound retry work.
        self.listener_retry = Some(now + Duration::from_millis(100));
        Ok(())
    }

    pub(crate) fn completed(&mut self, packet: Packet, now: Instant) -> Option<Request> {
        let index = packet.key.checked_sub(PIPE_KEY)?;
        self.slots
            .get_mut(index)?
            .as_mut()?
            .completed(index, packet, now)
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.deadline().is_some_and(|until| now >= until) {
                slot.close();
            }
        }
    }

    pub(crate) fn service(&mut self, now: Instant) -> io::Result<()> {
        for slot in self.slots.iter_mut().flatten() {
            slot.write(now);
            slot.read();
        }
        self.listen(now)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.slots
            .iter()
            .flatten()
            .filter_map(Slot::deadline)
            .chain(self.listener_retry)
            .min()
    }

    fn connection(&self, peer: Peer) -> Option<&Connection> {
        self.slots[peer.index]
            .as_ref()?
            .connection
            .as_ref()
            .filter(|connection| connection.generation == peer.generation && !connection.ending)
    }

    fn connection_mut(&mut self, peer: Peer) -> Option<&mut Connection> {
        self.slots[peer.index]
            .as_mut()?
            .connection
            .as_mut()
            .filter(|connection| connection.generation == peer.generation && !connection.ending)
    }

    pub(crate) fn alive(&self, peer: Peer) -> bool {
        self.connection(peer).is_some()
    }

    pub(crate) fn client(&self, peer: Peer) -> Option<(ClientKind, bool)> {
        self.connection(peer)?.client
    }

    pub(crate) fn check_id(&mut self, peer: Peer, id: u64) -> bool {
        let Some(connection) = self.connection_mut(peer) else {
            return false;
        };
        if id <= connection.last_id {
            return false;
        }
        connection.last_id = id;
        true
    }

    pub(crate) fn authenticate(
        &mut self,
        peer: Peer,
        client: ClientKind,
    ) -> Result<bool, ErrorCode> {
        let Some(slot) = self.slots[peer.index].as_ref() else {
            return Err(ErrorCode::AccessDenied);
        };
        self.security
            .authenticate(raw(&slot.handle), client == ClientKind::Ui)
            .map_err(|_| ErrorCode::AccessDenied)
    }

    pub(crate) fn authenticated(&mut self, peer: Peer, client: ClientKind, verified_ui: bool) {
        if let Some(connection) = self.connection_mut(peer) {
            connection.hello_by = None;
            connection.client = Some((client, verified_ui));
        }
    }

    pub(crate) fn subscriptions(&mut self, peer: Peer, topics: Subscriptions) {
        if let Some(connection) = self.connection_mut(peer) {
            connection.topics = topics;
            if !topics.state {
                connection.state = None;
            }
            if !topics.osd {
                connection.osd = None;
            }
            if !topics.telemetry {
                connection.sample = None;
            }
            if !topics.battery {
                connection.battery = None;
            }
        }
    }

    pub(crate) fn wants_telemetry(&self) -> bool {
        self.active().any(|connection| {
            connection.topics.telemetry && connection.client == Some((ClientKind::Ui, true))
        })
    }

    pub(crate) fn wants_gpu_telemetry(&self) -> bool {
        self.active().any(|connection| {
            connection.topics.gpu_telemetry && connection.client == Some((ClientKind::Ui, true))
        })
    }

    pub(crate) fn wants_battery(&self) -> bool {
        self.active().any(|connection| {
            connection.topics.battery && connection.client == Some((ClientKind::Ui, true))
        })
    }

    pub(crate) fn has_clients(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.connection.is_some() || slot.closing)
    }

    fn active(&self) -> impl Iterator<Item = &Connection> {
        self.slots
            .iter()
            .flatten()
            .filter_map(|slot| slot.connection.as_ref())
            .filter(|connection| !connection.ending)
    }

    fn active_mut(&mut self) -> impl Iterator<Item = &mut Connection> {
        self.slots
            .iter_mut()
            .flatten()
            .filter_map(|slot| slot.connection.as_mut())
            .filter(|connection| !connection.ending)
    }

    pub(crate) fn send_state(&mut self, snapshot: &Arc<SystemSnapshot>) {
        for connection in self.active_mut().filter(|c| c.topics.state) {
            connection.state = Some(snapshot.clone());
        }
    }

    pub(crate) fn send_osd(&mut self, revision: u64, kind: OsdKind) {
        for connection in self.active_mut().filter(|c| c.topics.osd) {
            connection.osd = Some((revision, kind));
        }
    }

    pub(crate) fn send_sample(&mut self, sample: TelemetrySample) {
        for connection in self.active_mut().filter(|c| c.topics.telemetry) {
            connection.sample = Some(sample);
        }
    }

    pub(crate) fn clear_samples(&mut self) {
        for connection in self.active_mut() {
            connection.sample = None;
        }
    }

    pub(crate) fn send_battery(&mut self, sample: BatterySample) {
        for connection in self.active_mut().filter(|c| c.topics.battery) {
            connection.battery = Some(sample);
        }
    }

    pub(crate) fn clear_battery(&mut self) {
        for connection in self.active_mut() {
            connection.battery = None;
        }
    }

    pub(crate) fn reply(
        &mut self,
        peer: Peer,
        id: u64,
        result: Result<IpcReply, ErrorCode>,
        close: bool,
    ) {
        if let Some(connection) = self.connection_mut(peer) {
            if connection.awaiting != Some(id) || connection.response.is_some() {
                return;
            }
            connection.response = Some(IpcResponse {
                id,
                result: match result {
                    Ok(reply) => IpcResult::Ok { reply },
                    Err(code) => IpcResult::Error {
                        error: IpcError { code },
                    },
                },
            });
            if close {
                connection.ending = true;
                connection.topics = Subscriptions::default();
                connection.hello_by = None;
                connection.frame_by = None;
            }
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            slot.close();
        }
        while self.slots.iter().flatten().any(Slot::pending) {
            let Ok(packet) = self.port.wait() else {
                std::process::abort();
            };
            self.completed(packet, Instant::now());
        }
    }
}
