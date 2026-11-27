use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use ort::session::Session;
use ort::session::SessionOutputs;
use ort::value::{DynValue, PrimitiveTensorElementType, Tensor, TensorRef};

mod kokoro;
mod parakeet;
mod phonemizer;
pub mod pocket;
mod silero;
mod supertonic;

pub use parakeet::Parakeet;
pub use pocket::{
    DEFAULT_POCKET_CHUNK_SAMPLES, DEFAULT_POCKET_ONNX_MODEL, DEFAULT_POCKET_SAMPLE_RATE,
    POCKET_VOICES, Pocket, PocketConfig, PocketOnnx, PocketSynthesis, PocketTensor,
    PocketTokenEncoding, PocketVoiceState, PocketVoiceStateCache, load_safetensors_f32,
    parse_safetensors_f32, pocket_voices,
};
pub use silero::{SileroModel, VadConfig};

use crate::{Error, Result, VoiceServer};

#[derive(Debug)]
pub struct Config {
    pub models: PathBuf,
    pub engines: Vec<String>,
    pub default_engine: String,
    pub default_voice: Option<String>,
    pub tts_threads: usize,
    pub tts_steps: usize,
    /// Threads the Pocket TTS session uses. Pocket sets its own thread count, so it has its own
    /// setting, like upstream's `VOICE_POCKET_THREADS`.
    pub pocket_threads: usize,
    pub vad: VadConfig,
    pub preroll_ms: f64,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let engines =
            env::var("VOICE_ENGINES").unwrap_or_else(|_| "supertonic,pocket,kokoro".to_owned());
        let threshold = number::<f32>("VOICE_VAD_THRESHOLD", 0.5)?;
        let silence = number::<f32>("VOICE_VAD_SILENCE", 1.2)?;
        let preroll_ms = number::<f64>("VOICE_VAD_PREROLL_MS", 750.)?;
        let tts_threads = number("VOICE_TTS_THREADS", 8)?;
        let pocket_threads = number("VOICE_POCKET_THREADS", 2)?;
        let tts_steps = number("VOICE_TTS_STEPS", 5)?;
        if !(0. ..=1.).contains(&threshold)
            || !silence.is_finite()
            || !(0. ..60.).contains(&silence)
            || !preroll_ms.is_finite()
            || tts_threads == 0
            || pocket_threads == 0
            || tts_steps == 0
        {
            return Err(Error::Config("invalid voice model configuration".into()));
        }
        Ok(Self {
            models: env::var_os("VOICE_MODELS")
                .map_or_else(|| PathBuf::from("/models"), PathBuf::from),
            engines: engines
                .split(',')
                .map(|name| name.trim().to_owned())
                .collect(),
            default_engine: env::var("VOICE_DEFAULT_ENGINE")
                .unwrap_or_else(|_| "supertonic".into()),
            default_voice: env::var("VOICE_DEFAULT").ok(),
            tts_threads,
            tts_steps,
            pocket_threads,
            vad: VadConfig {
                threshold,
                silence_seconds: silence,
            },
            preroll_ms,
            port: number("PORT", 8000)?,
        })
    }

    pub fn load_engines(&self) -> Result<Vec<crate::Engine>> {
        let mut engines = Vec::new();
        for name in &self.engines {
            if engines
                .iter()
                .any(|engine: &crate::Engine| engine.info.id == *name)
            {
                continue;
            }
            match name.as_str() {
                "supertonic" => engines.push(supertonic::load(
                    &self.models.join("supertonic3"),
                    self.tts_threads,
                    self.tts_steps,
                )?),
                "kokoro" => {
                    engines.push(kokoro::load(&self.models.join("kokoro"), self.tts_threads)?)
                }
                "pocket" => {
                    if let Some(engine) = pocket::load_if_present(
                        &self.models.join("pocket"),
                        self.pocket_threads,
                        self.tts_steps,
                    )? {
                        engines.push(engine);
                    }
                }
                _ => {}
            }
        }
        if engines.is_empty() {
            return Err(Error::Config(
                "no native voice engines enabled; configure VOICE_ENGINES=supertonic, pocket, or kokoro"
                    .into(),
            ));
        }
        Ok(engines)
    }

    pub fn load(&self, token: &str) -> Result<VoiceServer> {
        let engines = self.load_engines()?;
        let vad = SileroModel::load(&self.models.join("silero_vad.onnx"), self.vad)?;
        let stt = Arc::new(Parakeet::load(&self.models.join("parakeet"))?);
        VoiceServer::new(
            token,
            engines,
            &self.default_engine,
            self.default_voice.as_deref(),
            move || Ok(Box::new(vad.detector())),
            self.preroll_ms,
            stt,
        )
    }
}

fn number<T: FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("invalid {name}: {text}"))),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(Error::Config(format!("invalid {name}: {error}"))),
    }
}

/// Initialize a runtime explicitly, so a missing/incompatible library is a startup error.
pub fn init_runtime(path: Option<&OsStr>) -> Result<()> {
    let path = path
        .map(OsString::from)
        .or_else(|| env::var_os("ORT_DYLIB_PATH").filter(|p| !p.is_empty()))
        .unwrap_or_else(|| {
            let name = if cfg!(target_os = "windows") {
                "onnxruntime.dll"
            } else if cfg!(target_os = "macos") {
                "libonnxruntime.dylib"
            } else {
                "libonnxruntime.so"
            };
            name.into()
        });
    let environment = ort::init_from(path).map_err(|err| {
        Error::Config(format!(
            "could not load ONNX Runtime (set ORT_DYLIB_PATH): {err}"
        ))
    })?;
    environment.commit();
    Ok(())
}

fn model<T, E: std::fmt::Display>(value: std::result::Result<T, E>) -> Result<T> {
    value.map_err(|error| Error::Model(error.to_string()))
}

fn session(path: &Path, threads: usize) -> Result<Session> {
    let open = || -> Result<Session> {
        let builder = model(Session::builder())?;
        let builder = model(builder.with_intra_threads(threads))?;
        let builder = model(builder.with_inter_threads(1))?;
        let builder = model(builder.with_intra_op_spinning(false))?;
        let mut builder = model(builder.with_inter_op_spinning(false))?;
        model(builder.commit_from_file(path))
    };
    open().map_err(|error| Error::Model(format!("{}: {error}", path.display())))
}

fn lock(session: &Mutex<Session>) -> Result<MutexGuard<'_, Session>> {
    session
        .lock()
        .map_err(|_| Error::Model("ONNX session lock poisoned".into()))
}

fn tensor<T: PrimitiveTensorElementType + Copy + std::fmt::Debug + 'static, const N: usize>(
    shape: [usize; N],
    data: Vec<T>,
) -> Result<Tensor<T>> {
    model(Tensor::from_array((shape, data)))
}

fn view<T: PrimitiveTensorElementType + Copy + std::fmt::Debug + 'static, const N: usize>(
    shape: [usize; N],
    data: &[T],
) -> Result<TensorRef<'_, T>> {
    model(TensorRef::from_array_view((shape, data)))
}

fn output(outputs: SessionOutputs<'_>) -> Result<DynValue> {
    outputs
        .into_iter()
        .next()
        .map(|(_, val)| val)
        .ok_or_else(|| Error::Model("missing ONNX output".into()))
}
