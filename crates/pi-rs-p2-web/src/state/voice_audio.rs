//! Voice room protocol, binary PCM16 framing, resampling, and VAD state machine
//! (upstream `apps/p2/plugins/voice/protocol.ts` + WebAudio streaming).

use serde::{Deserialize, Serialize};

pub const AGENT_AUDIO: u8 = 1;
pub const HUMAN_AUDIO: u8 = 2;
pub const HUMAN_RATE: u32 = 16_000;
pub const MIN_PAUSE_SECS: f64 = 0.5;
pub const MAX_PAUSE_SECS: f64 = 2.0;
pub const DEFAULT_PAUSE_SECS: f64 = 0.8;

/// Decoded binary WebSocket frame from `/voice/room`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomBinaryFrame {
    pub kind: u8,
    pub id: String,
    pub pcm: Vec<u8>,
}

/// Parses a binary `/voice/room` frame with 2-byte alignment padding (`2 + n + ((2 + n) & 1)`).
pub fn parse_room_frame(frame: &[u8]) -> Option<RoomBinaryFrame> {
    let kind = *frame.first()?;
    let length = usize::from(*frame.get(1)?);
    let header = 2 + length;
    let offset = header + (header & 1);
    if frame.len() < offset {
        return None;
    }
    let id = String::from_utf8_lossy(&frame[2..header]).into_owned();
    Some(RoomBinaryFrame {
        kind,
        id,
        pcm: frame[offset..].to_vec(),
    })
}

/// Encodes a binary `/voice/room` frame matching `parse_room_frame` padding rules.
pub fn encode_room_frame(kind: u8, id: &str, pcm: &[u8]) -> Vec<u8> {
    let id_bytes = id.as_bytes();
    let len = id_bytes.len().min(u8::MAX as usize);
    let header = 2 + len;
    let pad = header & 1;
    let mut out = Vec::with_capacity(header + pad + pcm.len());
    out.push(kind);
    out.push(len as u8);
    out.extend_from_slice(&id_bytes[..len]);
    if pad == 1 {
        out.push(0);
    }
    out.extend_from_slice(pcm);
    out
}

/// Server-to-browser JSON events received on `/voice/room`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoomEvent {
    Welcome {
        room: String,
        #[serde(rename = "micSampleRate", default)]
        mic_sample_rate: u32,
        #[serde(default)]
        members: Vec<RoomMember>,
    },
    MemberJoined {
        member: RoomMember,
    },
    MemberUpdated {
        member: RoomMember,
    },
    MemberLeft {
        member: RoomMember,
    },
    Config {
        engine: String,
        voice: String,
        speed: Option<f64>,
    },
    SpeakAck {
        id: String,
        duplicate: bool,
    },
    Vad {
        silence: f64,
    },
    TtsStart {
        id: String,
        engine: String,
        voice: String,
        #[serde(rename = "sampleRate")]
        sample_rate: u32,
    },
    TtsDone {
        id: String,
        cancelled: bool,
    },
    TtsError {
        id: String,
        code: String,
        message: String,
    },
    Talking {
        participant: String,
        on: bool,
    },
    SpeechStart,
    SpeechEnd,
    Interim {
        text: String,
        ms: u64,
    },
    Final {
        text: String,
        ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomMember {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub connections: u32,
}

/// Transition reported by [`VadDetector`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadTransition {
    None,
    SpeechStart,
    SpeechEnd,
}

/// Simple energy-based Voice Activity Detector for browser mic frames.
#[derive(Clone, Debug)]
pub struct VadDetector {
    pub threshold: f32,
    pub silence_ms: u64,
    speaking: bool,
    silence_accum_ms: u64,
}

impl Default for VadDetector {
    fn default() -> Self {
        Self::new(0.015, (DEFAULT_PAUSE_SECS * 1000.0) as u64)
    }
}

impl VadDetector {
    pub fn new(threshold: f32, silence_ms: u64) -> Self {
        Self {
            threshold: threshold.max(0.001),
            silence_ms: silence_ms.clamp(
                (MIN_PAUSE_SECS * 1000.0) as u64,
                (MAX_PAUSE_SECS * 1000.0) as u64,
            ),
            speaking: false,
            silence_accum_ms: 0,
        }
    }

    pub fn set_silence_secs(&mut self, secs: f64) {
        let clamped = secs.clamp(MIN_PAUSE_SECS, MAX_PAUSE_SECS);
        self.silence_ms = (clamped * 1000.0).round() as u64;
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    pub fn process_frame(&mut self, samples: &[f32], frame_ms: u64) -> VadTransition {
        if samples.is_empty() {
            return VadTransition::None;
        }
        let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
        let rms = (sum_sq / samples.len() as f32).sqrt();
        if rms >= self.threshold {
            self.silence_accum_ms = 0;
            if !self.speaking {
                self.speaking = true;
                return VadTransition::SpeechStart;
            }
            return VadTransition::None;
        }
        if self.speaking {
            self.silence_accum_ms = self.silence_accum_ms.saturating_add(frame_ms);
            if self.silence_accum_ms >= self.silence_ms {
                self.speaking = false;
                self.silence_accum_ms = 0;
                return VadTransition::SpeechEnd;
            }
        }
        VadTransition::None
    }
}

/// Downsamples mono `f32` audio from `source_rate` to `target_rate` and encodes little-endian PCM16.
pub fn downsample_f32_to_pcm16le(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<u8> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    let ratio = source_rate as f64 / target_rate as f64;
    let out_len = ((samples.len() as f64) / ratio).floor().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len * 2);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = (src_pos as usize).min(samples.len() - 1);
        let sample = samples[idx].clamp(-1.0, 1.0);
        let i16_val = (sample * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&i16_val.to_le_bytes());
    }
    out
}

/// Decodes little-endian PCM16 mono bytes to normalized `f32` samples in `[-1.0, 1.0]`.
pub fn pcm16le_to_f32(pcm: &[u8]) -> Vec<f32> {
    let (chunks, _) = pcm.as_chunks::<2>();
    chunks
        .iter()
        .map(|&pair| {
            let val = i16::from_le_bytes(pair);
            f32::from(val) / 32768.0
        })
        .collect()
}

/// Builds the `/voice/room` WebSocket URL for `(session, conversation)`.
pub fn voice_room_ws_url(scheme: &str, host: &str, session: &str, conversation: u64) -> String {
    let ws_scheme = if scheme == "https:" || scheme == "wss" {
        "wss"
    } else {
        "ws"
    };
    format!(
        "{ws_scheme}://{host}/voice/room?session={session}&conversation={}",
        conversation.max(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_frame_handles_odd_and_even_id_padding() {
        let odd_frame = encode_room_frame(AGENT_AUDIO, "abc", &[1, 2, 3, 4]);
        // 2 + 3 = 5 (odd) -> 1 pad byte -> offset 6
        assert_eq!(odd_frame.len(), 10);
        let parsed = parse_room_frame(&odd_frame).expect("parsed odd frame");
        assert_eq!(parsed.kind, AGENT_AUDIO);
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.pcm, vec![1, 2, 3, 4]);

        let even_frame = encode_room_frame(HUMAN_AUDIO, "ab", &[9, 8]);
        assert_eq!(even_frame.len(), 6);
        let parsed_even = parse_room_frame(&even_frame).expect("parsed even frame");
        assert_eq!(parsed_even.id, "ab");
        assert_eq!(parsed_even.pcm, vec![9, 8]);
    }

    #[test]
    fn vad_triggers_speech_start_and_end_after_silence_window() {
        let mut vad = VadDetector::new(0.02, 500);
        assert_eq!(vad.process_frame(&[0.0; 160], 100), VadTransition::None);
        assert_eq!(
            vad.process_frame(&[0.2; 160], 100),
            VadTransition::SpeechStart
        );
        assert_eq!(vad.process_frame(&[0.0; 160], 300), VadTransition::None);
        assert_eq!(
            vad.process_frame(&[0.0; 160], 250),
            VadTransition::SpeechEnd
        );
    }

    #[test]
    fn pcm16_downsample_and_decode_round_trip() {
        let pcm = downsample_f32_to_pcm16le(&[0.5, 0.5, -0.5, -0.5], 32_000, HUMAN_RATE);
        assert_eq!(pcm.len(), 4);
        let decoded = pcm16le_to_f32(&pcm);
        assert_eq!(decoded.len(), 2);
        assert!(decoded[0] > 0.45 && decoded[1] < -0.45);
        assert_eq!(
            voice_room_ws_url("https:", "localhost:8080", "s1", 2),
            "wss://localhost:8080/voice/room?session=s1&conversation=2"
        );
    }
}
