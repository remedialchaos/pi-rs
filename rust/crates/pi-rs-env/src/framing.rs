//! Binary framing shared by the client side of a `pi-env` connection (docs/protocol.md).
//!
//! A frame is `u32 length | u8 type | u32 id | u32 jsonLength | json | payload`, all integers big-endian.
//! The daemon owns an equivalent reader/writer; this module adds the incremental decode the client needs because
//! stdout arrives in arbitrary chunks.

use serde_json::Value;
use thiserror::Error;

pub const REQUEST: u8 = 1;
pub const RESULT: u8 = 2;
pub const ERROR: u8 = 3;
pub const EVENT: u8 = 4;
pub const CANCEL: u8 = 5;
pub const PING: u8 = 6;

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Largest payload a response carries, leaving room for its JSON.
pub const MAX_PAYLOAD: usize = MAX_FRAME - 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame too large")]
    TooLarge,
    #[error("frame length out of range")]
    BadLength,
    #[error("JSON length out of range")]
    BadJsonLength,
    #[error("invalid JSON in frame")]
    BadJson,
    #[error("unserializable JSON")]
    Unserializable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub id: u32,
    pub json: Value,
    pub payload: Vec<u8>,
}

/// Encode a frame. Refuses frames above `MAX_FRAME`, as the daemon does.
pub fn encode(kind: u8, id: u32, json: &Value, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(json).map_err(|_| FrameError::Unserializable)?;
    let length = 9 + body.len() + payload.len();
    if length > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(4 + length);
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Decode one frame from the front of `input`. `Ok(None)` means more bytes are needed.
pub fn decode(input: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
    if input.len() < 4 {
        return Ok(None);
    }
    let length = u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as usize;
    if !(9..=MAX_FRAME).contains(&length) {
        return Err(FrameError::BadLength);
    }
    if input.len() < 4 + length {
        return Ok(None);
    }
    let body = &input[4..4 + length];
    let json_length = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as usize;
    if 9 + json_length > length {
        return Err(FrameError::BadJsonLength);
    }
    let json: Value = if json_length == 0 {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_slice(&body[9..9 + json_length]).map_err(|_| FrameError::BadJson)?
    };
    Ok(Some((
        Frame {
            kind: body[0],
            id: u32::from_be_bytes([body[1], body[2], body[3], body[4]]),
            json,
            payload: body[9 + json_length..].to_vec(),
        },
        4 + length,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_kind_id_json_and_payload() {
        let bytes = encode(RESULT, 7, &json!({ "a": 1 }), &[0, 255, 10]).unwrap();
        let (frame, used) = decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!((frame.kind, frame.id), (RESULT, 7));
        assert_eq!(frame.json, json!({ "a": 1 }));
        assert_eq!(frame.payload, vec![0, 255, 10]);
    }

    #[test]
    fn empty_json_and_empty_payload_use_the_nine_byte_minimum() {
        let bytes = encode(PING, 0, &json!({}), &[]).unwrap();
        // Nine framing bytes plus the two bytes of `{}`.
        assert_eq!(bytes.len(), 15);
        assert_eq!(bytes[4], PING);
        let (frame, _) = decode(&bytes).unwrap().unwrap();
        assert_eq!(frame.json, json!({}));
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn waits_for_incomplete_frames_and_rejects_bad_bounds() {
        let bytes = encode(REQUEST, 3, &json!({ "op": "hi" }), &[1, 2, 3]).unwrap();
        for split in 1..bytes.len() {
            assert!(decode(&bytes[..split]).unwrap().is_none());
        }
        assert_eq!(decode(&[0, 0, 0, 8]).unwrap_err(), FrameError::BadLength);
        let mut oversize = bytes.clone();
        oversize[..4].copy_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        assert_eq!(decode(&oversize).unwrap_err(), FrameError::BadLength);
        let mut bad_json = bytes.clone();
        bad_json[9..13].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode(&bad_json).unwrap_err(), FrameError::BadJsonLength);
    }

    #[test]
    fn refuses_to_encode_a_frame_over_the_limit() {
        let payload = vec![0u8; MAX_FRAME];
        assert_eq!(
            encode(EVENT, 1, &json!({}), &payload).unwrap_err(),
            FrameError::TooLarge
        );
    }
}
