// Inference contract adapted from Supertone Inc. (2025), MIT. See THIRD-PARTY.md.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ort::session::Session;
use rand_distr::{Distribution, StandardNormal};
use serde::Deserialize;
use serde_json::Value;

use super::{model, output, session, tensor, view};
use crate::{
    Engine, EngineInfo, Error, Result, SentenceEngine, SentenceSynthesizer, Voice,
    text::{UnicodeProcessor, get_latent_mask},
};

#[derive(Deserialize)]
struct Config {
    ae: Audio,
    ttl: Latent,
}
#[derive(Deserialize)]
struct Audio {
    sample_rate: u32,
    base_chunk_size: usize,
}
#[derive(Deserialize)]
struct Latent {
    latent_dim: usize,
    chunk_compress_factor: usize,
}
#[derive(Deserialize)]
struct StyleSource {
    style_ttl: StyleChannel,
    style_dp: StyleChannel,
}
#[derive(Deserialize)]
struct StyleChannel {
    dims: [usize; 3],
    data: Value,
}
struct Style {
    ttl: ([usize; 3], Vec<f32>),
    dp: ([usize; 3], Vec<f32>),
}
struct Supertonic {
    config: Config,
    processor: UnicodeProcessor,
    duration: Session,
    encoder: Session,
    vector: Session,
    vocoder: Session,
    styles: BTreeMap<String, Style>,
    steps: usize,
}

pub(crate) fn load(base: &Path, threads: usize, steps: usize) -> Result<Engine> {
    let onnx = base.join("onnx");
    let config: Config = serde_json::from_reader(fs::File::open(onnx.join("tts.json"))?)?;
    let sample_rate = config.ae.sample_rate;
    let indexer = serde_json::from_reader(fs::File::open(onnx.join("unicode_indexer.json"))?)?;
    let mut styles = BTreeMap::new();
    for entry in fs::read_dir(base.join("voice_styles"))? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let raw: StyleSource = serde_json::from_reader(fs::File::open(&path)?)?;
        let voice = Style {
            ttl: channel(raw.style_ttl)?,
            dp: channel(raw.style_dp)?,
        };
        styles.insert(name.to_owned(), voice);
    }
    let voices = styles
        .keys()
        .map(|id| {
            let mut characters = id.chars();
            let kind = if characters.next() == Some('F') {
                "Female"
            } else {
                "Male"
            };
            Voice {
                id: id.clone(),
                name: format!("{kind} {}", characters.as_str()),
            }
        })
        .collect();
    let model = Supertonic {
        config,
        processor: UnicodeProcessor::new(indexer),
        duration: session(&onnx.join("duration_predictor.onnx"), threads)?,
        encoder: session(&onnx.join("text_encoder.onnx"), threads)?,
        vector: session(&onnx.join("vector_estimator.onnx"), threads)?,
        vocoder: session(&onnx.join("vocoder.onnx"), threads)?,
        styles,
        steps,
    };
    Engine::new(
        EngineInfo {
            id: "supertonic".into(),
            name: "Supertonic 3".into(),
            streaming: false,
            sample_rate,
            default_speed: 1.05,
            default_voice: "F1".into(),
            voices,
        },
        SentenceEngine::new(model, sample_rate),
    )
}

fn channel(channel: StyleChannel) -> Result<([usize; 3], Vec<f32>)> {
    fn collect(value: Value, floats: &mut Vec<f32>) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    collect(value, floats)?;
                }
            }
            Value::Number(value) => floats.push(
                value
                    .as_f64()
                    .ok_or_else(|| Error::Model("invalid voice style".into()))?
                    as f32,
            ),
            _ => return Err(Error::Model("invalid voice style".into())),
        }
        Ok(())
    }
    let mut floats = Vec::new();
    collect(channel.data, &mut floats)?;
    if channel.dims[0] != 1
        || channel
            .dims
            .into_iter()
            .try_fold(1usize, usize::checked_mul)
            != Some(floats.len())
    {
        return Err(Error::Model("voice style tensor shape mismatch".into()));
    }
    Ok((channel.dims, floats))
}

impl SentenceSynthesizer for Supertonic {
    fn sentence(&mut self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>> {
        let style = self
            .styles
            .get(voice)
            .ok_or_else(|| Error::Model(format!("missing Supertonic style for {voice}")))?;
        let ids = self.processor.encode(text, "en")?;
        let count = ids.len();
        let ids = tensor([1, count], ids)?;
        let mask = tensor([1, 1, count], vec![1.0f32; count])?;
        let dp = view(style.dp.0, &style.dp.1)?;
        let ttl = view(style.ttl.0, &style.ttl.1)?;
        let duration = output(model(self.duration.run(
            ort::inputs!["text_ids" => ids.view(), "style_dp" => dp, "text_mask" => mask.view()],
        ))?)?;
        let duration = *model(duration.try_extract_tensor::<f32>())?
            .1
            .first()
            .ok_or_else(|| Error::Model("missing Supertonic duration".into()))?
            / speed;
        if !duration.is_finite() || duration <= 0. {
            return Err(Error::Model("invalid Supertonic duration".into()));
        }
        let wav_len = (duration * self.config.ae.sample_rate as f32) as usize;
        let embedding = output(model(self.encoder.run(ort::inputs!["text_ids" => ids.view(), "style_ttl" => &*ttl, "text_mask" => mask.view()]))?)?;
        let config = &self.config;
        let (frames, latent_mask) = get_latent_mask(
            &[wav_len],
            config.ae.base_chunk_size,
            config.ttl.chunk_compress_factor,
        )?;
        let dim = config
            .ttl
            .latent_dim
            .checked_mul(config.ttl.chunk_compress_factor)
            .ok_or_else(|| Error::Model("invalid Supertonic latent dimension".into()))?;
        let mut rng = rand::rng();
        let noisy: Vec<f32> = (0..dim
            .checked_mul(frames)
            .ok_or_else(|| Error::Model("Supertonic latent tensor is too large".into()))?)
            .map(|_| StandardNormal.sample(&mut rng))
            .collect();
        let mut latent = tensor([1, dim, frames], noisy)?.into_dyn();
        let latent_mask = tensor([1, 1, frames], latent_mask)?;
        let steps = [self.steps as f32];
        let total = view([1], &steps)?;
        for index in 0..self.steps {
            let current = [index as f32];
            latent = output(model(self.vector.run(
                ort::inputs!["noisy_latent" => latent, "text_emb" => &embedding,
                "style_ttl" => &*ttl, "text_mask" => &mask, "latent_mask" => &latent_mask,
                "current_step" => view([1], &current)?, "total_step" => &*total],
            ))?)?;
        }
        let wave = output(model(self.vocoder.run(ort::inputs!["latent" => latent]))?)?;
        let (_, audio) = model(wave.try_extract_tensor::<f32>())?;
        let cropped = (f64::from(duration) * f64::from(config.ae.sample_rate)) as usize;
        Ok(audio[..audio.len().min(cropped)].to_vec())
    }
}
