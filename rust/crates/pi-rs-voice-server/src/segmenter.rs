use std::collections::VecDeque;

use crate::Result;

pub const SAMPLE_RATE: u32 = 16_000;
pub const WINDOW: usize = 512;

pub struct SpeechSegment {
    pub start: usize,
    pub samples: Vec<f32>,
}

pub struct VadOutput {
    pub speaking: bool,
    pub segment: Option<SpeechSegment>,
}

pub trait VoiceActivityDetector: Send {
    fn accept(&mut self, frame: &[f32; WINDOW]) -> Result<VadOutput>;
    fn reset(&mut self);
}

#[derive(Debug, PartialEq)]
pub enum SegmenterEvent {
    SpeechStart,
    SpeechEnd(Vec<f32>),
}

struct Snapshot {
    detected: usize,
    start: usize,
    audio: Vec<f32>,
}

pub struct Segmenter {
    vad: Box<dyn VoiceActivityDetector>,
    pending: [f32; WINDOW],
    pending_len: usize,
    history: VecDeque<f32>,
    fed: usize,
    preroll: usize,
    keep: usize,
    speaking: bool,
    snapshot: Option<Snapshot>,
    previous_end: usize,
}

impl Segmenter {
    pub fn new(vad: Box<dyn VoiceActivityDetector>, sample_rate: u32, preroll_ms: f64) -> Self {
        let preroll = (f64::from(sample_rate) * preroll_ms.max(0.0) / 1000.0) as usize;
        Self {
            vad,
            pending: [0.; WINDOW],
            pending_len: 0,
            history: VecDeque::new(),
            fed: 0,
            preroll,
            keep: preroll.saturating_add(sample_rate as usize),
            speaking: false,
            snapshot: None,
            previous_end: 0,
        }
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    pub fn reset(&mut self) {
        self.vad.reset();
        self.pending_len = 0;
        self.history.clear();
        self.fed = 0;
        self.speaking = false;
        self.snapshot = None;
        self.previous_end = 0;
    }

    pub fn accept(&mut self, mut samples: &[f32]) -> Result<Vec<SegmenterEvent>> {
        let mut events = Vec::new();
        if self.pending_len > 0 {
            let count = samples.len().min(WINDOW - self.pending_len);
            self.pending[self.pending_len..self.pending_len + count]
                .copy_from_slice(&samples[..count]);
            self.pending_len += count;
            if self.pending_len < WINDOW {
                return Ok(events);
            }
            self.pending_len = 0;
            let pending = self.pending;
            self.frame(&pending, &mut events)?;
            samples = &samples[count..];
        }
        let (windows, remainder) = samples.as_chunks::<WINDOW>();
        for window in windows {
            self.frame(window, &mut events)?;
        }
        self.pending[..remainder.len()].copy_from_slice(remainder);
        self.pending_len = remainder.len();
        Ok(events)
    }

    fn frame(&mut self, frame: &[f32; WINDOW], events: &mut Vec<SegmenterEvent>) -> Result<()> {
        let VadOutput { speaking, segment } = self.vad.accept(frame)?;
        self.fed += WINDOW;
        if self.preroll != 0 {
            let skip = WINDOW.saturating_sub(self.keep);
            let drop = (self.history.len() + WINDOW - skip).saturating_sub(self.keep);
            self.history.drain(..drop);
            self.history.extend(&frame[skip..]);
        }
        if speaking && !self.speaking {
            self.speaking = true;
            if self.preroll != 0 {
                self.snapshot = Some(Snapshot {
                    detected: self.fed,
                    start: self.fed - self.history.len(),
                    audio: self.history.iter().copied().collect(),
                });
            }
            events.push(SegmenterEvent::SpeechStart);
        }
        if let Some(SpeechSegment { start, samples }) = segment {
            self.speaking = false;
            let samples_end = start.saturating_add(samples.len());
            events.push(SegmenterEvent::SpeechEnd(self.extend(start, samples)));
            self.previous_end = samples_end;
        }
        Ok(())
    }

    fn extend(&mut self, start: usize, audio: Vec<f32>) -> Vec<f32> {
        let Some(snapshot) = self.snapshot.take() else {
            return audio;
        };
        if start < snapshot.start || start > snapshot.start + snapshot.audio.len() {
            return audio;
        }
        let begin = snapshot
            .detected
            .saturating_sub(self.preroll)
            .max(snapshot.start)
            .max(self.previous_end);
        if begin >= start {
            return audio;
        }
        let mut extended = Vec::with_capacity(start - begin + audio.len());
        extended.extend_from_slice(&snapshot.audio[begin - snapshot.start..start - snapshot.start]);
        extended.extend(audio);
        extended
    }
}
