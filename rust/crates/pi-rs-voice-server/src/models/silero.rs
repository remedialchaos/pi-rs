use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ort::session::Session;

use super::{lock, model, session, view};
use crate::{Error, Result, SAMPLE_RATE, SpeechSegment, VadOutput, VoiceActivityDetector, WINDOW};

const MIN_SPEECH: usize = SAMPLE_RATE as usize / 4;
const MAX_SPEECH: usize = SAMPLE_RATE as usize * 30;
const LOOK_BACK: usize = 2 * WINDOW + MIN_SPEECH;

#[derive(Clone, Copy, Debug)]
pub struct VadConfig {
    pub threshold: f32,
    pub silence_seconds: f32,
}
impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            silence_seconds: 1.2,
        }
    }
}

#[derive(Clone)]
pub struct SileroModel {
    session: Arc<Mutex<Session>>,
    stock: bool,
    config: VadConfig,
}

impl SileroModel {
    pub fn load(path: &Path, config: VadConfig) -> Result<Self> {
        let loaded = session(path, 1)?;
        let names = loaded.inputs().iter().map(|v| v.name()).collect::<Vec<_>>();
        let stock = names == ["x", "h", "c"];
        if !stock
            && !(names.len() == 4
                && ["input", "sr", "h", "c"]
                    .iter()
                    .all(|name| names.contains(name)))
        {
            return Err(Error::Model(format!(
                "unsupported Silero V4 input signature: {names:?}"
            )));
        }
        Ok(Self {
            session: Arc::new(Mutex::new(loaded)),
            config,
            stock,
        })
    }

    pub fn detector(&self) -> impl VoiceActivityDetector + use<> {
        Silero {
            model: self.clone(),
            h: [0.; 128],
            c: [0.; 128],
            history: VecDeque::new(),
            tail: 0,
            start: None,
            above: None,
            below: None,
            triggered: false,
        }
    }
}

struct Silero {
    model: SileroModel,
    h: [f32; 128],
    c: [f32; 128],
    history: VecDeque<f32>,
    tail: usize,
    start: Option<usize>,
    above: Option<usize>,
    below: Option<usize>,
    triggered: bool,
}

impl Silero {
    fn probability(&mut self, frame: &[f32; WINDOW]) -> Result<f32> {
        let mut session = lock(&self.model.session)?;
        let audio = view([1, WINDOW], frame)?;
        let h = view([2, 1, 64], &self.h)?;
        let c = view([2, 1, 64], &self.c)?;
        let rate = [i64::from(SAMPLE_RATE)];
        let outputs = if self.model.stock {
            model(session.run(ort::inputs!["x" => audio, "h" => h, "c" => c]))?
        } else {
            model(session.run(ort::inputs!["input" => audio, "sr" => view([0usize; 0], &rate)?, "h" => h, "c" => c]))?
        };
        let mut outputs = outputs.into_iter().map(|(_, v)| v);
        let mut next = || {
            outputs
                .next()
                .ok_or_else(|| Error::Model("missing Silero output".into()))
        };
        let prob = *model(next()?.try_extract_tensor::<f32>())?
            .1
            .first()
            .ok_or_else(|| Error::Model("empty Silero probability".into()))?;
        let h = next()?;
        let c = next()?;
        let h: &[f32; 128] = model(h.try_extract_tensor::<f32>())?
            .1
            .try_into()
            .map_err(|_| Error::Model("invalid Silero hidden state".into()))?;
        let c: &[f32; 128] = model(c.try_extract_tensor::<f32>())?
            .1
            .try_into()
            .map_err(|_| Error::Model("invalid Silero cell state".into()))?;
        self.h = *h;
        self.c = *c;
        Ok(prob)
    }

    fn classify(&mut self, probability: f32, threshold: f32, silence: usize) -> bool {
        if probability > threshold {
            self.below = None;
            let Some(above) = self.above else {
                self.above = Some(self.tail);
                return false;
            };
            if !self.triggered {
                if self.tail - above < MIN_SPEECH {
                    return false;
                }
                self.triggered = true;
                return true;
            }
        }
        if probability < threshold && !self.triggered {
            self.above = None;
            self.below = None;
            return false;
        }
        if self.triggered && probability > (threshold - 0.15).max(0.01) {
            return true;
        }
        if self.triggered && probability < threshold {
            let first = *self.below.get_or_insert(self.tail);
            if self.tail - first < silence {
                return true;
            }
            self.above = None;
            self.below = None;
            self.triggered = false;
        }
        false
    }
}

impl VoiceActivityDetector for Silero {
    fn accept(&mut self, frame: &[f32; WINDOW]) -> Result<VadOutput> {
        let long = self.history.len() > MAX_SPEECH;
        let threshold = if long {
            0.90
        } else {
            self.model.config.threshold
        };
        let silence = (SAMPLE_RATE as f32
            * if long {
                0.1
            } else {
                self.model.config.silence_seconds
            }) as usize;
        let probability = self.probability(frame)?;
        self.history.extend(frame);
        self.tail += WINDOW;
        let head = self.tail - self.history.len();
        let speaking = self.classify(probability, threshold, silence);
        let segment = if speaking {
            self.start
                .get_or_insert_with(|| self.tail.saturating_sub(LOOK_BACK).max(head));
            None
        } else if let Some(start) = self.start.take() {
            let end = self.tail.saturating_sub(silence).max(start);
            let audio = self
                .history
                .range(start - head..end - head)
                .copied()
                .collect();
            self.history.drain(..end - head);
            Some(SpeechSegment {
                start,
                samples: audio,
            })
        } else {
            self.history
                .drain(..self.history.len().saturating_sub(LOOK_BACK));
            None
        };
        Ok(VadOutput { speaking, segment })
    }

    fn reset(&mut self) {
        self.h.fill(0.);
        self.c.fill(0.);
        self.history.clear();
        self.tail = 0;
        self.start = None;
        self.above = None;
        self.below = None;
        self.triggered = false;
    }
}
