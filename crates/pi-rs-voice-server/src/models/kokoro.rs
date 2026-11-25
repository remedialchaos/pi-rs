use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::Path;

use ort::session::{Session, SessionInputValue};
use ort::value::TensorElementType;
use serde::Deserialize;

use super::{model, output, phonemizer::phonemize, session, tensor, view};
use crate::{Engine, EngineInfo, Error, Result, SentenceEngine, SentenceSynthesizer, Voice};

const RATE: u32 = 24_000;
const WIDTH: usize = 256;
const MAX_PHONEMES: usize = 510;
const SENTENCE_MARKS: &str = ".!?…";
const CLAUSE_MARKS: &str = ",;:";
const VOICES: &[(&str, &str)] = &[
    ("af_heart", "Heart (US female)"),
    ("af_bella", "Bella (US female)"),
    ("af_nicole", "Nicole (US female)"),
    ("af_sarah", "Sarah (US female)"),
    ("am_michael", "Michael (US male)"),
    ("am_adam", "Adam (US male)"),
    ("am_fenrir", "Fenrir (US male)"),
    ("bf_emma", "Emma (UK female)"),
    ("bf_isabella", "Isabella (UK female)"),
    ("bm_george", "George (UK male)"),
    ("bm_lewis", "Lewis (UK male)"),
    ("bm_fable", "Fable (UK male)"),
];

#[derive(Deserialize)]
struct Vocabulary {
    vocab: HashMap<char, i64>,
}

struct Kokoro {
    session: Session,
    styles: HashMap<&'static str, Vec<f32>>,
    vocab: HashMap<char, i64>,
    token_name: String,
    token_type: TensorElementType,
    speed_type: TensorElementType,
}

pub(super) fn load(base: &Path, threads: usize) -> Result<Engine> {
    let session = session(&base.join("kokoro-v1.0.onnx"), threads)?;
    let metadata = model(session.metadata())?;
    let vocab = metadata
        .custom("kokoro_config")
        .and_then(|config| serde_json::from_str::<Vocabulary>(&config).ok())
        .map_or_else(
            || serde_json::from_str::<Vocabulary>(include_str!("kokoro_vocab.json")),
            Ok,
        )?
        .vocab;
    drop(metadata);
    let tokens = session
        .inputs()
        .iter()
        .find(|v| matches!(v.name(), "input_ids" | "tokens"))
        .ok_or_else(|| Error::Model("missing Kokoro token input".into()))?;
    let token_name = tokens.name().to_owned();
    let token_type = tokens
        .dtype()
        .tensor_type()
        .ok_or_else(|| Error::Model("invalid Kokoro token input".into()))?;
    let speed_type = session
        .inputs()
        .iter()
        .find(|v| v.name() == "speed")
        .and_then(|v| v.dtype().tensor_type())
        .ok_or_else(|| Error::Model("missing Kokoro speed input".into()))?;
    let mut archive = model(zip::ZipArchive::new(File::open(
        base.join("voices-v1.0.bin"),
    )?))?;
    let mut styles = HashMap::new();
    let mut voices = Vec::new();
    let mut bytes = Vec::new();
    for &(id, name) in VOICES {
        match archive.by_name(&format!("{id}.npy")) {
            Ok(mut entry) => {
                bytes.clear();
                entry.read_to_end(&mut bytes)?;
                styles.insert(id, read_npy(&bytes)?);
                voices.push(Voice {
                    id: id.into(),
                    name: name.into(),
                });
            }
            Err(zip::result::ZipError::FileNotFound) => {}
            Err(error) => return Err(Error::Model(error.to_string())),
        }
    }
    let model = Kokoro {
        session,
        styles,
        vocab,
        token_name,
        token_type,
        speed_type,
    };
    Engine::new(
        EngineInfo {
            id: "kokoro".into(),
            name: "Kokoro".into(),
            streaming: false,
            sample_rate: RATE,
            default_speed: 1.,
            default_voice: "af_heart".into(),
            voices,
        },
        SentenceEngine::new(model, RATE),
    )
}

fn read_npy(bytes: &[u8]) -> Result<Vec<f32>> {
    let bad = || Error::Model("invalid Kokoro NPY voice style".into());
    let Some((version, rest)) = bytes
        .strip_prefix(b"\x93NUMPY")
        .and_then(<[u8]>::split_first_chunk::<2>)
    else {
        return Err(bad());
    };
    let (size, payload) = if version == &[1, 0] {
        let (size, rest) = rest.split_first_chunk::<2>().ok_or_else(bad)?;
        (usize::from(u16::from_le_bytes(*size)), rest)
    } else if matches!(version, [2 | 3, 0]) {
        let (size, rest) = rest.split_first_chunk::<4>().ok_or_else(bad)?;
        (u32::from_le_bytes(*size) as usize, rest)
    } else {
        return Err(bad());
    };
    let (header, payload) = payload.split_at_checked(size).ok_or_else(bad)?;
    let values = std::str::from_utf8(header).map_err(|_| bad())?;
    let shape = values
        .split_once("'shape':")
        .and_then(|(_, tail)| tail.split_once('('))
        .and_then(|(_, tail)| tail.split_once(')'))
        .map(|(shape, _)| shape)
        .ok_or_else(bad)?;
    let shape: Vec<usize> = shape
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| bad())?;
    if !values.contains("'<f4'")
        || !values.contains("'fortran_order': False")
        || !matches!(shape.as_slice(), [n, 1, WIDTH] if *n > 0 && n.checked_mul(WIDTH * 4) == Some(payload.len()))
    {
        return Err(bad());
    }
    Ok(payload
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(f32::from_le_bytes)
        .collect())
}

impl SentenceSynthesizer for Kokoro {
    fn sentence(&mut self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>> {
        let phonemes = phonemize(text, voice.starts_with('b'))?;
        let phonemes: String = phonemes
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| self.vocab.contains_key(c))
            .collect();
        if phonemes.is_empty() {
            return Err(Error::Model(format!(
                "Nothing to synthesize, {text:?} produced no phonemes"
            )));
        }
        let batches = batches(&phonemes);
        let styles = self
            .styles
            .get(voice)
            .ok_or_else(|| Error::Model(format!("missing Kokoro style for {voice}")))?;
        let mut audio = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            let mut tokens = Vec::with_capacity(batch.chars().count() + 2);
            tokens.push(0);
            tokens.extend(batch.chars().filter_map(|c| self.vocab.get(&c)));
            let n = tokens.len() - 1;
            tokens.push(0);
            let len = tokens.len();
            let row = (n.min(styles.len() / WIDTH) - 1) * WIDTH;
            let style = view([1, WIDTH], &styles[row..row + WIDTH])?;
            let tokens = match self.token_type {
                TensorElementType::Int64 => SessionInputValue::from(tensor([1, len], tokens)?),
                TensorElementType::Int32 => SessionInputValue::from(tensor(
                    [1, len],
                    tokens.into_iter().map(|id| id as i32).collect(),
                )?),
                _ => return Err(Error::Model("unsupported Kokoro token type".into())),
            };
            let speed_value = [speed];
            let rounded = speed.round_ties_even().max(1.0);
            let speed = match self.speed_type {
                TensorElementType::Float32 => SessionInputValue::from(view([1], &speed_value)?),
                TensorElementType::Int32 => {
                    SessionInputValue::from(tensor([1], vec![rounded as i32])?)
                }
                TensorElementType::Int64 => {
                    SessionInputValue::from(tensor([1], vec![rounded as i64])?)
                }
                _ => return Err(Error::Model("unsupported Kokoro speed type".into())),
            };
            let wave = output(model(self.session.run(ort::inputs![self.token_name.as_str() => tokens, "style" => style, "speed" => speed]))?)?;
            let values = model(wave.try_extract_tensor::<f32>())?.1;
            audio.extend_from_slice(&values[trim(values)]);
            if index + 1 < batches.len() {
                let pause = match batch.chars().next_back() {
                    Some(c) if SENTENCE_MARKS.contains(c) => 6000,
                    Some(c) if CLAUSE_MARKS.contains(c) => 2400,
                    _ => 0,
                };
                audio.resize(audio.len() + pause, 0.);
            }
        }
        Ok(audio)
    }
}

fn batches(phonemes: &str) -> Vec<String> {
    fn split<'a>(input: &'a str, next: usize, atoms: &mut Vec<&'a str>) {
        if input.chars().count() <= MAX_PHONEMES {
            if !input.is_empty() {
                atoms.push(input);
            }
            return;
        }
        for level in next..3 {
            let marks = match level {
                0 => SENTENCE_MARKS,
                1 => CLAUSE_MARKS,
                _ => "",
            };
            let mut start = 0;
            let mut previous = None;
            for (i, c) in input.char_indices() {
                if c == ' ' && (level == 2 || previous.is_some_and(|c| marks.contains(c))) {
                    split(&input[start..i], level + 1, atoms);
                    start = i + 1;
                }
                previous = Some(c);
            }
            if start > 0 {
                split(&input[start..], level + 1, atoms);
                return;
            }
        }
        let mut start = 0;
        for (n, (i, _)) in input.char_indices().enumerate() {
            if n != 0 && n % MAX_PHONEMES == 0 {
                atoms.push(&input[start..i]);
                start = i;
            }
        }
        atoms.push(&input[start..]);
    }
    fn pack(lengths: &[usize], limit: usize) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut total = 0;
        for (i, &size) in lengths.iter().enumerate() {
            if i != start && total + 1 + size > limit {
                out.push(start..i);
                start = i;
                total = 0;
            }
            total += usize::from(i != start) + size;
        }
        if !lengths.is_empty() {
            out.push(start..lengths.len());
        }
        out
    }
    let mut atoms = Vec::new();
    split(phonemes, 0, &mut atoms);
    let lengths: Vec<_> = atoms.iter().map(|s| s.chars().count()).collect();
    let mut low = lengths.iter().copied().max().unwrap_or(0);
    let mut high = MAX_PHONEMES;
    let fewest = pack(&lengths, high).len();
    while low < high {
        let mid = (low + high) / 2;
        if pack(&lengths, mid).len() <= fewest {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    pack(&lengths, low)
        .into_iter()
        .map(|range| atoms[range].join(" "))
        .collect()
}

fn trim(samples: &[f32]) -> Range<usize> {
    let mut energy = Vec::with_capacity(samples.len() / 512 + 1);
    let mut window = 0..1024.min(samples.len());
    let power = |values: &[f32]| {
        values
            .iter()
            .map(|&f| f64::from(f) * f64::from(f))
            .sum::<f64>()
    };
    let mut sum = power(&samples[window.clone()]);
    let mut max = sum;
    for center in (0..=samples.len()).step_by(512) {
        let next = center.saturating_sub(1024)..(center + 1024).min(samples.len());
        sum += power(&samples[window.end..next.end]) - power(&samples[window.start..next.start]);
        window = next;
        max = max.max(sum);
        energy.push(sum);
    }
    let reference = (max / 2048.).max(1e-10).log10();
    let loud = |e: &f64| 10. * ((e / 2048.).max(1e-10).log10() - reference) > -60.;
    let start = energy.iter().position(loud).map_or(0, |i| 512 * i);
    let end = energy
        .iter()
        .rposition(loud)
        .map_or(0, |i| (512 * (i + 1)).min(samples.len()));
    start..end
}
