//! Wire protocol shared by the Icarus server, CLI and client.
//!
//! Frames are a 4-byte big-endian length followed by a UTF-8 JSON document.

pub mod layout;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: u32,
        #[serde(default)]
        token: String,
        client_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pair_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_label: Option<String>,
    },
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        version: u32,
        server_version: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paired_client_id: Option<String>,
    },
    Response {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Event {
        topic: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(body.len()))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Splits complete frames off the front of `buffer`, leaving any partial frame in place.
pub fn decode_frames<T: for<'de> Deserialize<'de>>(
    buffer: &mut Vec<u8>,
) -> Result<Vec<T>, FrameError> {
    let mut messages = Vec::new();
    loop {
        if buffer.len() < 4 {
            break;
        }
        let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge(len));
        }
        if buffer.len() < 4 + len {
            break;
        }
        let message = serde_json::from_slice(&buffer[4..4 + len])?;
        buffer.drain(..4 + len);
        messages.push(message);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_and_keep_partial_tails() {
        let request = ClientMessage::Request {
            id: 7,
            method: "server.info".into(),
            params: json!({}),
        };
        let mut stream = encode_frame(&request).expect("encode");
        stream.extend(encode_frame(&request).expect("encode"));
        let tail = encode_frame(&request).expect("encode");
        stream.extend_from_slice(&tail[..5]);

        let decoded: Vec<ClientMessage> = decode_frames(&mut stream).expect("decode");
        assert_eq!(decoded, vec![request.clone(), request]);
        assert_eq!(stream, tail[..5].to_vec());
    }

    #[test]
    fn hello_without_pairing_fields_matches_the_original_shape() {
        let old = json!({"type": "hello", "version": 1, "token": "t", "client_name": "c"});
        let hello: ClientMessage = serde_json::from_value(old.clone()).expect("decode");
        assert_eq!(serde_json::to_value(&hello).expect("encode"), old);
        let pairing: ClientMessage = serde_json::from_value(
            json!({"type": "hello", "version": 1, "client_name": "c", "pair_code": "ABCD2345"}),
        )
        .expect("decode");
        assert!(matches!(
            pairing,
            ClientMessage::Hello { ref token, pair_code: Some(_), .. } if token.is_empty()
        ));
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut stream = u32::MAX.to_be_bytes().to_vec();
        let result: Result<Vec<ClientMessage>, _> = decode_frames(&mut stream);
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
    }
}
