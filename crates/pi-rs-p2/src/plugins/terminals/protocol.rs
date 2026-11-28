//! Length-prefixed terminal attach protocol framing (apps/p2/plugins/terminals/protocol.ts).
//! Frame format: `[u32 BE length][u8 kind (0 = JSON, 1 = binary)][payload...]`.

use pi_rs_chord::{Error, Result};
use serde_json::Value;

pub const MAX_FRAME: usize = 8 * 1024 * 1024;
const FRAME_KIND_JSON: u8 = 0;
const FRAME_KIND_BINARY: u8 = 1;

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalFrame {
    Json(Value),
    Binary(Vec<u8>),
}

/// Encodes one JSON control frame or raw binary payload with a 4-byte BE length header.
pub fn encode_frame(frame: &TerminalFrame) -> Result<Vec<u8>> {
    let (kind, payload) = match frame {
        TerminalFrame::Json(value) => (FRAME_KIND_JSON, serde_json::to_vec(value)?),
        TerminalFrame::Binary(bytes) => (FRAME_KIND_BINARY, bytes.clone()),
    };
    let body_len = 1usize
        .checked_add(payload.len())
        .ok_or_else(|| Error::Invalid("Terminal frame length overflow".into()))?;
    if body_len > MAX_FRAME {
        return Err(Error::Invalid("Terminal frame too large".into()));
    }
    let len_u32 =
        u32::try_from(body_len).map_err(|_| Error::Invalid("Terminal frame too large".into()))?;
    let mut out = Vec::with_capacity(4 + body_len);
    out.extend_from_slice(&len_u32.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Incremental linear-time terminal frame decoder: keeps incomplete chunk slices until the full
/// declared frame arrives, coalesces at most once per frame boundary, and drains pending state
/// before invoking callbacks.
pub struct FrameDecoder {
    pending: Vec<Vec<u8>>,
    pending_bytes: usize,
    needed: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            pending_bytes: 0,
            needed: 4,
        }
    }

    pub fn push(
        &mut self,
        chunk: &[u8],
        mut on_frame: impl FnMut(TerminalFrame) -> Result<()>,
    ) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.pending.push(chunk.to_vec());
        self.pending_bytes += chunk.len();
        if self.pending_bytes < self.needed {
            return Ok(());
        }
        let mut buffer = if self.pending.len() == 1 {
            self.pending.pop().unwrap_or_default()
        } else {
            let mut joined = Vec::with_capacity(self.pending_bytes);
            for part in self.pending.drain(..) {
                joined.extend_from_slice(&part);
            }
            joined
        };
        // Reset before delivering frames so a failing callback never leaves stale frames queued.
        self.pending.clear();
        self.pending_bytes = 0;
        self.needed = 4;

        let mut cursor = 0usize;
        while buffer.len() - cursor >= 4 {
            let length = u32::from_be_bytes([
                buffer[cursor],
                buffer[cursor + 1],
                buffer[cursor + 2],
                buffer[cursor + 3],
            ]) as usize;
            if !(1..=MAX_FRAME).contains(&length) {
                return Err(Error::Invalid("Corrupt terminal frame".into()));
            }
            if buffer.len() - cursor < 4 + length {
                self.needed = 4 + length;
                break;
            }
            let kind = buffer[cursor + 4];
            let body = &buffer[cursor + 5..cursor + 4 + length];
            cursor += 4 + length;
            let frame = match kind {
                FRAME_KIND_JSON => {
                    let parsed: Value = serde_json::from_slice(body)
                        .map_err(|_| Error::Invalid("Corrupt terminal frame JSON".into()))?;
                    TerminalFrame::Json(parsed)
                }
                FRAME_KIND_BINARY => TerminalFrame::Binary(body.to_vec()),
                _ => return Err(Error::Invalid("Unknown terminal frame kind".into())),
            };
            on_frame(frame)?;
        }
        if cursor < buffer.len() {
            let tail = buffer.split_off(cursor);
            self.pending_bytes = tail.len();
            self.pending.push(tail);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_fragmented_and_coalesced_json_and_binary_frames() -> Result<()> {
        let mut wire = encode_frame(&TerminalFrame::Json(json!({"type": "ready", "cols": 80})))?;
        wire.extend_from_slice(&encode_frame(&TerminalFrame::Binary(
            b"hello pty\n".to_vec(),
        ))?);

        let mut decoder = FrameDecoder::new();
        let mut received = Vec::new();
        for byte in &wire {
            decoder.push(std::slice::from_ref(byte), |frame| {
                received.push(frame);
                Ok(())
            })?;
        }
        assert_eq!(
            received,
            vec![
                TerminalFrame::Json(json!({"type": "ready", "cols": 80})),
                TerminalFrame::Binary(b"hello pty\n".to_vec()),
            ]
        );
        Ok(())
    }

    #[test]
    fn rejects_zero_and_oversized_frames_and_clears_pending_before_callback_failure() -> Result<()>
    {
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&0u32.to_be_bytes(), |_| Ok(())).is_err());

        let mut decoder = FrameDecoder::new();
        let too_big = u32::try_from(MAX_FRAME + 1)
            .map_err(|_| Error::Invalid("overflow".into()))?
            .to_be_bytes();
        assert!(decoder.push(&too_big, |_| Ok(())).is_err());

        let mut two = encode_frame(&TerminalFrame::Binary(b"first".to_vec()))?;
        two.extend_from_slice(&encode_frame(&TerminalFrame::Binary(b"second".to_vec()))?);
        let mut decoder = FrameDecoder::new();
        let mut count = 0;
        let result = decoder.push(&two, |_| {
            count += 1;
            Err(Error::Invalid("boom".into()))
        });
        assert!(result.is_err());
        assert_eq!(count, 1);
        // Subsequent valid frame is decoded cleanly without redelivering "second".
        let mut next = Vec::new();
        decoder.push(
            &encode_frame(&TerminalFrame::Binary(b"fresh".to_vec()))?,
            |f| {
                next.push(f);
                Ok(())
            },
        )?;
        assert_eq!(next, vec![TerminalFrame::Binary(b"fresh".to_vec())]);
        Ok(())
    }

    #[test]
    fn decodes_large_frame_in_small_chunks_without_quadratic_copy() -> Result<()> {
        let payload = vec![b'a'; 1024 * 1024];
        let wire = encode_frame(&TerminalFrame::Binary(payload.clone()))?;
        let mut decoder = FrameDecoder::new();
        let mut received = Vec::new();
        for piece in wire.chunks(512) {
            decoder.push(piece, |frame| {
                received.push(frame);
                Ok(())
            })?;
        }
        assert_eq!(received, vec![TerminalFrame::Binary(payload)]);
        Ok(())
    }
}
