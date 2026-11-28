//! Voice room protocol, binary PCM16 framing, resampling, and VAD state machine
//! (upstream `apps/p2/plugins/voice/protocol.ts` + WebAudio streaming).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const AGENT_AUDIO: u8 = 1;
pub const HUMAN_AUDIO: u8 = 2;
pub const HUMAN_RATE: u32 = 16_000;
pub const MIN_PAUSE_SECS: f64 = 0.5;
pub const MAX_PAUSE_SECS: f64 = 2.0;
pub const DEFAULT_PAUSE_SECS: f64 = 0.8;

/// A frame's RMS must reach this many times the noise floor to count as speech (~+11 dB).
pub const MARGIN: f64 = 3.5;
/// The lowest RMS that ever counts as speech, however quiet the floor.
pub const MIN_LEVEL: f64 = 0.003;
/// The highest level ever required (`pibot`'s fixed threshold).
pub const MAX_LEVEL: f64 = 0.018;
const FLOOR_FALL_S: f64 = 0.3;
const FLOOR_RISE_S: f64 = 8.0;

/// Minimum frame RMS required over a noise floor of `floor`.
pub fn speech_level(floor: f64) -> f64 {
    if !floor.is_finite() {
        return MIN_LEVEL;
    }
    (floor * MARGIN).clamp(MIN_LEVEL, MAX_LEVEL)
}

/// Phase classification mirrored from `pibot`'s barge-in detector (`plugins/voice/barge-in.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BargeInPhase {
    Inactive,
    Listening,
    Hearing,
    Thinking,
    Tool,
    Speaking,
    Error,
}

/// Per-window microphone level summary for diagnostics logging.
#[derive(Clone, Debug, PartialEq)]
pub struct MicLevels {
    pub playing: bool,
    pub frames: usize,
    pub peak_rms: f64,
    pub mean_rms: f64,
    pub floor: f64,
    pub level: f64,
    pub residual_at_peak: Option<f64>,
    pub max_residual: Option<f64>,
    pub candidates: usize,
}

#[derive(Clone, Debug, Default)]
struct LevelWindow {
    frames: usize,
    peak_rms: f64,
    energy: f64,
    residual_at_peak: Option<f64>,
    max_residual: Option<f64>,
    candidates: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BargeInMetrics {
    pub mic_rms: f64,
    pub residual_ratio: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BargeInResult {
    pub triggered: bool,
    pub preroll: Option<Vec<i16>>,
    pub metrics: Option<BargeInMetrics>,
}

/// Adaptive barge-in detector comparing mic energy against an adaptive noise floor and
/// delayed playback residual correlation (`plugins/voice/barge-in.ts`).
#[derive(Clone, Debug)]
pub struct BargeInDetector {
    streaming: bool,
    consecutive_frames: usize,
    playback_sample_rate: u32,
    playback_write: usize,
    playback_samples: usize,
    playback_ring: Vec<f32>,
    mic_write: usize,
    mic_samples: usize,
    mic_ring: Vec<i16>,
    residual_threshold: f64,
    trigger_frames: usize,
    floor: Option<f64>,
    windows: BTreeMap<bool, LevelWindow>,
}

impl Default for BargeInDetector {
    fn default() -> Self {
        Self::new(HUMAN_RATE as usize, 0.62, 5)
    }
}

impl BargeInDetector {
    pub fn new(target_sample_rate: usize, residual_threshold: f64, trigger_frames: usize) -> Self {
        Self {
            streaming: false,
            consecutive_frames: 0,
            playback_sample_rate: 0,
            playback_write: 0,
            playback_samples: 0,
            playback_ring: vec![0.0; 48_000 * 8],
            mic_write: 0,
            mic_samples: 0,
            mic_ring: vec![0; target_sample_rate.max(1)],
            residual_threshold,
            trigger_frames: trigger_frames.max(1),
            floor: None,
            windows: BTreeMap::new(),
        }
    }

    pub fn floor(&self) -> f64 {
        self.floor.unwrap_or(0.0)
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    pub fn reset_streaming(&mut self) {
        if self.streaming {
            self.mic_samples = 0;
        }
        self.streaming = false;
        self.consecutive_frames = 0;
    }

    pub fn take_levels(&mut self) -> Vec<MicLevels> {
        let floor = self.floor();
        let level = speech_level(floor);
        let windows = std::mem::take(&mut self.windows);
        windows
            .into_iter()
            .map(|(playing, window)| MicLevels {
                playing,
                frames: window.frames,
                peak_rms: window.peak_rms,
                mean_rms: (window.energy / (window.frames.max(1) as f64)).sqrt(),
                floor,
                level,
                residual_at_peak: window.residual_at_peak,
                max_residual: window.max_residual,
                candidates: window.candidates,
            })
            .collect()
    }

    pub fn handle_playback_audio(&mut self, samples: &[f32], sample_rate: u32) {
        if sample_rate == 0 {
            return;
        }
        if self.playback_sample_rate != sample_rate {
            self.playback_sample_rate = sample_rate;
            self.playback_ring = vec![0.0; (sample_rate as usize) * 8];
            self.playback_write = 0;
            self.playback_samples = 0;
        }
        let cap = self.playback_ring.len();
        for &sample in samples {
            self.playback_ring[self.playback_write] = sample;
            self.playback_write = (self.playback_write + 1) % cap;
            self.playback_samples += 1;
        }
    }

    pub fn should_stream_mic_normally(phase: BargeInPhase) -> bool {
        matches!(phase, BargeInPhase::Listening | BargeInPhase::Hearing)
    }

    fn should_buffer_for_barge_in(phase: BargeInPhase) -> bool {
        matches!(
            phase,
            BargeInPhase::Thinking | BargeInPhase::Tool | BargeInPhase::Speaking
        )
    }

    pub fn observe_mic(
        &mut self,
        input: &[f32],
        sample_rate: u32,
        pcm: &[i16],
        phase: BargeInPhase,
        tts_speaking: bool,
    ) -> BargeInResult {
        self.append_mic_buffer(pcm);
        let rms = mic_rms(input);
        // Evaluate against the noise floor captured before this frame so speech cannot raise its own bar.
        let level = speech_level(self.floor.unwrap_or(rms));
        self.follow(rms, input.len() as f64 / f64::from(sample_rate.max(1)));
        let buffering = Self::should_buffer_for_barge_in(phase) && !self.streaming;
        let ratio = if !buffering {
            None
        } else if phase == BargeInPhase::Tool && !tts_speaking {
            Some(1.0)
        } else {
            Some(self.barge_residual_ratio(input, sample_rate))
        };
        let triggered = ratio.is_some_and(|r| rms >= level && r >= self.residual_threshold);
        self.record(tts_speaking, rms, ratio, triggered);
        let Some(ratio) = ratio else {
            return BargeInResult::default();
        };
        self.consecutive_frames = if triggered {
            self.consecutive_frames + 1
        } else {
            self.consecutive_frames.saturating_sub(1)
        };
        if self.consecutive_frames < self.trigger_frames {
            return BargeInResult::default();
        }
        self.streaming = true;
        self.consecutive_frames = 0;
        BargeInResult {
            triggered: true,
            preroll: Some(self.buffered_mic_pcm()),
            metrics: Some(BargeInMetrics {
                mic_rms: rms,
                residual_ratio: ratio,
            }),
        }
    }

    fn append_mic_buffer(&mut self, pcm: &[i16]) {
        let cap = self.mic_ring.len();
        for &sample in pcm {
            self.mic_ring[self.mic_write] = sample;
            self.mic_write = (self.mic_write + 1) % cap;
            self.mic_samples = (self.mic_samples + 1).min(cap);
        }
    }

    fn buffered_mic_pcm(&self) -> Vec<i16> {
        let cap = self.mic_ring.len();
        let start = (self.mic_write + cap - self.mic_samples) % cap;
        let mut out = Vec::with_capacity(self.mic_samples);
        for index in 0..self.mic_samples {
            out.push(self.mic_ring[(start + index) % cap]);
        }
        out
    }

    fn follow(&mut self, rms: f64, seconds: f64) {
        let Some(current) = self.floor else {
            self.floor = Some(rms);
            return;
        };
        let tau = if rms < current {
            FLOOR_FALL_S
        } else {
            FLOOR_RISE_S
        };
        self.floor = Some(current + (rms - current) * (1.0 - (-seconds / tau).exp()));
    }

    fn record(&mut self, playing: bool, rms: f64, ratio: Option<f64>, candidate: bool) {
        let window = self.windows.entry(playing).or_default();
        window.frames += 1;
        window.energy += rms * rms;
        if candidate {
            window.candidates += 1;
        }
        if rms >= window.peak_rms {
            window.peak_rms = rms;
            if let Some(r) = ratio {
                window.residual_at_peak = Some(r);
            }
        }
        if let Some(r) = ratio {
            window.max_residual = Some(window.max_residual.map_or(r, |prev| prev.max(r)));
        }
    }

    fn barge_residual_ratio(&self, input: &[f32], sample_rate: u32) -> f64 {
        if self.playback_sample_rate != sample_rate || self.playback_samples < input.len() {
            return 1.0;
        }
        let len = input.len().max(1) as f64;
        let mic_energy: f64 = input
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / len;
        if mic_energy < 1e-7 {
            return 0.0;
        }
        let mut best_correlation = 0.0f64;
        let mut best_ratio = 1.0f64;
        for delay_ms in (20..=420).step_by(10) {
            let delay_samples =
                ((f64::from(delay_ms) / 1000.0) * f64::from(sample_rate)).round() as isize;
            let start = self.playback_samples as isize - delay_samples - input.len() as isize;
            let mut ref_energy = 0.0f64;
            let mut dot = 0.0f64;
            for (index, &sample) in input.iter().enumerate() {
                let reference = f64::from(self.read_playback_reference(start + index as isize));
                ref_energy += reference * reference;
                dot += f64::from(sample) * reference;
            }
            ref_energy /= len;
            dot /= len;
            if ref_energy < 1e-7 {
                continue;
            }
            let correlation = dot.abs() / (ref_energy * mic_energy).sqrt();
            let explained = (dot * dot) / ref_energy;
            let ratio = (mic_energy - explained).max(0.0) / mic_energy;
            if correlation > best_correlation {
                best_correlation = correlation;
                best_ratio = ratio;
            }
        }
        best_ratio
    }

    fn read_playback_reference(&self, total_index: isize) -> f32 {
        if total_index < 0 || (total_index as usize) >= self.playback_samples {
            return 0.0;
        }
        let cap = self.playback_ring.len();
        self.playback_ring[(total_index as usize) % cap]
    }
}

fn mic_rms(input: &[f32]) -> f64 {
    let energy: f64 = input.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (energy / input.len().max(1) as f64).sqrt()
}

/// An audio output device as returned by `enumerateDevices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutput {
    pub kind: String,
    pub device_id: String,
    pub label: String,
}

fn is_worn_output_label(label: &str) -> bool {
    let lower = label.to_lowercase();
    [
        "headphone",
        "head phone",
        "earphone",
        "ear phone",
        "headset",
        "earbud",
        "ear bud",
        "airpod",
        "in-ear",
        "kopfhörer",
        "kopfhorer",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

/// Returns `false` when the default audio output is headphones/earphones so the browser's
/// echo canceller does not suppress the speaker's voice while agent speech plays.
pub fn echo_cancellation_for(devices: &[AudioOutput]) -> bool {
    let mut outputs = devices.iter().filter(|d| d.kind == "audiooutput");
    let used = devices
        .iter()
        .find(|d| d.kind == "audiooutput" && d.device_id == "default")
        .or_else(|| outputs.next());
    !used.is_some_and(|d| is_worn_output_label(&d.label))
}

/// Stateful 16 kHz PCM16 resampler preserving fractional sample offset across frames.
#[derive(Clone, Debug, Default)]
pub struct Pcm16kResampler {
    rest: Vec<f32>,
    offset: f64,
    rate: u32,
}

impl Pcm16kResampler {
    pub fn push(&mut self, input: &[f32], rate: u32) -> Vec<i16> {
        if rate == 0 || input.is_empty() {
            return Vec::new();
        }
        if rate != self.rate {
            self.reset();
            self.rate = rate;
        }
        let ratio = f64::from(rate) / f64::from(HUMAN_RATE);
        let mut samples = Vec::with_capacity(self.rest.len() + input.len());
        samples.extend_from_slice(&self.rest);
        samples.extend_from_slice(input);
        let out_len = (((samples.len() as f64) - self.offset) / ratio)
            .floor()
            .max(0.0) as usize;
        let mut output = Vec::with_capacity(out_len);
        for index in 0..out_len {
            let start = (self.offset + (index as f64) * ratio).floor() as usize;
            let end = ((self.offset + ((index + 1) as f64) * ratio).floor() as usize)
                .max(start + 1)
                .min(samples.len());
            let mut sum = 0.0f64;
            for &sample in &samples[start..end] {
                sum += f64::from(sample);
            }
            let mean = (sum / (end - start).max(1) as f64).clamp(-1.0, 1.0);
            let scaled = if mean < 0.0 {
                mean * f64::from(i16::MAX) + 0.0
            } else {
                mean * f64::from(i16::MAX)
            };
            output.push(scaled.round() as i16);
        }
        let consumed = self.offset + (out_len as f64) * ratio;
        let whole = (consumed.floor() as usize).min(samples.len());
        self.offset = consumed - whole as f64;
        self.rest = samples[whole..].to_vec();
        output
    }

    pub fn reset(&mut self) {
        self.rest.clear();
        self.offset = 0.0;
    }
}

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

    fn tone(rms: f32, samples: usize, phase_step: f32) -> (Vec<f32>, Vec<i16>) {
        let amp = rms * std::f32::consts::SQRT_2;
        let mut floats = Vec::with_capacity(samples);
        let mut pcm = Vec::with_capacity(samples);
        for i in 0..samples {
            let s = amp * ((i as f32) * phase_step).sin();
            floats.push(s);
            pcm.push((s * f32::from(i16::MAX)).round() as i16);
        }
        (floats, pcm)
    }

    #[test]
    fn adaptive_barge_in_hears_quiet_mic_ignores_floor_and_echo_and_reports_levels() {
        let rate = 16_000;
        let frame_len = 1600;
        let mut detector = BargeInDetector::new(rate as usize, 0.62, 5);
        let (floor_f, floor_p) = tone(0.001, frame_len, 0.2);
        for _ in 0..5 {
            assert!(
                !detector
                    .observe_mic(&floor_f, rate, &floor_p, BargeInPhase::Listening, false)
                    .triggered
            );
        }
        // Quiet speaker at arm's length (~0.006 RMS over 0.001 floor) triggers barge-in.
        let (voice_f, voice_p) = tone(0.006, frame_len, 0.35);
        let mut triggered = false;
        for _ in 0..5 {
            if detector
                .observe_mic(&voice_f, rate, &voice_p, BargeInPhase::Speaking, true)
                .triggered
            {
                triggered = true;
            }
        }
        assert!(
            triggered,
            "quiet mic 0.006 over 0.001 floor should trigger barge-in"
        );

        let levels = detector.take_levels();
        assert_eq!(levels.len(), 2);
        assert!(detector.take_levels().is_empty());

        // Steady noise at the floor and near silence never trigger.
        let mut quiet_det = BargeInDetector::new(rate as usize, 0.62, 5);
        let (silence_f, silence_p) = tone(0.0015, frame_len, 0.1);
        for _ in 0..10 {
            assert!(
                !quiet_det
                    .observe_mic(&silence_f, rate, &silence_p, BargeInPhase::Speaking, true)
                    .triggered
            );
        }

        // Playback echo correlated 50ms later over the floor does not trigger.
        let mut echo_det = BargeInDetector::new(rate as usize, 0.62, 5);
        for _ in 0..5 {
            echo_det.observe_mic(&floor_f, rate, &floor_p, BargeInPhase::Listening, false);
        }
        let (echo_f, echo_p) = tone(0.008, frame_len, 0.25);
        for _ in 0..8 {
            echo_det.handle_playback_audio(&echo_f, rate);
        }
        for _ in 0..6 {
            assert!(
                !echo_det
                    .observe_mic(&echo_f, rate, &echo_p, BargeInPhase::Speaking, true)
                    .triggered
            );
        }
    }

    #[test]
    fn echo_cancellation_disables_for_headphones_and_enables_for_speakers() {
        let dev = |id: &str, label: &str| AudioOutput {
            kind: "audiooutput".into(),
            device_id: id.into(),
            label: label.into(),
        };
        assert!(echo_cancellation_for(&[]));
        assert!(echo_cancellation_for(&[dev(
            "default",
            "MacBook Pro Speakers"
        )]));
        assert!(!echo_cancellation_for(&[dev("default", "AirPods Pro")]));
        assert!(!echo_cancellation_for(&[dev("default", "USB Headset")]));
        assert!(!echo_cancellation_for(&[dev(
            "default",
            "Beyerdynamic Kopfhörer"
        )]));
    }
}
