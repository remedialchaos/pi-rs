use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::session::{Session, SessionInputValue};
use ort::value::{TensorElementType, TensorRef};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{model, session, tensor};
use crate::{Engine, EngineInfo, Error, Result, TtsEngine, Voice};

pub const DEFAULT_POCKET_SAMPLE_RATE: u32 = 24_000;
/// 80 ms of audio at the default sample rate, like upstream's Pocket TTS chunks.
pub const DEFAULT_POCKET_CHUNK_SAMPLES: usize = 1_920;
pub const DEFAULT_POCKET_ONNX_MODEL: &str = "pocket_tts.onnx";
const MAX_SAFETENSORS_HEADER_BYTES: usize = 100 * 1024 * 1024;
const CANONICAL_STATE_TENSORS: &[&str] = &[
    "prompt_state",
    "voice_state",
    "audio_prompt",
    "state",
    "conditioning",
];

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
        let default_voice = if voices.iter().any(|v| v.id == "alba") {
            "alba".into()
        } else {
            voices
                .first()
                .map(|v| v.id.clone())
                .unwrap_or_else(|| "alba".into())
        };
        Engine::new(
            EngineInfo {
                id: "pocket".into(),
                name: "Pocket TTS (Kyutai)".into(),
                streaming: true,
                sample_rate,
                default_speed: 1.,
                default_voice,
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

/// Decoded `f32` tensor from a `.safetensors` prompt-state file.
#[derive(Clone, Debug, PartialEq)]
pub struct PocketTensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// Parsed prompt-state tensors for a single Pocket voice.
#[derive(Clone, Debug, PartialEq)]
pub struct PocketVoiceState {
    pub tensors: BTreeMap<String, PocketTensor>,
}

impl PocketVoiceState {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            tensors: parse_safetensors_f32(bytes)?,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            tensors: load_safetensors_f32(path)?,
        })
    }

    pub fn tensor(&self, name: &str) -> Option<&PocketTensor> {
        self.tensors.get(name)
    }

    pub fn primary_tensor(&self, preferred: Option<&str>) -> Result<(&str, &PocketTensor)> {
        if let Some(name) = preferred {
            return self
                .tensors
                .get_key_value(name)
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| {
                    Error::Model(format!("missing Pocket prompt-state tensor {name:?}"))
                });
        }
        if self.tensors.len() == 1
            && let Some((name, tensor)) = self.tensors.iter().next()
        {
            return Ok((name.as_str(), tensor));
        }
        for &candidate in CANONICAL_STATE_TENSORS {
            if let Some((name, tensor)) = self.tensors.get_key_value(candidate) {
                return Ok((name.as_str(), tensor));
            }
        }
        self.tensors
            .iter()
            .next()
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| Error::Model("Pocket prompt-state file contains no tensors".into()))
    }
}

/// Cache of parsed Pocket voice prompt-state tensors keyed by voice ID.
#[derive(Debug, Default)]
pub struct PocketVoiceStateCache {
    states: BTreeMap<String, Arc<PocketVoiceState>>,
}

impl PocketVoiceStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(&mut self, voice_id: &str, path: &Path) -> Result<Arc<PocketVoiceState>> {
        if let Some(cached) = self.states.get(voice_id) {
            return Ok(Arc::clone(cached));
        }
        let parsed = Arc::new(PocketVoiceState::load(path)?);
        self.states.insert(voice_id.to_owned(), Arc::clone(&parsed));
        Ok(parsed)
    }

    pub fn get(&self, voice_id: &str) -> Option<&Arc<PocketVoiceState>> {
        self.states.get(voice_id)
    }

    pub fn contains(&self, voice_id: &str) -> bool {
        self.states.contains_key(voice_id)
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn clear(&mut self) {
        self.states.clear();
    }
}

#[derive(Deserialize)]
struct SafetensorsEntry {
    dtype: String,
    shape: Vec<u64>,
    data_offsets: [u64; 2],
}

/// Parses a HuggingFace `.safetensors` buffer into `f32` tensors (`F32`, `F16`, and `BF16`).
pub fn parse_safetensors_f32(bytes: &[u8]) -> Result<BTreeMap<String, PocketTensor>> {
    let (len_bytes, rest) = bytes
        .split_first_chunk::<8>()
        .ok_or_else(|| Error::Model("truncated safetensors header length".into()))?;
    let header_len = usize::try_from(u64::from_le_bytes(*len_bytes))
        .map_err(|_| Error::Model("safetensors header length overflows usize".into()))?;
    if header_len == 0 || header_len > MAX_SAFETENSORS_HEADER_BYTES {
        return Err(Error::Model(format!(
            "invalid safetensors header length {header_len}"
        )));
    }
    let (header_bytes, data_buf) = rest
        .split_at_checked(header_len)
        .ok_or_else(|| Error::Model("truncated safetensors JSON header".into()))?;
    let header_str = std::str::from_utf8(header_bytes)
        .map_err(|_| Error::Model("safetensors header is not valid UTF-8".into()))?;
    let header: serde_json::Map<String, Value> = serde_json::from_str(header_str)
        .map_err(|err| Error::Model(format!("invalid safetensors JSON header: {err}")))?;

    let mut tensors = BTreeMap::new();
    for (name, value) in header {
        if name == "__metadata__" {
            if !value.is_object() {
                return Err(Error::Model(
                    "safetensors __metadata__ must be a JSON object".into(),
                ));
            }
            continue;
        }
        let entry: SafetensorsEntry = serde_json::from_value(value).map_err(|err| {
            Error::Model(format!("invalid safetensors metadata for {name}: {err}"))
        })?;
        let shape: Vec<usize> = entry
            .shape
            .into_iter()
            .map(|dim| {
                usize::try_from(dim).map_err(|_| {
                    Error::Model(format!("safetensors shape overflows usize for {name}"))
                })
            })
            .collect::<Result<_>>()?;
        let start = usize::try_from(entry.data_offsets[0]).map_err(|_| {
            Error::Model(format!(
                "safetensors start offset overflows usize for {name}"
            ))
        })?;
        let end = usize::try_from(entry.data_offsets[1]).map_err(|_| {
            Error::Model(format!("safetensors end offset overflows usize for {name}"))
        })?;
        if start > end || end > data_buf.len() {
            return Err(Error::Model(format!(
                "safetensors data offsets [{start}, {end}] out of bounds (buffer len {}) for {name}",
                data_buf.len()
            )));
        }
        let element_count = shape
            .iter()
            .try_fold(1usize, |acc, &dim| acc.checked_mul(dim))
            .ok_or_else(|| {
                Error::Model(format!(
                    "safetensors element count overflows usize for {name}"
                ))
            })?;
        let elem_bytes = match entry.dtype.as_str() {
            "F32" => 4usize,
            "F16" | "BF16" => 2usize,
            other => {
                return Err(Error::Model(format!(
                    "unsupported safetensors dtype {other} for {name}"
                )));
            }
        };
        if element_count.checked_mul(elem_bytes) != Some(end - start) {
            return Err(Error::Model(format!(
                "safetensors byte length mismatch for {name}: expected {} * {elem_bytes}, got {}",
                element_count,
                end - start
            )));
        }
        let raw = &data_buf[start..end];
        let data = match entry.dtype.as_str() {
            "F32" => raw
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .map(f32::from_le_bytes)
                .collect(),
            "F16" => raw
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(|b| f16_to_f32(u16::from_le_bytes(b)))
                .collect(),
            "BF16" => raw
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(|b| bf16_to_f32(u16::from_le_bytes(b)))
                .collect(),
            // INFALLIBLE: `elem_bytes` match above already rejected every other dtype.
            _ => unreachable!("validated safetensors dtype"),
        };
        tensors.insert(name, PocketTensor { shape, data });
    }
    if tensors.is_empty() {
        return Err(Error::Model("safetensors file contains no tensors".into()));
    }
    Ok(tensors)
}

pub fn load_safetensors_f32(path: &Path) -> Result<BTreeMap<String, PocketTensor>> {
    let bytes = fs::read(path)?;
    parse_safetensors_f32(&bytes).map_err(|err| Error::Model(format!("{}: {err}", path.display())))
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = u32::from((bits >> 10) & 0x1f);
    let frac = u32::from(bits & 0x03ff);
    let out = match exp {
        0 => {
            if frac == 0 {
                sign
            } else {
                let mut e = 127u32 - 14;
                let mut m = frac;
                while (m & 0x0400) == 0 {
                    m <<= 1;
                    e -= 1;
                }
                sign | (e << 23) | ((m & 0x03ff) << 13)
            }
        }
        0x1f => {
            if frac == 0 {
                sign | 0x7f80_0000
            } else {
                sign | 0x7fc0_0000 | (frac << 13)
            }
        }
        _ => sign | ((exp + 112) << 23) | (frac << 13),
    };
    f32::from_bits(out)
}

fn bf16_to_f32(bits: u16) -> f32 {
    f32::from_bits(u32::from(bits) << 16)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PocketTokenEncoding {
    #[default]
    Auto,
    Codepoints,
    Utf8Bytes,
    Vocab,
}

/// Configuration for a native ONNX Pocket TTS model directory (`pocket.json`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct PocketConfig {
    pub sample_rate: u32,
    pub chunk_samples: usize,
    pub onnx_model: String,
    pub vocab: Option<String>,
    pub token_encoding: PocketTokenEncoding,
    pub bos_token: Option<i64>,
    pub eos_token: Option<i64>,
    pub unk_token: Option<i64>,
    pub tokens_input: String,
    pub state_input: String,
    pub state_tensor: Option<String>,
    pub speed_input: Option<String>,
    pub steps_input: Option<String>,
    pub default_speed: f32,
    pub audio_output: Option<String>,
    /// Tokens per streaming step. `None` renders the whole utterance in one pass; a value makes
    /// the model run step by step, carrying its state forward, like upstream's Pocket TTS.
    pub stream_tokens: Option<usize>,
    /// The output that carries the state to the next step. Defaults to the state input's name.
    pub state_output: Option<String>,
}

impl Default for PocketConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_POCKET_SAMPLE_RATE,
            chunk_samples: DEFAULT_POCKET_CHUNK_SAMPLES,
            onnx_model: DEFAULT_POCKET_ONNX_MODEL.into(),
            vocab: None,
            token_encoding: PocketTokenEncoding::Auto,
            bos_token: None,
            eos_token: None,
            unk_token: None,
            tokens_input: "tokens".into(),
            state_input: "prompt_state".into(),
            state_tensor: None,
            speed_input: None,
            steps_input: None,
            default_speed: 1.0,
            audio_output: None,
            stream_tokens: None,
            state_output: None,
        }
    }
}

impl PocketConfig {
    pub fn load(base: &Path) -> Result<Self> {
        let path = base.join("pocket.json");
        if !fs::exists(&path)? {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)?;
        let config: Self = serde_json::from_str(&raw)
            .map_err(|err| Error::Config(format!("invalid {}: {err}", path.display())))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.sample_rate == 0 {
            return Err(Error::Config("pocket sample_rate must be > 0".into()));
        }
        if self.chunk_samples == 0 {
            return Err(Error::Config("pocket chunk_samples must be > 0".into()));
        }
        if self.onnx_model.trim().is_empty() {
            return Err(Error::Config("pocket onnx_model cannot be empty".into()));
        }
        if !self.default_speed.is_finite() || self.default_speed <= 0.0 {
            return Err(Error::Config(
                "pocket default_speed must be finite and > 0".into(),
            ));
        }
        if self.token_encoding == PocketTokenEncoding::Vocab && self.vocab.is_none() {
            return Err(Error::Config(
                "pocket token_encoding=vocab requires a vocab path".into(),
            ));
        }
        if self.stream_tokens == Some(0) {
            return Err(Error::Config(
                "pocket stream_tokens must be greater than 0".into(),
            ));
        }
        Ok(())
    }

    pub fn model_path(&self, base: &Path) -> PathBuf {
        base.join(&self.onnx_model)
    }

    /// The ONNX weights of the Pocket directory. `VOICE_POCKET_MODEL` wins, then the configured
    /// name, then the usual export names, so an upstream-style directory works.
    pub fn resolve_model_path(&self, base: &Path) -> Option<PathBuf> {
        if let Some(model) = std::env::var_os("VOICE_POCKET_MODEL") {
            let model = PathBuf::from(model);
            let candidate = if model.is_absolute() {
                model
            } else {
                base.join(model)
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let configured = base.join(&self.onnx_model);
        if configured.is_file() {
            return Some(configured);
        }
        ["pocket_tts.onnx", "model.onnx", "pocket.onnx"]
            .iter()
            .map(|name| base.join(name))
            .find(|path| path.is_file())
    }
}

enum TokenizerKind {
    Codepoints,
    Utf8Bytes,
    Vocab {
        tokens: HashMap<String, i64>,
        max_chars: usize,
    },
    Indexer(Vec<i64>),
}

struct PocketTokenizer {
    kind: TokenizerKind,
    bos: Option<i64>,
    eos: Option<i64>,
    unk: Option<i64>,
}

impl PocketTokenizer {
    fn from_config(base: &Path, config: &PocketConfig) -> Result<Self> {
        let kind = match config.token_encoding {
            PocketTokenEncoding::Codepoints => TokenizerKind::Codepoints,
            PocketTokenEncoding::Utf8Bytes => TokenizerKind::Utf8Bytes,
            PocketTokenEncoding::Vocab => {
                let vocab_rel = config.vocab.as_deref().ok_or_else(|| {
                    Error::Config("pocket token_encoding=vocab requires a vocab path".into())
                })?;
                load_vocab_kind(&base.join(vocab_rel))?
            }
            PocketTokenEncoding::Auto => {
                if let Some(vocab_rel) = config.vocab.as_deref() {
                    load_vocab_kind(&base.join(vocab_rel))?
                } else {
                    TokenizerKind::Codepoints
                }
            }
        };
        Ok(Self {
            kind,
            bos: config.bos_token,
            eos: config.eos_token,
            unk: config.unk_token,
        })
    }

    fn encode(&self, text: &str) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        if let Some(bos) = self.bos {
            ids.push(bos);
        }
        let start_payload = ids.len();
        match &self.kind {
            TokenizerKind::Codepoints => {
                ids.extend(text.chars().map(|c| i64::from(u32::from(c))));
            }
            TokenizerKind::Utf8Bytes => {
                ids.extend(text.bytes().map(i64::from));
            }
            TokenizerKind::Indexer(table) => {
                for c in text.chars() {
                    let id = table
                        .get(u32::from(c) as usize)
                        .copied()
                        .or(self.unk)
                        .ok_or_else(|| {
                            Error::Model(format!(
                                "Pocket indexer has no entry for U+{:04X}",
                                c as u32
                            ))
                        })?;
                    ids.push(id);
                }
            }
            TokenizerKind::Vocab { tokens, max_chars } => {
                let mut rest = text;
                while !rest.is_empty() {
                    let mut matched = None;
                    let mut boundaries = rest
                        .char_indices()
                        .skip(1)
                        .map(|(idx, _)| idx)
                        .take(*max_chars)
                        .collect::<Vec<_>>();
                    if rest.chars().count() <= *max_chars {
                        boundaries.push(rest.len());
                    }
                    for &end in boundaries.iter().rev() {
                        if let Some(&id) = tokens.get(&rest[..end]) {
                            matched = Some((end, id));
                            break;
                        }
                    }
                    if let Some((end, id)) = matched {
                        ids.push(id);
                        rest = &rest[end..];
                    } else if let Some(ch) = rest.chars().next() {
                        let single_len = ch.len_utf8();
                        if let Some(unk) = self.unk {
                            ids.push(unk);
                        } else {
                            return Err(Error::Model(format!(
                                "Pocket vocab has no token for {:?}",
                                &rest[..single_len]
                            )));
                        }
                        rest = &rest[single_len..];
                    }
                }
            }
        }
        if ids.len() == start_payload {
            return Err(Error::Model(
                "Pocket text produced no synthesis tokens".into(),
            ));
        }
        if let Some(eos) = self.eos {
            ids.push(eos);
        }
        Ok(ids)
    }
}

fn load_vocab_kind(path: &Path) -> Result<TokenizerKind> {
    let raw = fs::read_to_string(path)
        .map_err(|err| Error::Config(format!("failed to read {}: {err}", path.display())))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|err| Error::Config(format!("invalid Pocket vocab {}: {err}", path.display())))?;
    if let Some(array) = value.as_array() {
        let mut table = Vec::with_capacity(array.len());
        for entry in array {
            let id = entry.as_i64().ok_or_else(|| {
                Error::Config(format!(
                    "Pocket indexer {} must contain integers",
                    path.display()
                ))
            })?;
            table.push(id);
        }
        return Ok(TokenizerKind::Indexer(table));
    }
    let map = value
        .get("vocab")
        .and_then(Value::as_object)
        .or_else(|| {
            value
                .get("model")
                .and_then(|m| m.get("vocab"))
                .and_then(Value::as_object)
        })
        .or_else(|| value.as_object())
        .ok_or_else(|| {
            Error::Config(format!(
                "Pocket vocab {} must be a JSON object or integer array",
                path.display()
            ))
        })?;
    let mut tokens = HashMap::with_capacity(map.len());
    let mut max_chars = 1usize;
    for (token, id_val) in map {
        let id = id_val.as_i64().ok_or_else(|| {
            Error::Config(format!(
                "Pocket vocab {} has non-integer ID for {token:?}",
                path.display()
            ))
        })?;
        max_chars = max_chars.max(token.chars().count());
        tokens.insert(token.clone(), id);
    }
    if tokens.is_empty() {
        return Err(Error::Config(format!(
            "Pocket vocab {} is empty",
            path.display()
        )));
    }
    Ok(TokenizerKind::Vocab { tokens, max_chars })
}

#[derive(Clone, Debug)]
enum InputRole {
    Tokens {
        dtype: TensorElementType,
        rank: usize,
    },
    PrimaryState {
        expected_shape: Vec<i64>,
    },
    NamedState {
        expected_shape: Vec<i64>,
    },
    Speed {
        dtype: TensorElementType,
        rank: usize,
    },
    Steps {
        dtype: TensorElementType,
        rank: usize,
    },
    TokensLength {
        dtype: TensorElementType,
        rank: usize,
    },
    AttentionMask {
        dtype: TensorElementType,
        rank: usize,
    },
}

#[derive(Clone, Debug)]
struct InputBinding {
    name: String,
    role: InputRole,
}

/// Native ONNX Runtime adapter for exported Pocket TTS weights.
pub struct PocketOnnx {
    session: Session,
    config: PocketConfig,
    tokenizer: PocketTokenizer,
    bindings: Vec<InputBinding>,
    output_name: String,
    steps: usize,
    states: PocketVoiceStateCache,
}

impl PocketOnnx {
    pub fn load(base: &Path, threads: usize, steps: usize) -> Result<Self> {
        let config = PocketConfig::load(base)?;
        Self::load_with_config(base, config, threads, steps)
    }

    pub fn load_with_config(
        base: &Path,
        config: PocketConfig,
        threads: usize,
        steps: usize,
    ) -> Result<Self> {
        config.validate()?;
        let model_path = config.resolve_model_path(base).ok_or_else(|| {
            Error::Model(format!("Pocket ONNX model not found in {}", base.display()))
        })?;
        let session = session(&model_path, threads)?;
        let tokenizer = PocketTokenizer::from_config(base, &config)?;
        let bindings = classify_inputs(&session, &config)?;
        let output_name = select_audio_output(&session, config.audio_output.as_deref())?;
        Ok(Self {
            session,
            config,
            tokenizer,
            bindings,
            output_name,
            steps,
            states: PocketVoiceStateCache::new(),
        })
    }

    pub fn config(&self) -> &PocketConfig {
        &self.config
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn cached_voices(&self) -> usize {
        self.states.len()
    }
}

fn classify_inputs(session: &Session, config: &PocketConfig) -> Result<Vec<InputBinding>> {
    let mut bindings = Vec::with_capacity(session.inputs().len());
    let mut has_tokens = false;
    let mut has_state = false;
    let mut matched_speed = false;
    let mut matched_steps = false;

    for inlet in session.inputs() {
        let name = inlet.name();
        let dtype = inlet
            .dtype()
            .tensor_type()
            .ok_or_else(|| Error::Model(format!("Pocket ONNX input {name} is not a tensor")))?;
        let shape = inlet
            .dtype()
            .tensor_shape()
            .ok_or_else(|| Error::Model(format!("Pocket ONNX input {name} has no shape")))?
            .to_vec();
        let rank = shape.len();

        let is_tokens = name == config.tokens_input
            || (config.tokens_input == "tokens" && matches!(name, "input_ids" | "text_ids"));
        let is_primary_state = name == config.state_input
            || (config.state_input == "prompt_state"
                && matches!(
                    name,
                    "voice_state" | "state" | "audio_prompt" | "conditioning"
                ));
        let is_speed = config
            .speed_input
            .as_deref()
            .map_or(name == "speed", |expected| name == expected);
        let is_steps = config.steps_input.as_deref().map_or(
            matches!(name, "steps" | "num_steps" | "total_step"),
            |expected| name == expected,
        );

        let role = if is_tokens {
            if !matches!(dtype, TensorElementType::Int64 | TensorElementType::Int32)
                || !matches!(rank, 1 | 2)
            {
                return Err(Error::Model(format!(
                    "Pocket ONNX token input {name} must be rank 1 or 2 int32/int64"
                )));
            }
            has_tokens = true;
            InputRole::Tokens { dtype, rank }
        } else if is_primary_state {
            if dtype != TensorElementType::Float32 {
                return Err(Error::Model(format!(
                    "Pocket ONNX state input {name} must be float32"
                )));
            }
            has_state = true;
            InputRole::PrimaryState {
                expected_shape: shape,
            }
        } else if is_speed {
            if !matches!(
                dtype,
                TensorElementType::Float32
                    | TensorElementType::Float64
                    | TensorElementType::Int32
                    | TensorElementType::Int64
            ) || rank > 1
            {
                return Err(Error::Model(format!(
                    "Pocket ONNX speed input {name} must be scalar or 1D numeric"
                )));
            }
            matched_speed = true;
            InputRole::Speed { dtype, rank }
        } else if is_steps {
            if !matches!(
                dtype,
                TensorElementType::Int64 | TensorElementType::Int32 | TensorElementType::Float32
            ) || rank > 1
            {
                return Err(Error::Model(format!(
                    "Pocket ONNX steps input {name} must be scalar or 1D integer/float32"
                )));
            }
            matched_steps = true;
            InputRole::Steps { dtype, rank }
        } else if matches!(name, "tokens_length" | "input_lengths" | "text_length") {
            if !matches!(dtype, TensorElementType::Int64 | TensorElementType::Int32) || rank > 1 {
                return Err(Error::Model(format!(
                    "Pocket ONNX length input {name} must be scalar or 1D integer"
                )));
            }
            InputRole::TokensLength { dtype, rank }
        } else if matches!(name, "attention_mask" | "text_mask") {
            if !matches!(
                dtype,
                TensorElementType::Int64 | TensorElementType::Int32 | TensorElementType::Float32
            ) || !matches!(rank, 1 | 2)
            {
                return Err(Error::Model(format!(
                    "Pocket ONNX mask input {name} must be rank 1 or 2"
                )));
            }
            InputRole::AttentionMask { dtype, rank }
        } else if dtype == TensorElementType::Float32 {
            has_state = true;
            InputRole::NamedState {
                expected_shape: shape,
            }
        } else {
            return Err(Error::Model(format!(
                "unsupported Pocket ONNX input {name} ({dtype})"
            )));
        };

        bindings.push(InputBinding {
            name: name.to_owned(),
            role,
        });
    }

    if !has_tokens {
        return Err(Error::Model(format!(
            "Pocket ONNX model is missing token input {:?}",
            config.tokens_input
        )));
    }
    if !has_state {
        return Err(Error::Model(format!(
            "Pocket ONNX model is missing prompt-state input {:?}",
            config.state_input
        )));
    }
    if let Some(speed) = config.speed_input.as_deref()
        && !matched_speed
    {
        return Err(Error::Model(format!(
            "Pocket ONNX model is missing configured speed input {speed:?}"
        )));
    }
    if let Some(steps) = config.steps_input.as_deref()
        && !matched_steps
    {
        return Err(Error::Model(format!(
            "Pocket ONNX model is missing configured steps input {steps:?}"
        )));
    }
    Ok(bindings)
}

fn select_audio_output(session: &Session, preferred: Option<&str>) -> Result<String> {
    let outputs = session.outputs();
    let outlet = if let Some(name) = preferred {
        outputs
            .iter()
            .find(|out| out.name() == name)
            .ok_or_else(|| Error::Model(format!("missing Pocket ONNX output {name:?}")))?
    } else {
        outputs
            .iter()
            .find(|out| matches!(out.name(), "audio" | "waveform" | "wav"))
            .or_else(|| outputs.first())
            .ok_or_else(|| Error::Model("Pocket ONNX model declares no outputs".into()))?
    };
    if outlet.dtype().tensor_type() != Some(TensorElementType::Float32) {
        return Err(Error::Model(format!(
            "Pocket ONNX output {:?} must be float32",
            outlet.name()
        )));
    }
    Ok(outlet.name().to_owned())
}

/// The token windows of one utterance: one per streaming step, or the whole utterance.
fn stream_windows(tokens: &[i64], stream_tokens: Option<usize>) -> Vec<&[i64]> {
    match stream_tokens.filter(|size| *size > 0) {
        Some(size) => tokens.chunks(size).collect(),
        None => vec![tokens],
    }
}

/// The state a step carries forward for one state input.
fn carried_state<'a>(
    carried: &'a HashMap<String, CarriedState>,
    config: &PocketConfig,
    input_name: &str,
) -> Option<&'a CarriedState> {
    if let Some(state) = carried.get(input_name) {
        return Some(state);
    }
    if let Some(name) = config.state_output.as_deref()
        && let Some(state) = carried.get(name)
    {
        return Some(state);
    }
    // A model with one state output names it after its input; a model that names it otherwise
    // still has exactly one state to carry.
    (carried.len() == 1)
        .then(|| carried.values().next())
        .flatten()
}

/// A state the previous streaming step produced.
#[derive(Clone, Debug, PartialEq)]
struct CarriedState {
    shape: Vec<usize>,
    data: Vec<f32>,
}

/// Builds the session input for a state tensor, checking it against the model's shape.
fn state_value<'a>(
    shape: &[usize],
    data: &'a [f32],
    expected_shape: &[i64],
    input_name: &str,
) -> Result<SessionInputValue<'a>> {
    let effective_shape: Vec<usize> = if shape.len() == expected_shape.len() {
        shape.to_vec()
    } else if shape.len() + 1 == expected_shape.len()
        && matches!(expected_shape.first(), Some(1 | -1))
    {
        let mut expanded = Vec::with_capacity(expected_shape.len());
        expanded.push(1);
        expanded.extend_from_slice(shape);
        expanded
    } else {
        return Err(Error::Model(format!(
            "Pocket prompt-state rank mismatch for {input_name}: expected {}, got {}",
            expected_shape.len(),
            shape.len()
        )));
    };
    for (&got, &want) in effective_shape.iter().zip(expected_shape.iter()) {
        if want >= 0 && got as i64 != want {
            return Err(Error::Model(format!(
                "Pocket prompt-state shape mismatch for {input_name}: expected {expected_shape:?}, got {effective_shape:?}"
            )));
        }
    }
    Ok(SessionInputValue::from(model(TensorRef::from_array_view(
        (effective_shape, data),
    ))?))
}

fn state_input_value<'a>(
    tensor_data: &'a PocketTensor,
    expected_shape: &[i64],
    input_name: &str,
) -> Result<SessionInputValue<'a>> {
    state_value(
        &tensor_data.shape,
        &tensor_data.data,
        expected_shape,
        input_name,
    )
}

fn scalar_or_vec1_shape(rank: usize) -> Vec<usize> {
    if rank == 0 { Vec::new() } else { vec![1] }
}

impl PocketSynthesis for PocketOnnx {
    fn generate(
        &mut self,
        state: &Path,
        text: &str,
        stop: &CancellationToken,
        emit: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        if stop.is_cancelled() {
            return Ok(());
        }
        let voice_id = state
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Model(format!(
                    "invalid Pocket voice state path {}",
                    state.display()
                ))
            })?;
        let voice_state = self.states.load(voice_id, state)?;
        let token_ids = self.tokenizer.encode(text)?;
        // One pass renders the whole utterance. A streaming model runs step by step and carries
        // its state forward, so the first audio arrives before the utterance is done.
        let windows = stream_windows(&token_ids, self.config.stream_tokens);
        let mut carried: HashMap<String, CarriedState> = HashMap::new();
        for window in windows {
            if stop.is_cancelled() {
                return Ok(());
            }
            let token_len = window.len();

            let mut inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> =
                Vec::with_capacity(self.bindings.len());
            for binding in &self.bindings {
                let value = match &binding.role {
                    InputRole::Tokens { dtype, rank } => {
                        let shape: Vec<usize> = if *rank == 2 {
                            vec![1, token_len]
                        } else {
                            vec![token_len]
                        };
                        match dtype {
                            TensorElementType::Int64 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, window.to_vec())),
                            )?),
                            TensorElementType::Int32 => {
                                let cast = window
                                    .iter()
                                    .map(|&id| {
                                        i32::try_from(id).map_err(|_| {
                                            Error::Model(format!(
                                                "Pocket token ID {id} overflows int32"
                                            ))
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                SessionInputValue::from(model(ort::value::Tensor::from_array((
                                    shape, cast,
                                )))?)
                            }
                            // INFALLIBLE: `classify_inputs` validates token dtype.
                            _ => unreachable!("validated token dtype"),
                        }
                    }
                    InputRole::PrimaryState { expected_shape } => {
                        match carried_state(&carried, &self.config, &binding.name) {
                            Some(carried) => state_value(
                                &carried.shape,
                                &carried.data,
                                expected_shape,
                                &binding.name,
                            )?,
                            None => {
                                let (_, st) = voice_state
                                    .primary_tensor(self.config.state_tensor.as_deref())?;
                                state_input_value(st, expected_shape, &binding.name)?
                            }
                        }
                    }
                    InputRole::NamedState { expected_shape } => {
                        match carried_state(&carried, &self.config, &binding.name) {
                            Some(carried) => state_value(
                                &carried.shape,
                                &carried.data,
                                expected_shape,
                                &binding.name,
                            )?,
                            None => {
                                let st = voice_state.tensor(&binding.name).ok_or_else(|| {
                                    Error::Model(format!(
                                        "Pocket voice {voice_id} is missing prompt-state tensor {:?}",
                                        binding.name
                                    ))
                                })?;
                                state_input_value(st, expected_shape, &binding.name)?
                            }
                        }
                    }
                    InputRole::Speed { dtype, rank } => {
                        let shape = scalar_or_vec1_shape(*rank);
                        let speed = self.config.default_speed;
                        match dtype {
                            TensorElementType::Float32 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![speed])),
                            )?),
                            TensorElementType::Float64 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![f64::from(speed)])),
                            )?),
                            TensorElementType::Int32 => {
                                SessionInputValue::from(model(ort::value::Tensor::from_array((
                                    shape,
                                    vec![speed.round_ties_even().max(1.0) as i32],
                                )))?)
                            }
                            TensorElementType::Int64 => {
                                SessionInputValue::from(model(ort::value::Tensor::from_array((
                                    shape,
                                    vec![speed.round_ties_even().max(1.0) as i64],
                                )))?)
                            }
                            // INFALLIBLE: `classify_inputs` validates speed dtype.
                            _ => unreachable!("validated speed dtype"),
                        }
                    }
                    InputRole::Steps { dtype, rank } => {
                        let shape = scalar_or_vec1_shape(*rank);
                        match dtype {
                            TensorElementType::Int64 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![self.steps as i64])),
                            )?),
                            TensorElementType::Int32 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![self.steps as i32])),
                            )?),
                            TensorElementType::Float32 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![self.steps as f32])),
                            )?),
                            // INFALLIBLE: `classify_inputs` validates steps dtype.
                            _ => unreachable!("validated steps dtype"),
                        }
                    }
                    InputRole::TokensLength { dtype, rank } => {
                        let shape = scalar_or_vec1_shape(*rank);
                        match dtype {
                            TensorElementType::Int64 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![token_len as i64])),
                            )?),
                            TensorElementType::Int32 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![token_len as i32])),
                            )?),
                            // INFALLIBLE: `classify_inputs` validates length dtype.
                            _ => unreachable!("validated length dtype"),
                        }
                    }
                    InputRole::AttentionMask { dtype, rank } => {
                        let shape = if *rank == 2 {
                            vec![1, token_len]
                        } else {
                            vec![token_len]
                        };
                        match dtype {
                            TensorElementType::Int64 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![1i64; token_len])),
                            )?),
                            TensorElementType::Int32 => SessionInputValue::from(model(
                                ort::value::Tensor::from_array((shape, vec![1i32; token_len])),
                            )?),
                            TensorElementType::Float32 => SessionInputValue::from(tensor(
                                [1, token_len],
                                vec![1.0f32; token_len],
                            )?),
                            // INFALLIBLE: `classify_inputs` validates mask dtype.
                            _ => unreachable!("validated mask dtype"),
                        }
                    }
                };
                inputs.push((Cow::Borrowed(binding.name.as_str()), value));
            }

            if stop.is_cancelled() {
                return Ok(());
            }
            let outputs = model(self.session.run(inputs))?;
            let audio_val = outputs.get(&self.output_name).ok_or_else(|| {
                Error::Model(format!("missing Pocket ONNX output {:?}", self.output_name))
            })?;
            let (_, samples) = model(audio_val.try_extract_tensor::<f32>())?;
            emit_pcm_chunks(samples, self.config.chunk_samples, stop, emit)?;
            carried = carry_state(&outputs, &self.output_name)?;
        }
        Ok(())
    }
}

/// Collects the state the model produced, keyed by output name, so the next step can use it.
/// The state outputs are named like the state inputs they replace.
fn carry_state(
    outputs: &ort::session::SessionOutputs<'_>,
    audio_output: &str,
) -> Result<HashMap<String, CarriedState>> {
    let mut carried = HashMap::new();
    for (name, value) in outputs.iter() {
        if name == audio_output {
            continue;
        }
        let Ok((shape, data)) = value.try_extract_tensor::<f32>() else {
            continue;
        };
        carried.insert(
            name.to_owned(),
            CarriedState {
                shape: shape.iter().map(|&dim| dim.max(0) as usize).collect(),
                data: data.to_vec(),
            },
        );
    }
    Ok(carried)
}

fn emit_pcm_chunks(
    samples: &[f32],
    chunk_samples: usize,
    stop: &CancellationToken,
    emit: &mut dyn FnMut(&[f32]) -> bool,
) -> Result<()> {
    if chunk_samples == 0 {
        return Err(Error::Config("pocket chunk_samples must be > 0".into()));
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(Error::Model(
            "Pocket ONNX model produced non-finite PCM samples".into(),
        ));
    }
    if samples.is_empty() {
        return Ok(());
    }
    let mut buffer = Vec::with_capacity(chunk_samples.min(samples.len()));
    for chunk in samples.chunks(chunk_samples) {
        if stop.is_cancelled() {
            break;
        }
        buffer.clear();
        buffer.extend(chunk.iter().map(|&sample| sample.clamp(-1.0, 1.0)));
        if !emit(&buffer) {
            break;
        }
    }
    Ok(())
}

pub(crate) fn load_if_present(base: &Path, threads: usize, steps: usize) -> Result<Option<Engine>> {
    let config = PocketConfig::load(base)?;
    if config.resolve_model_path(base).is_none() {
        eprintln!(
            "Pocket TTS not loaded: install a compatible exported pocket_tts.onnx alongside the voice .safetensors in {}, or set VOICE_POCKET_MODEL to the graph path. Raw Hugging Face weights require conversion. See the Pocket export section in crates/pi-rs-voice-server/README.md.",
            base.display()
        );
        return Ok(None);
    }
    let sample_rate = config.sample_rate;
    let model = PocketOnnx::load_with_config(base, config, threads, steps)?;
    Ok(Some(Pocket::engine(
        model,
        base.to_path_buf(),
        sample_rate,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streaming_run_splits_the_utterance_into_token_windows() {
        let tokens = [1i64, 2, 3, 4, 5];
        assert_eq!(stream_windows(&tokens, None), vec![tokens.as_slice()]);
        assert_eq!(
            stream_windows(&tokens, Some(2)),
            vec![
                tokens[..2].as_ref(),
                tokens[2..4].as_ref(),
                tokens[4..].as_ref()
            ]
        );
        // A zero or missing size renders the whole utterance in one pass.
        assert_eq!(stream_windows(&tokens, Some(0)), vec![tokens.as_slice()]);
    }

    #[test]
    fn a_carried_state_is_found_by_its_input_or_output_name() {
        let mut carried = HashMap::new();
        carried.insert(
            "next_state".to_owned(),
            CarriedState {
                shape: vec![1, 2],
                data: vec![0.5, 0.25],
            },
        );
        let config = PocketConfig::default();
        // The state's own name, then the configured output name, then the only state there is.
        assert!(carried_state(&carried, &config, "next_state").is_some());
        assert!(carried_state(&carried, &config, "prompt_state").is_some());
        let named = PocketConfig {
            state_output: Some("next_state".into()),
            ..PocketConfig::default()
        };
        assert!(carried_state(&carried, &named, "prompt_state").is_some());
        let mut two = carried.clone();
        two.insert(
            "other".to_owned(),
            CarriedState {
                shape: vec![1, 2],
                data: vec![0.1, 0.2],
            },
        );
        // With more than one state, only a matching name counts.
        assert!(carried_state(&two, &config, "prompt_state").is_none());
    }

    #[test]
    fn a_state_tensor_must_match_the_shape_the_model_expects() -> Result<()> {
        let state = CarriedState {
            shape: vec![1, 4],
            data: vec![0.0; 4],
        };
        assert!(state_value(&state.shape, &state.data, &[1, 3], "prompt_state").is_err());
        assert!(state_value(&state.shape, &state.data, &[4], "prompt_state").is_err());
        Ok(())
    }

    #[test]
    fn f16_and_bf16_bit_conversions_cover_normals_subnormals_zeros_and_specials() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!(f16_to_f32(0x8000).is_sign_negative() && f16_to_f32(0x8000) == 0.0);
        assert!((f16_to_f32(0x3c00) - 1.0).abs() < 1e-6);
        assert!((f16_to_f32(0xc000) + 2.0).abs() < 1e-6);
        assert!((f16_to_f32(0x0001) - 5.9604645e-8).abs() < 1e-12);
        assert_eq!(f16_to_f32(0x7c00), f32::INFINITY);
        assert_eq!(f16_to_f32(0xfc00), f32::NEG_INFINITY);
        assert!(f16_to_f32(0x7e00).is_nan());

        assert_eq!(bf16_to_f32(0x3f80), 1.0);
        assert_eq!(bf16_to_f32(0xbf80), -1.0);
        assert_eq!(bf16_to_f32(0x7f80), f32::INFINITY);
        assert!(bf16_to_f32(0x7fc0).is_nan());
    }

    #[test]
    fn emit_pcm_chunks_clamps_splits_rejects_non_finite_and_honors_stop_and_sink_false()
    -> Result<()> {
        let stop = CancellationToken::new();
        let mut emitted = Vec::new();
        emit_pcm_chunks(&[-1.5, -0.25, 0.25, 1.5, 0.5], 2, &stop, &mut |chunk| {
            emitted.push(chunk.to_vec());
            true
        })?;
        assert_eq!(emitted, vec![vec![-1.0, -0.25], vec![0.25, 1.0], vec![0.5]]);

        let mut sink_stopped = Vec::new();
        emit_pcm_chunks(&[0.1, 0.2, 0.3, 0.4], 2, &stop, &mut |chunk| {
            sink_stopped.push(chunk.to_vec());
            false
        })?;
        assert_eq!(sink_stopped, vec![vec![0.1, 0.2]]);

        let cancel_after_first = CancellationToken::new();
        let mut cancelled = Vec::new();
        emit_pcm_chunks(
            &[0.1, 0.2, 0.3, 0.4],
            2,
            &cancel_after_first,
            &mut |chunk| {
                cancelled.push(chunk.to_vec());
                cancel_after_first.cancel();
                true
            },
        )?;
        assert_eq!(cancelled, vec![vec![0.1, 0.2]]);

        let mut on_nan = Vec::new();
        let err = emit_pcm_chunks(&[0.1, f32::NAN, 0.3], 2, &stop, &mut |chunk| {
            on_nan.push(chunk.to_vec());
            true
        })
        .unwrap_err();
        assert!(matches!(err, Error::Model(_)));
        assert!(on_nan.is_empty());

        let err = emit_pcm_chunks(&[0.1, f32::INFINITY], 2, &stop, &mut |_| true).unwrap_err();
        assert!(matches!(err, Error::Model(_)));
        Ok(())
    }

    #[test]
    fn pocket_tokenizer_supports_codepoints_utf8_bytes_vocab_and_indexer() -> Result<()> {
        let cp = PocketTokenizer {
            kind: TokenizerKind::Codepoints,
            bos: Some(1),
            eos: Some(2),
            unk: None,
        };
        assert_eq!(cp.encode("Aé")?, vec![1, 65, 233, 2]);

        let bytes = PocketTokenizer {
            kind: TokenizerKind::Utf8Bytes,
            bos: None,
            eos: None,
            unk: None,
        };
        assert_eq!(bytes.encode("é")?, vec![0xc3, 0xa9]);

        let vocab = PocketTokenizer {
            kind: TokenizerKind::Vocab {
                tokens: HashMap::from([("he".into(), 10), ("llo".into(), 11), ("h".into(), 12)]),
                max_chars: 3,
            },
            bos: Some(0),
            eos: Some(99),
            unk: Some(7),
        };
        assert_eq!(vocab.encode("hello!")?, vec![0, 10, 11, 7, 99]);

        let mut table = vec![0; 128];
        table[b'H' as usize] = 42;
        table[b'i' as usize] = 43;
        let indexer = PocketTokenizer {
            kind: TokenizerKind::Indexer(table),
            bos: None,
            eos: None,
            unk: None,
        };
        assert_eq!(indexer.encode("Hi")?, vec![42, 43]);
        assert!(matches!(indexer.encode("é"), Err(Error::Model(_))));
        Ok(())
    }

    #[test]
    fn pocket_tts_engine_rejects_unknown_voice() {
        struct Noop;
        impl PocketSynthesis for Noop {
            fn generate(
                &mut self,
                _: &Path,
                _: &str,
                _: &CancellationToken,
                _: &mut dyn FnMut(&[f32]) -> bool,
            ) -> Result<()> {
                Ok(())
            }
        }
        let mut pocket = Pocket {
            model: Noop,
            states: PathBuf::from("."),
        };
        let stop = CancellationToken::new();
        let err = pocket
            .stream("hello", "unknown_voice", 1.0, &stop, &mut |_| true)
            .unwrap_err();
        assert!(matches!(err, Error::Request(_)));
    }
}
