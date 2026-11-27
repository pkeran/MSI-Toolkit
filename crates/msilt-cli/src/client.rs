use std::fmt;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use msilt_core::ipc::{
    ClientKind, FRAME_HEADER_LEN, FrameError, IPC_VERSION, IpcCommand, IpcMessage, IpcReply,
    IpcResponse, IpcResult, decode_server_frame,
};
use msilt_core::{ControlChange, ControlTarget, FanChannel, FanCurve, FanPoint};

use crate::args::Action;

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub trait Transport: Read + Write {
    fn set_deadline(&mut self, deadline: Instant);
}

#[derive(Debug)]
pub struct ClientError {
    phase: &'static str,
    cause: Failure,
    outcome_unknown: bool,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.phase)?;
        match &self.cause {
            Failure::Io(error) => write!(f, "{error}")?,
            Failure::Frame(FrameError::Json(_)) => f.write_str("invalid IPC JSON")?,
            Failure::Frame(error) => write!(f, "{error}")?,
            Failure::Protocol(message) => f.write_str(message)?,
        }
        if self.outcome_unknown {
            f.write_str("; outcome unknown, run status before retrying")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Failure {
    Io(io::Error),
    Frame(FrameError),
    Protocol(&'static str),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameError> for Failure {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

#[cfg(test)]
fn execute(io: impl Transport, action: Action) -> Result<IpcResponse, ClientError> {
    execute_with_frame(io, action).map(|(reply, _)| reply)
}

pub fn execute_with_frame(
    io: impl Transport,
    action: Action,
) -> Result<(IpcResponse, Vec<u8>), ClientError> {
    let mut client = Client {
        io,
        frame: Vec::with_capacity(512),
    };
    let hello = client
        .request(
            1,
            IpcCommand::Hello {
                version: IPC_VERSION,
                client: ClientKind::Cli,
            },
            HELLO_TIMEOUT,
        )
        .map_err(handshake_error)?;
    let snapshot = match &hello.result {
        IpcResult::Ok {
            reply: IpcReply::Hello { version, snapshot },
        } if *version == IPC_VERSION => snapshot,
        IpcResult::Ok {
            reply: IpcReply::Hello { .. },
        } => {
            return Err(handshake_error(Failure::Protocol(
                "incompatible daemon protocol version",
            )));
        }
        IpcResult::Ok { .. } => {
            return Err(handshake_error(Failure::Protocol("expected a hello reply")));
        }
        IpcResult::Error { .. } => return Ok((hello, client.frame)),
    };
    let revision = snapshot.revision;
    let (command, expected) = match action {
        Action::Status => (IpcCommand::GetSnapshot {}, ReplyKind::Snapshot),
        Action::ConfigGet => (IpcCommand::GetConfig {}, ReplyKind::Config),
        Action::FanAuto => (
            IpcCommand::ResetFanMode {
                expected_revision: revision,
            },
            ReplyKind::Applied,
        ),
        Action::FanCurve { channel, duties } => {
            let state = match channel {
                FanChannel::Cpu => snapshot.hardware.cpu_fan.as_ref(),
                FanChannel::Gpu => snapshot.hardware.gpu_fan.as_ref(),
            }
            .ok_or_else(|| ClientError {
                phase: "fan curve unavailable",
                cause: Failure::Protocol("daemon snapshot has no fan curve for that channel"),
                outcome_unknown: false,
            })?;
            let mut points = state.curve.points;
            points[0].duty_percent = 0;
            for (slot, duty) in points[1..].iter_mut().zip(duties) {
                slot.duty_percent = duty;
            }
            let curve = FanCurve { points };
            if let Some(limits) = state.limits.as_ref() {
                if curve.validate_with_limits(limits).is_err() {
                    return Err(ClientError {
                        phase: "fan curve rejected",
                        cause: Failure::Protocol(
                            "duties violate factory safety floor, monotonic order, or max duty",
                        ),
                        outcome_unknown: false,
                    });
                }
            }
            (
                IpcCommand::Control {
                    expected_revision: revision,
                    change: ControlChange::SetFanCurve { channel, curve },
                },
                ReplyKind::Applied,
            )
        }
        Action::Control(change) => (
            IpcCommand::Control {
                expected_revision: revision,
                change,
            },
            ReplyKind::Applied,
        ),
        Action::Release(target) => (
            IpcCommand::ReleaseManualOverride {
                expected_revision: revision,
                target,
            },
            ReplyKind::Applied,
        ),
    };
    let response = client
        .request(2, command, COMMAND_TIMEOUT)
        .and_then(|response| match (&response.result, expected) {
            (IpcResult::Error { .. }, _)
            | (
                IpcResult::Ok {
                    reply: IpcReply::Snapshot { .. },
                },
                ReplyKind::Snapshot,
            )
            | (
                IpcResult::Ok {
                    reply: IpcReply::Config { .. },
                },
                ReplyKind::Config,
            )
            | (
                IpcResult::Ok {
                    reply: IpcReply::Applied { .. },
                },
                ReplyKind::Applied,
            ) => Ok(response),
            _ => Err(Failure::Protocol("unexpected command reply kind")),
        })
        .map_err(|cause| ClientError {
            phase: "IPC command failed",
            cause,
            outcome_unknown: matches!(expected, ReplyKind::Applied),
        })?;
    Ok((response, client.frame))
}

fn handshake_error(cause: Failure) -> ClientError {
    ClientError {
        phase: "daemon handshake failed",
        cause,
        outcome_unknown: false,
    }
}

#[derive(Clone, Copy)]
enum ReplyKind {
    Snapshot,
    Config,
    Applied,
}

struct Client<T> {
    io: T,
    frame: Vec<u8>,
}

impl<T: Transport> Client<T> {
    fn request(
        &mut self,
        id: u64,
        command: IpcCommand,
        timeout: Duration,
    ) -> Result<IpcResponse, Failure> {
        self.io.set_deadline(Instant::now() + timeout);
        encode_cli_request(id, &command, &mut self.frame)?;
        self.io.write_all(&self.frame)?;
        loop {
            match self.read_frame()? {
                IpcMessage::Response(response) if response.id == id => return Ok(response),
                IpcMessage::Response(_) => {
                    return Err(Failure::Protocol("response ID does not match the request"));
                }
                IpcMessage::Request(_) => {
                    return Err(Failure::Protocol("received a request from the daemon"));
                }
                IpcMessage::Event(_) => continue,
            }
        }
    }

    fn read_frame(&mut self) -> Result<IpcMessage, Failure> {
        let mut header = [0; FRAME_HEADER_LEN];
        self.io.read_exact(&mut header)?;
        decode_server_frame(&header)?; // Validate the length before growing the receive buffer.
        let len = u32::from_le_bytes(header) as usize;
        self.frame.resize(FRAME_HEADER_LEN + len, 0);
        self.frame[..FRAME_HEADER_LEN].copy_from_slice(&header);
        self.io.read_exact(&mut self.frame[FRAME_HEADER_LEN..])?;
        match decode_server_frame(&self.frame)? {
            Some((message, _)) => Ok(message),
            None => Err(Failure::Protocol("incomplete IPC frame")),
        }
    }
}

fn encode_cli_request(id: u64, command: &IpcCommand, out: &mut Vec<u8>) -> Result<(), Failure> {
    // Keep only CLI requests in the sender. Every interpolated string is a protocol
    // constant or a profile ID checked against the Core ASCII allowlist.
    out.clear();
    out.extend_from_slice(&[0; FRAME_HEADER_LEN]);
    write!(
        out,
        r#"{{"type":"request","message":{{"id":{id},"command":{{"#
    )?;
    match command {
        IpcCommand::Hello {
            version,
            client: ClientKind::Cli,
        } => write!(out, r#""op":"hello","version":{version},"client":"cli""#)?,
        IpcCommand::GetSnapshot {} => out.extend_from_slice(br#""op":"getSnapshot""#),
        IpcCommand::GetConfig {} => out.extend_from_slice(br#""op":"getConfig""#),
        IpcCommand::ResetFanMode { expected_revision } => write!(
            out,
            r#""op":"resetFanMode","expectedRevision":{expected_revision}"#
        )?,
        IpcCommand::ReleaseManualOverride {
            expected_revision,
            target,
        } => {
            write!(
                out,
                r#""op":"releaseManualOverride","expectedRevision":{expected_revision},"target":{{"#
            )?;
            let kind = match target {
                ControlTarget::PerformanceMode {} => "performanceMode",
                ControlTarget::FanCurve { channel } => {
                    let channel = if *channel == FanChannel::Cpu {
                        "cpu"
                    } else {
                        "gpu"
                    };
                    write!(out, r#""kind":"fanCurve","channel":"{channel}""#)?;
                    ""
                }
                ControlTarget::CoolerBoost {} => "coolerBoost",
                ControlTarget::ChargeLimit {} => "chargeLimit",
                ControlTarget::DisplayRefreshRate {} => "displayRefreshRate",
                ControlTarget::DisplayBrightness {} => "displayBrightness",
                ControlTarget::ColorProfile {} => "colorProfile",
                ControlTarget::KeyboardLighting {} => "keyboardLighting",
                ControlTarget::GpuClockOffset {} => "gpuClockOffset",
                _ => return Err(Failure::Protocol("unsupported CLI release target")),
            };
            if !kind.is_empty() {
                write!(out, r#""kind":"{kind}""#)?;
            }
            out.push(b'}');
        }
        IpcCommand::Control {
            expected_revision,
            change,
        } => {
            write!(
                out,
                r#""op":"control","expectedRevision":{expected_revision},"change":{{"#
            )?;
            match change {
                ControlChange::SetPerformanceMode { mode } => write!(
                    out,
                    r#""kind":"setPerformanceMode","mode":"{}""#,
                    mode.protocol_name()
                )?,
                ControlChange::SetFanCurve { channel, curve } => {
                    let ch = if *channel == FanChannel::Cpu { "cpu" } else { "gpu" };
                    write!(out, r#""kind":"setFanCurve","channel":"{ch}","curve":{{"points":["#)?;
                    for (index, FanPoint { temperature_c, duty_percent }) in curve.points.iter().enumerate() {
                        if index > 0 {
                            out.push(b',');
                        }
                        write!(
                            out,
                            r#"{{"temperatureC":{temperature_c},"dutyPercent":{duty_percent}}}"#
                        )?;
                    }
                    out.extend_from_slice(b"]}");
                }
                ControlChange::SetGpuClockOffset { core_mhz, mem_mhz } => write!(
                    out,
                    r#""kind":"setGpuClockOffset","coreMhz":{core_mhz},"memMhz":{mem_mhz}"#
                )?,
                ControlChange::SetCoolerBoost { enabled } => {
                    write!(out, r#""kind":"setCoolerBoost","enabled":{enabled}"#)?
                }
                ControlChange::SetChargeLimit { percent } => {
                    write!(out, r#""kind":"setChargeLimit","percent":{percent}"#)?
                }
                ControlChange::SetDisplayBrightness { percent } => {
                    write!(out, r#""kind":"setDisplayBrightness","percent":{percent}"#)?
                }
                ControlChange::SetDisplayRefreshRate { hz } => {
                    write!(out, r#""kind":"setDisplayRefreshRate","hz":{hz}"#)?
                }
                ControlChange::SetColorProfile { profile_id } => {
                    change
                        .validate_parameters()
                        .map_err(|_| Failure::Protocol("invalid color profile ID"))?;
                    write!(
                        out,
                        r#""kind":"setColorProfile","profileId":"{profile_id}""#
                    )?;
                }
                _ => return Err(Failure::Protocol("unsupported CLI control")),
            }
            out.push(b'}');
        }
        _ => return Err(Failure::Protocol("unsupported CLI request")),
    }
    out.extend_from_slice(b"}}}");
    let len = (out.len() - FRAME_HEADER_LEN) as u32;
    out[..FRAME_HEADER_LEN].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use msilt_core::ipc::{
        ErrorCode, IpcError, IpcEvent, IpcRequest, MAX_FRAME_PAYLOAD, decode_frame, encode_frame,
    };
    use msilt_core::{
        AppConfig, ControlChange, ControlTarget, FanChannel, PerformanceMode, SystemSnapshot,
    };

    #[test]
    fn cli_control_encoder_matches_frozen_v1_and_rejects_unescaped_input() {
        for change in [
            ControlChange::SetPerformanceMode {
                mode: PerformanceMode::SuperBattery,
            },
            ControlChange::SetCoolerBoost { enabled: false },
            ControlChange::SetChargeLimit { percent: 100 },
            ControlChange::SetDisplayBrightness { percent: 0 },
            ControlChange::SetDisplayRefreshRate { hz: 240 },
            ControlChange::SetColorProfile {
                profile_id: "native-6500K".into(),
            },
            ControlChange::SetGpuClockOffset {
                core_mhz: 150,
                mem_mhz: 200,
            },
            ControlChange::SetFanCurve {
                channel: FanChannel::Cpu,
                curve: FanCurve {
                    points: [
                        FanPoint { temperature_c: 0, duty_percent: 0 },
                        FanPoint { temperature_c: 48, duty_percent: 36 },
                        FanPoint { temperature_c: 56, duty_percent: 42 },
                        FanPoint { temperature_c: 62, duty_percent: 56 },
                        FanPoint { temperature_c: 70, duty_percent: 88 },
                        FanPoint { temperature_c: 78, duty_percent: 120 },
                    ],
                },
            },
        ] {
            let command = IpcCommand::Control {
                expected_revision: u64::MAX,
                change,
            };
            let mut out = Vec::new();
            encode_cli_request(2, &command, &mut out).unwrap();
            assert_eq!(
                out,
                encode_frame(&IpcMessage::Request(IpcRequest {
                    id: 2,
                    ttl_ms: None,
                    command
                }))
                .unwrap()
            );
        }
        assert!(
            encode_cli_request(
                2,
                &IpcCommand::Control {
                    expected_revision: 1,
                    change: ControlChange::SetColorProfile {
                        profile_id: "\"}".into()
                    }
                },
                &mut Vec::new()
            )
            .is_err()
        );
    }

    #[derive(Default)]
    struct MockPipe {
        incoming: Cursor<Vec<u8>>,
        sent: Vec<u8>,
        deadlines: Vec<Instant>,
    }

    impl Read for &mut MockPipe {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let n = output.len().min(3);
            self.incoming.read(&mut output[..n])
        }
    }

    impl Write for &mut MockPipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.sent.write(&bytes[..bytes.len().min(7)])
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for &mut MockPipe {
        fn set_deadline(&mut self, deadline: Instant) {
            self.deadlines.push(deadline);
        }
    }

    fn snapshot(revision: u64) -> SystemSnapshot {
        SystemSnapshot {
            revision,
            ..SystemSnapshot::default()
        }
    }

    fn response(id: u64, reply: IpcReply) -> IpcMessage {
        IpcMessage::Response(IpcResponse {
            id,
            result: IpcResult::Ok { reply },
        })
    }

    fn hello(version: u16) -> IpcMessage {
        response(
            1,
            IpcReply::Hello {
                version,
                snapshot: snapshot(42),
            },
        )
    }

    fn input(messages: &[IpcMessage]) -> MockPipe {
        let mut pipe = MockPipe::default();
        for message in messages {
            pipe.incoming
                .get_mut()
                .extend(encode_frame(message).unwrap());
        }
        pipe
    }

    fn assert_frames(pipe: &MockPipe, payloads: &[&[u8]]) {
        let mut input = pipe.sent.as_slice();
        for payload in payloads {
            let (message, len) = decode_frame(input).unwrap().unwrap();
            assert!(matches!(message, IpcMessage::Request(_)));
            assert_eq!(
                &input[..FRAME_HEADER_LEN],
                &(payload.len() as u32).to_le_bytes()
            );
            assert_eq!(&input[FRAME_HEADER_LEN..len], *payload);
            input = &input[len..];
        }
        assert!(input.is_empty());
    }

    const HELLO: &[u8] = br#"{"type":"request","message":{"id":1,"command":{"op":"hello","version":1,"client":"cli"}}}"#;

    #[test]
    fn control_uses_handshake_revision_and_returns_observed_snapshot() {
        let event = IpcMessage::Event(IpcEvent::StateChanged {
            snapshot: snapshot(41),
        });
        let final_reply = response(
            2,
            IpcReply::Applied {
                snapshot: snapshot(43),
            },
        );
        let mut pipe = input(&[hello(1), event, final_reply.clone()]);
        let change = ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance,
        };
        assert_eq!(
            IpcMessage::Response(execute(&mut pipe, Action::Control(change)).unwrap()),
            final_reply
        );
        assert_frames(&pipe, &[HELLO, br#"{"type":"request","message":{"id":2,"command":{"op":"control","expectedRevision":42,"change":{"kind":"setPerformanceMode","mode":"extremePerformance"}}}}"#]);
        assert_eq!(pipe.deadlines.len(), 2);
        assert!(pipe.deadlines[1] > pipe.deadlines[0]);
    }

    #[test]
    fn release_serializes_a_channel_target_and_does_not_replay_a_rejected_write() {
        let final_reply = IpcMessage::Response(IpcResponse {
            id: 2,
            result: IpcResult::Error {
                error: IpcError {
                    code: ErrorCode::RevisionConflict,
                },
            },
        });
        let mut pipe = input(&[hello(1), final_reply.clone()]);
        let target = ControlTarget::FanCurve {
            channel: FanChannel::Gpu,
        };
        assert_eq!(
            IpcMessage::Response(execute(&mut pipe, Action::Release(target)).unwrap()),
            final_reply
        );
        assert_frames(&pipe, &[HELLO, br#"{"type":"request","message":{"id":2,"command":{"op":"releaseManualOverride","expectedRevision":42,"target":{"kind":"fanCurve","channel":"gpu"}}}}"#]);
    }

    #[test]
    fn status_and_config_send_their_own_requests() {
        for (action, reply, payload) in [
            (
                Action::Status,
                IpcReply::Snapshot {
                    snapshot: snapshot(43),
                },
                br#"{"type":"request","message":{"id":2,"command":{"op":"getSnapshot"}}}"#
                    .as_slice(),
            ),
            (
                Action::ConfigGet,
                IpcReply::Config {
                    revision: 5,
                    config: AppConfig::default(),
                },
                br#"{"type":"request","message":{"id":2,"command":{"op":"getConfig"}}}"#.as_slice(),
            ),
        ] {
            let final_reply = response(2, reply);
            let mut pipe = input(&[hello(1), final_reply.clone()]);
            assert_eq!(
                IpcMessage::Response(execute(&mut pipe, action).unwrap()),
                final_reply
            );
            assert_frames(&pipe, &[HELLO, payload]);
        }
    }

    #[test]
    fn invalid_hello_prevents_the_command_from_being_sent() {
        for greeting in [
            hello(2),
            response(
                8,
                IpcReply::Hello {
                    version: IPC_VERSION,
                    snapshot: snapshot(42),
                },
            ),
            response(
                1,
                IpcReply::Snapshot {
                    snapshot: snapshot(42),
                },
            ),
            IpcMessage::Request(IpcRequest {
                id: 1,
                ttl_ms: None,
                command: IpcCommand::GetSnapshot {},
            }),
        ] {
            let mut pipe = input(&[greeting]);
            let error = execute(&mut pipe, Action::Status).err().unwrap();
            assert_eq!(error.phase, "daemon handshake failed");
            assert!(!error.outcome_unknown);
            assert_frames(&pipe, &[HELLO]);
        }
    }

    #[test]
    fn rejected_hello_returns_the_daemon_error_without_sending_the_command() {
        let denied = IpcMessage::Response(IpcResponse {
            id: 1,
            result: IpcResult::Error {
                error: IpcError {
                    code: ErrorCode::AccessDenied,
                },
            },
        });
        let mut pipe = input(&[denied.clone()]);
        assert_eq!(
            IpcMessage::Response(execute(&mut pipe, Action::Status).unwrap()),
            denied
        );
        assert_frames(&pipe, &[HELLO]);
    }

    #[test]
    fn frame_reader_checks_lengths_before_allocating_and_reports_truncation() {
        for len in [0, (MAX_FRAME_PAYLOAD + 1) as u32] {
            let mut pipe = MockPipe {
                incoming: Cursor::new(len.to_le_bytes().to_vec()),
                ..MockPipe::default()
            };
            let mut client = Client {
                io: &mut pipe,
                frame: Vec::new(),
            };
            assert!(
                matches!(client.read_frame(), Err(Failure::Frame(FrameError::InvalidLength(n))) if n == len)
            );
            assert!(client.frame.is_empty());
            assert_eq!(client.frame.capacity(), 0);
        }
        let mut pipe = input(&[hello(1)]);
        pipe.incoming.get_mut().pop();
        let error = execute(&mut pipe, Action::Status).err().unwrap();
        assert!(matches!(error.cause, Failure::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn lost_or_wrong_write_reply_is_indeterminate() {
        for mut pipe in [
            input(&[hello(1)]),
            input(&[
                hello(1),
                response(
                    2,
                    IpcReply::Snapshot {
                        snapshot: snapshot(43),
                    },
                ),
            ]),
        ] {
            let error = execute(
                &mut pipe,
                Action::Control(ControlChange::SetCoolerBoost { enabled: true }),
            )
            .err()
            .unwrap();
            assert!(error.outcome_unknown);
            assert!(error.to_string().contains("run status before retrying"));
        }
    }
}
