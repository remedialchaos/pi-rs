use std::sync::Mutex;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{Error, Result, text::sentences};

#[derive(Clone, Debug, Serialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct EngineInfo {
    pub id: String,
    pub name: String,
    pub streaming: bool,
    pub sample_rate: u32,
    pub default_speed: f32,
    pub default_voice: String,
    /// Voice ids here are local, without the engine prefix.
    pub voices: Vec<Voice>,
}

/// A model returns synchronously, emitting ordered chunks until the sink is closed or cancelled.
/// The service gives each engine its own lock, held for a complete request.
pub trait TtsEngine: Send {
    fn stream(
        &mut self,
        text: &str,
        voice: &str,
        speed: f32,
        stop: &CancellationToken,
        chunk: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()>;
}

pub trait SttRecognizer: Send + Sync {
    fn transcribe(&self, samples: &[f32]) -> Result<String>;
}

pub struct Engine {
    pub info: EngineInfo,
    inner: Mutex<Box<dyn TtsEngine>>,
}

impl Engine {
    pub fn new(info: EngineInfo, engine: impl TtsEngine + 'static) -> Result<Self> {
        if info.sample_rate == 0
            || !info.voices.iter().any(|v| v.id == info.default_voice)
            || info
                .voices
                .iter()
                .any(|voice| !valid_id(&format!("{}-{}", info.id, voice.id), 32))
        {
            return Err(Error::Config(format!(
                "invalid voice catalog for {}",
                info.id
            )));
        }
        Ok(Self {
            info,
            inner: Mutex::new(Box::new(engine)),
        })
    }

    pub(crate) fn stream(
        &self,
        text: &str,
        voice: &str,
        speed: Option<f32>,
        stop: &CancellationToken,
        chunk: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        let mut engine = self
            .inner
            .lock()
            .map_err(|_| Error::Model("synthesis engine lock poisoned".into()))?;
        if stop.is_cancelled() {
            return Ok(());
        }
        engine.stream(
            text,
            voice,
            speed.unwrap_or(self.info.default_speed),
            stop,
            &mut |samples| !stop.is_cancelled() && (samples.is_empty() || chunk(samples)),
        )
    }
}

pub trait SentenceSynthesizer: Send {
    fn sentence(&mut self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>>;
}

pub struct SentenceEngine<S> {
    model: S,
    silence: usize,
}

impl<S: SentenceSynthesizer> SentenceEngine<S> {
    pub fn new(model: S, sample_rate: u32) -> Self {
        Self {
            model,
            silence: sample_rate as usize / 4,
        }
    }
}

impl<S: SentenceSynthesizer> TtsEngine for SentenceEngine<S> {
    fn stream(
        &mut self,
        text: &str,
        voice: &str,
        speed: f32,
        stop: &CancellationToken,
        chunk: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        for (index, sentence) in sentences(text)?.into_iter().enumerate() {
            if stop.is_cancelled() {
                break;
            }
            let mut audio = self.model.sentence(&sentence, voice, speed)?;
            if index != 0 {
                let mut with_gap = Vec::with_capacity(self.silence + audio.len());
                with_gap.resize(self.silence, 0.);
                with_gap.append(&mut audio);
                audio = with_gap;
            }
            if stop.is_cancelled() || !chunk(&audio) {
                break;
            }
        }
        Ok(())
    }
}

pub(crate) fn valid_id(id: &str, limit: usize) -> bool {
    (1..=limit).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
