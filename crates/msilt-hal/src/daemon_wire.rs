//! Directional IPC v1 framing for the native pipe host.

use std::io::{self, Write};

use msilt_core::{
    FRAME_HEADER_LEN, FrameError, IpcEvent, IpcRequest, IpcResponse, MAX_FRAME_PAYLOAD,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(
    tag = "type",
    content = "message",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum Incoming {
    Request(IpcRequest),
}

#[derive(Serialize)]
#[serde(tag = "type", content = "message", rename_all = "camelCase")]
pub enum Outgoing<'a> {
    Response(&'a IpcResponse),
    Event(&'a IpcEvent),
}

pub fn decode_request(input: &[u8]) -> Result<Option<(IpcRequest, usize)>, FrameError> {
    let Some(header) = input.first_chunk::<FRAME_HEADER_LEN>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(*header);
    if len == 0 || len as usize > MAX_FRAME_PAYLOAD {
        return Err(FrameError::InvalidLength(len));
    }
    let end = FRAME_HEADER_LEN + len as usize;
    let Some(body) = input.get(FRAME_HEADER_LEN..end) else {
        return Ok(None);
    };
    let Incoming::Request(request) = serde_json::from_slice(body)?;
    Ok(Some((request, end)))
}

pub fn encode_reply(message: Outgoing<'_>, output: &mut Vec<u8>) -> Result<(), FrameError> {
    output.clear();
    output.extend_from_slice(&[0; FRAME_HEADER_LEN]);
    let mut writer = FrameWriter {
        output,
        overflow: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, &message) {
        writer.output.clear();
        return Err(if writer.overflow {
            FrameError::TooLarge
        } else {
            FrameError::Json(error)
        });
    }
    let len = (writer.output.len() - FRAME_HEADER_LEN) as u32;
    writer.output[..FRAME_HEADER_LEN].copy_from_slice(&len.to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use msilt_core::{IpcCommand, IpcMessage, IpcReply, IpcResult, encode_frame};

    #[test]
    fn directional_framing_matches_v1_at_the_pipe_boundary() {
        let request = IpcRequest {
            id: 12,
            ttl_ms: Some(700),
            command: IpcCommand::GetSystemTools {},
        };
        let encoded = encode_frame(&IpcMessage::Request(request.clone())).unwrap();
        for cut in 0..encoded.len() {
            assert!(decode_request(&encoded[..cut]).unwrap().is_none());
        }
        assert_eq!(
            decode_request(&encoded).unwrap(),
            Some((request, encoded.len()))
        );
        let response = IpcResponse {
            id: 12,
            result: IpcResult::Ok {
                reply: IpcReply::SystemTools {
                    startup: Default::default(),
                    oem: Default::default(),
                },
            },
        };
        let mut bounded = Vec::new();
        encode_reply(Outgoing::Response(&response), &mut bounded).unwrap();
        assert_eq!(
            bounded,
            encode_frame(&IpcMessage::Response(response)).unwrap()
        );
        assert!(decode_request(&bounded).is_err());
    }
}
