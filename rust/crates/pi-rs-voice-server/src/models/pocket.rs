use std::fs;
use std::path::Path;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::{Engine, EngineInfo, Error, Result, TtsEngine, Voice};

pub const POCKET_VOICES: &[&str] = &[
    "alba",
    "anna",
    "azelma",
    "bill_boerst",
    "caro_davy",
    "charles",
    "cosette",
    "eponine",
    "eve",
    "fantine",
    "george",
    "jane",
    "jean",
    "javert",
    "marius",
    "mary",
    "michael",
    "paul",
    "peter_yearsley",
    "stuart_bell",
    "vera",
];

pub fn pocket_voices(path: &Path) -> Result<Vec<Voice>> {
    let mut voices = Vec::new();
    for id in POCKET_VOICES {
        if fs::exists(path.join(format!("{id}.safetensors")))? {
            let name = id
                .split('_')
                .map(|part| {
                    let mut c = part.chars();
                    c.next().map_or_else(String::new, |first| {
                        first.to_uppercase().collect::<String>() + c.as_str()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            voices.push(Voice {
                id: (*id).into(),
                name,
            });
        }
    }
    Ok(voices)
}

/// Native runtime adapter for exported Pocket weights. The PyTorch checkpoint is not executable
/// by ONNX Runtime, and the individual .safetensors files contain prompt states, not the model.
pub trait PocketSynthesis: Send {
    fn generate(
        &mut self,
        state: &Path,
        text: &str,
        stop: &CancellationToken,
        emit: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()>;
}

pub struct Pocket<P> {
    model: P,
    states: PathBuf,
}

impl<P: PocketSynthesis + 'static> Pocket<P> {
    pub fn engine(model: P, states: PathBuf, sample_rate: u32) -> Result<Engine> {
        let voices = pocket_voices(&states)?;
        Engine::new(
            EngineInfo {
                id: "pocket".into(),
                name: "Pocket TTS (Kyutai)".into(),
                streaming: true,
                sample_rate,
                default_speed: 1.,
                default_voice: "alba".into(),
                voices,
            },
            Self { model, states },
        )
    }
}

impl<P: PocketSynthesis> TtsEngine for Pocket<P> {
    fn stream(
        &mut self,
        text: &str,
        voice: &str,
        _speed: f32,
        stop: &CancellationToken,
        emit: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        if !POCKET_VOICES.contains(&voice) {
            return Err(Error::Request(format!("unknown pocket voice {voice}")));
        }
        self.model.generate(
            &self.states.join(format!("{voice}.safetensors")),
            text,
            stop,
            emit,
        )
    }
}
