use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AppConfig, AutomationPreset, AutomationRuleView, ControlChange, ControlError, ControlTarget,
    HalError, OemAction, OemInventory, OsdKind, StartupStatus, SystemSnapshot, TelemetrySample,
    TransactionFailure, WriteGuardError, model::str_enum_serde,
};

pub const PIPE_NAME: &str = r"\\.\pipe\MSIToolkit-IPC-v1";
pub const IPC_VERSION: u16 = 1;
pub const FRAME_HEADER_LEN: usize = 4;
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
pub const DEFAULT_QUEUE_TTL_MS: u16 = 5_000;
pub const MAX_QUEUE_TTL_MS: u16 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Ui,
    Cli,
}
str_enum_serde!(ClientKind, "client kind",
    Ui => "ui",
    Cli => "cli",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Subscriptions {
    pub state: bool,
    pub telemetry: bool,
    pub osd: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub gpu_telemetry: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Subscriptions {
    pub const fn validate_for(self, client: ClientKind) -> Result<(), ErrorCode> {
        if (self.telemetry || self.gpu_telemetry) && matches!(client, ClientKind::Cli) {
            return Err(ErrorCode::Unsupported);
        }
        Ok(())
    }
}

pub type RequestId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IpcRequest {
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u16>,
    pub command: IpcCommand,
}

impl IpcRequest {
    pub fn queue_ttl_ms(&self) -> Result<u16, ErrorCode> {
        let ttl = self.ttl_ms.unwrap_or(DEFAULT_QUEUE_TTL_MS);
        if self.id == 0 || !(1..=MAX_QUEUE_TTL_MS).contains(&ttl) {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(ttl)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum IpcCommand {
    Hello { version: u16, client: ClientKind },
    GetSnapshot {},
    SetSubscriptions { topics: Subscriptions },
    Control { expected_revision: u64, change: ControlChange },
    ReleaseManualOverride { expected_revision: u64, target: ControlTarget },
    ResetFanMode { expected_revision: u64 },
    GetConfig {},
    SetConfig { expected_revision: u64, config: AppConfig },
    GetSystemTools {},
    SetStartup { enabled: bool },
    ManageOem { action: OemAction },
    GetAutomationRules {},
    SetAutomationRuleEnabled { id: String, enabled: bool },
    AddAutomationPreset { preset: AutomationPreset },
    DeleteAutomationRule { id: String },
    DryRunAutomation {},
    TestOsd {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: RequestId,
    pub result: IpcResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum IpcResult {
    Ok { reply: IpcReply },
    Error { error: IpcError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IpcReply {
    Hello { version: u16, snapshot: SystemSnapshot },
    Snapshot { snapshot: SystemSnapshot },
    Subscriptions { topics: Subscriptions, snapshot: SystemSnapshot },
    Applied { snapshot: SystemSnapshot },
    Config { config: AppConfig, revision: u64 },
    SystemTools { startup: StartupStatus, oem: OemInventory },
    AutomationRules { rules: Vec<AutomationRuleView>, summary: String },
    OsdShown {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: ErrorCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    IncompatibleVersion,
    AccessDenied,
    NotReady,
    Unsupported,
    ReadOnlyFirmware,
    DeniedWrite,
    WriteNotAllowed,
    ValidationFailed,
    RevisionConflict,
    RequestExpired,
    ConflictDetected,
    Busy,
    TimedOut,
    BackendUnavailable,
    WriteFailed,
    VerificationFailed,
    RollbackFailed,
    RecoveryRequired,
    Internal,
}
str_enum_serde!(ErrorCode, "error code",
    InvalidRequest => "invalidRequest",
    IncompatibleVersion => "incompatibleVersion",
    AccessDenied => "accessDenied",
    NotReady => "notReady",
    Unsupported => "unsupported",
    ReadOnlyFirmware => "readOnlyFirmware",
    DeniedWrite => "deniedWrite",
    WriteNotAllowed => "writeNotAllowed",
    ValidationFailed => "validationFailed",
    RevisionConflict => "revisionConflict",
    RequestExpired => "requestExpired",
    ConflictDetected => "conflictDetected",
    Busy => "busy",
    TimedOut => "timedOut",
    BackendUnavailable => "backendUnavailable",
    WriteFailed => "writeFailed",
    VerificationFailed => "verificationFailed",
    RollbackFailed => "rollbackFailed",
    RecoveryRequired => "recoveryRequired",
    Internal => "internal",
);

impl From<ControlError> for ErrorCode {
    fn from(error: ControlError) -> Self {
        match error {
            ControlError::UnsupportedValue(_) => Self::Unsupported,
            ControlError::InvalidField(_) | ControlError::FanCurve(_) => Self::ValidationFailed,
        }
    }
}

impl From<HalError> for ErrorCode {
    fn from(error: HalError) -> Self {
        match error {
            HalError::Unsupported => Self::Unsupported,
            HalError::Busy => Self::Busy,
            HalError::TimedOut => Self::TimedOut,
            HalError::Unavailable | HalError::Io | HalError::NativeIo(_) => Self::BackendUnavailable,
        }
    }
}

impl From<&WriteGuardError> for ErrorCode {
    fn from(error: &WriteGuardError) -> Self {
        match error {
            WriteGuardError::ReadOnlyFirmware => Self::ReadOnlyFirmware,
            WriteGuardError::RecoveryRequired => Self::RecoveryRequired,
            WriteGuardError::DeniedWrite { .. } => Self::DeniedWrite,
            WriteGuardError::WriteNotAllowed { .. } => Self::WriteNotAllowed,
            WriteGuardError::UnsupportedFan { .. } => Self::Unsupported,
            WriteGuardError::InvalidFanCurve(_) => Self::ValidationFailed,
            WriteGuardError::UnsafeRegisterBaseline { .. }
            | WriteGuardError::UnsafeFanBaseline(_) => Self::RecoveryRequired,
            WriteGuardError::SnapshotFailed { source, .. } => Self::from(*source),
            WriteGuardError::Indeterminate { .. } => Self::TimedOut,
            WriteGuardError::ApplyFailed { failure: TransactionFailure::Write { .. } } => Self::WriteFailed,
            WriteGuardError::ApplyFailed { .. } => Self::VerificationFailed,
            WriteGuardError::RollbackFailed { .. } => Self::RollbackFailed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum IpcEvent {
    StateChanged { snapshot: SystemSnapshot },
    TelemetryTick { sample: TelemetrySample },
    OsdNotify { revision: u64, kind: OsdKind },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "camelCase", deny_unknown_fields)]
pub enum IpcMessage {
    Request(IpcRequest),
    Response(IpcResponse),
    Event(IpcEvent),
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("invalid IPC payload length: {0}")]
    InvalidLength(u32),
    #[error("encoded IPC payload exceeds the length limit")]
    TooLarge,
    #[error("invalid IPC JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode_frame(message: &IpcMessage) -> Result<Vec<u8>, FrameError> {
    let mut output = Vec::with_capacity(256);
    encode_frame_into(message, &mut output)?;
    Ok(output)
}

pub fn encode_frame_into(message: &IpcMessage, output: &mut Vec<u8>) -> Result<(), FrameError> {
    output.clear();
    output.extend_from_slice(&[0; FRAME_HEADER_LEN]);
    let mut writer = FrameWriter { output, overflow: false };
    if let Err(error) = serde_json::to_writer(&mut writer, message) {
        writer.output.clear();
        return Err(if writer.overflow { FrameError::TooLarge } else { FrameError::Json(error) });
    }
    let length = (writer.output.len() - FRAME_HEADER_LEN) as u32;
    writer.output[..FRAME_HEADER_LEN].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

struct FrameWriter<'a> {
    output: &'a mut Vec<u8>,
    overflow: bool,
}

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_PAYLOAD + FRAME_HEADER_LEN - self.output.len() {
            self.overflow = true;
            return Err(io::Error::other("IPC frame too large"));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decodes only the first frame. `None` means retain the bytes and await more input.
pub fn decode_frame(input: &[u8]) -> Result<Option<(IpcMessage, usize)>, FrameError> {
    let Some((body, end)) = frame_payload(input)? else { return Ok(None); };
    Ok(Some((serde_json::from_slice(body)?, end)))
}

/// Daemon-to-client frames exclude requests, keeping unused command decoders out of one-shot clients.
pub fn decode_server_frame(input: &[u8]) -> Result<Option<(IpcMessage, usize)>, FrameError> {
    #[derive(Deserialize)]
    #[serde(tag = "type", content = "message", rename_all = "camelCase", deny_unknown_fields)]
    enum ServerMessage {
        Response(IpcResponse),
        Event(IpcEvent),
    }
    let Some((body, end)) = frame_payload(input)? else { return Ok(None); };
    let message = match serde_json::from_slice(body)? {
        ServerMessage::Response(response) => IpcMessage::Response(response),
        ServerMessage::Event(event) => IpcMessage::Event(event),
    };
    Ok(Some((message, end)))
}

fn frame_payload(input: &[u8]) -> Result<Option<(&[u8], usize)>, FrameError> {
    let Some(header) = input.first_chunk::<FRAME_HEADER_LEN>() else {
        return Ok(None);
    };
    let length = u32::from_le_bytes(*header);
    if length == 0 || length as usize > MAX_FRAME_PAYLOAD {
        return Err(FrameError::InvalidLength(length));
    }
    let end = FRAME_HEADER_LEN + length as usize;
    let Some(body) = input.get(FRAME_HEADER_LEN..end) else {
        return Ok(None);
    };
    Ok(Some((body, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HardwareState, PerformanceMode};

    fn hello() -> IpcMessage {
        IpcMessage::Request(IpcRequest {
            id: 1,
            ttl_ms: None,
            command: IpcCommand::Hello { version: IPC_VERSION, client: ClientKind::Ui },
        })
    }

    fn frame_bytes(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn wire_shape_and_length_are_stable() {
        let payload = br#"{"type":"request","message":{"id":1,"command":{"op":"hello","version":1,"client":"ui"}}}"#;
        assert_eq!(encode_frame(&hello()).unwrap(), frame_bytes(payload));
        let change = IpcMessage::Request(IpcRequest {
            id: 3,
            ttl_ms: None,
            command: IpcCommand::Control {
                expected_revision: 12,
                change: ControlChange::SetPerformanceMode { mode: PerformanceMode::Balanced },
            },
        });
        assert_eq!(encode_frame(&change).unwrap(), frame_bytes(br#"{"type":"request","message":{"id":3,"command":{"op":"control","expectedRevision":12,"change":{"kind":"setPerformanceMode","mode":"balanced"}}}}"#));
    }

    #[test]
    fn decodes_partial_and_coalesced_frames() {
        let first = encode_frame(&hello()).unwrap();
        for cut in 0..first.len() {
            assert!(decode_frame(&first[..cut]).unwrap().is_none());
        }
        let event = IpcMessage::Event(IpcEvent::StateChanged { snapshot: SystemSnapshot::default() });
        let second = encode_frame(&event).unwrap();
        let joined = [first.as_slice(), second.as_slice()].concat();
        assert_eq!(decode_frame(&joined).unwrap(), Some((hello(), first.len())));
        assert_eq!(decode_frame(&joined[first.len()..]).unwrap(), Some((event, second.len())));
    }

    #[test]
    fn server_decoder_validates_pushes_and_rejects_requests() {
        assert!(matches!(decode_server_frame(&encode_frame(&hello()).unwrap()), Err(FrameError::Json(_))));
        for message in [
            IpcMessage::Response(IpcResponse { id: 2, result: IpcResult::Ok { reply: IpcReply::Snapshot { snapshot: SystemSnapshot::default() } } }),
            IpcMessage::Event(IpcEvent::TelemetryTick { sample: TelemetrySample { gpu_clock_mhz: Some(1200), ..Default::default() } }),
        ] {
            let frame = encode_frame(&message).unwrap();
            for cut in 0..frame.len() {
                assert!(decode_server_frame(&frame[..cut]).unwrap().is_none());
            }
            assert_eq!(decode_server_frame(&frame).unwrap(), Some((message, frame.len())));
        }
        for length in [0, (MAX_FRAME_PAYLOAD + 1) as u32] {
            assert!(matches!(decode_server_frame(&length.to_le_bytes()), Err(FrameError::InvalidLength(_))));
        }
    }

    #[test]
    fn round_trips_error_and_push_messages() {
        let messages = [
            IpcMessage::Response(IpcResponse {
                id: 8,
                result: IpcResult::Error { error: IpcError { code: ErrorCode::ReadOnlyFirmware } },
            }),
            IpcMessage::Event(IpcEvent::TelemetryTick {
                sample: TelemetrySample { sampled_at_unix_ms: 100, cpu_temperature_c: Some(47), ..TelemetrySample::default() },
            }),
            IpcMessage::Event(IpcEvent::OsdNotify { revision: 9, kind: OsdKind::CoolerBoost }),
        ];
        let mut scratch = Vec::new();
        for message in messages {
            encode_frame_into(&message, &mut scratch).unwrap();
            assert_eq!(decode_frame(&scratch).unwrap(), Some((message, scratch.len())));
        }
    }

    #[test]
    fn rejects_invalid_lengths_before_waiting_for_payload() {
        for length in [0, (MAX_FRAME_PAYLOAD + 1) as u32, u32::MAX] {
            assert!(matches!(decode_frame(&length.to_le_bytes()), Err(FrameError::InvalidLength(n)) if n == length));
        }
        assert!(decode_frame(&(MAX_FRAME_PAYLOAD as u32).to_le_bytes()).unwrap().is_none());
        let mut payload = serde_json::to_vec(&hello()).unwrap();
        payload.resize(MAX_FRAME_PAYLOAD, b' ');
        let frame = frame_bytes(&payload);
        assert_eq!(decode_frame(&frame).unwrap(), Some((hello(), frame.len())));
    }

    #[test]
    fn rejects_malformed_and_unauthorized_message_shapes() {
        for payload in [
            b"{not-json".as_slice(),
            b"\xff",
            br#"{"type":"request","message":{"id":1,"command":{"op":"writeRegister","address":81,"value":9}}}"#,
            br#"{"type":"request","message":{"id":1,"id":2,"command":{"op":"getSnapshot"}}}"#,
            br#"{"type":"request","message":{"id":1,"command":{"op":"getSnapshot","extra":true}}}"#,
            br#"{"type":"request","message":{"id":1,"command":{"op":"hello","version":1,"client":"ui","admin":true}}}"#,
        ] {
            assert!(matches!(decode_frame(&frame_bytes(payload)), Err(FrameError::Json(_))));
        }
    }

    #[test]
    fn oversized_output_leaves_no_partial_frame() {
        let message = IpcMessage::Event(IpcEvent::StateChanged {
            snapshot: SystemSnapshot {
                hardware: HardwareState { color_profile_id: Some("p".repeat(MAX_FRAME_PAYLOAD)), ..HardwareState::default() },
                ..SystemSnapshot::default()
            },
        });
        let mut output = encode_frame(&hello()).unwrap();
        assert!(matches!(encode_frame_into(&message, &mut output), Err(FrameError::TooLarge)));
        assert!(output.is_empty());
    }

    #[test]
    fn cli_cannot_enable_periodic_telemetry() {
        let topics = Subscriptions { telemetry: true, ..Subscriptions::default() };
        assert_eq!(topics.validate_for(ClientKind::Ui), Ok(()));
        assert_eq!(topics.validate_for(ClientKind::Cli), Err(ErrorCode::Unsupported));
        assert_eq!(Subscriptions::default().validate_for(ClientKind::Cli), Ok(()));
        let topics = Subscriptions { gpu_telemetry: true, ..Subscriptions::default() };
        assert_eq!(topics.validate_for(ClientKind::Ui), Ok(()));
        assert_eq!(topics.validate_for(ClientKind::Cli), Err(ErrorCode::Unsupported));
    }

    #[test]
    fn gpu_subscription_is_an_optional_wire_extension() {
        let legacy = r#"{"state":true,"telemetry":true,"osd":false}"#;
        let mut topics: Subscriptions = serde_json::from_str(legacy).unwrap();
        assert!(!topics.gpu_telemetry);
        assert_eq!(serde_json::to_string(&topics).unwrap(), legacy);
        topics.gpu_telemetry = true;
        let json = serde_json::to_string(&topics).unwrap();
        assert_eq!(json, r#"{"state":true,"telemetry":true,"osd":false,"gpuTelemetry":true}"#);
        assert_eq!(serde_json::from_str::<Subscriptions>(&json).unwrap(), topics);
    }

    #[test]
    fn gpu_clock_offset_control_and_target_wire_names_are_stable() {
        let control = IpcMessage::Request(IpcRequest {
            id: 9, ttl_ms: None,
            command: IpcCommand::Control {
                expected_revision: 12,
                change: ControlChange::SetGpuClockOffset { core_mhz: 100, mem_mhz: 200 },
            },
        });
        let expected = frame_bytes(br#"{"type":"request","message":{"id":9,"command":{"op":"control","expectedRevision":12,"change":{"kind":"setGpuClockOffset","coreMhz":100,"memMhz":200}}}}"#);
        assert_eq!(encode_frame(&control).unwrap(), expected);
        assert_eq!(decode_frame(&expected).unwrap(), Some((control, expected.len())));
        assert_eq!(serde_json::to_string(&ControlTarget::GpuClockOffset {}).unwrap(), r#"{"kind":"gpuClockOffset"}"#);
        assert!(serde_json::from_str::<ControlChange>(r#"{"kind":"setGpuClockOffset","coreMhz":100,"memMhz":200,"pstate":1}"#).is_err());
    }

    #[test]
    fn queue_ttl_and_release_wire_shape() {
        let mut request = IpcRequest {
            id: 4,
            ttl_ms: Some(700),
            command: IpcCommand::ReleaseManualOverride {
                expected_revision: 12,
                target: ControlTarget::FanCurve { channel: crate::FanChannel::Cpu },
            },
        };
        let payload = br#"{"type":"request","message":{"id":4,"ttlMs":700,"command":{"op":"releaseManualOverride","expectedRevision":12,"target":{"kind":"fanCurve","channel":"cpu"}}}}"#;
        assert_eq!(encode_frame(&IpcMessage::Request(request.clone())).unwrap(), frame_bytes(payload));
        assert_eq!(request.queue_ttl_ms(), Ok(700));
        for ttl in [0, MAX_QUEUE_TTL_MS + 1] {
            request.ttl_ms = Some(ttl);
            assert_eq!(request.queue_ttl_ms(), Err(ErrorCode::InvalidRequest));
        }
        request.ttl_ms = None;
        assert_eq!(request.queue_ttl_ms(), Ok(DEFAULT_QUEUE_TTL_MS));
        request.id = 0;
        assert_eq!(request.queue_ttl_ms(), Err(ErrorCode::InvalidRequest));
        let reset = IpcMessage::Request(IpcRequest {
            id: 5,
            ttl_ms: None,
            command: IpcCommand::ResetFanMode { expected_revision: 12 },
        });
        assert_eq!(
            encode_frame(&reset).unwrap(),
            frame_bytes(br#"{"type":"request","message":{"id":5,"command":{"op":"resetFanMode","expectedRevision":12}}}"#)
        );
    }
}
