use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ort::session::Session;
use ort::value::{DynValue, TensorElementType};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use super::{lock, model, output, session, tensor, view};
use crate::{Error, Result, SttRecognizer};

const FRAME: usize = 400;
const HOP: usize = 160;
const FFT: usize = 512;

pub struct Parakeet {
    encoder: Mutex<Session>,
    decoder: Mutex<Session>,
    joiner: Mutex<Session>,
    features: LogMel,
    tokens: Vec<String>,
    blank: usize,
    state: [usize; 3],
    encoder_length_type: TensorElementType,
}

impl Parakeet {
    pub fn load(path: &Path) -> Result<Self> {
        let encoder = session(&path.join("encoder.int8.onnx"), 4)?;
        let decoder = session(&path.join("decoder.int8.onnx"), 4)?;
        let joiner = session(&path.join("joiner.int8.onnx"), 4)?;
        let metadata = model(encoder.metadata())?;
        let usize_field = |key: &str| -> Result<usize> {
            metadata
                .custom(key)
                .and_then(|value| value.parse().ok())
                .filter(|&v| v > 0)
                .ok_or_else(|| Error::Model(format!("invalid Parakeet encoder metadata {key}")))
        };
        let blank = usize_field("vocab_size")?;
        let channels = usize_field("feat_dim")?;
        let state = [
            usize_field("pred_rnn_layers")?,
            1,
            usize_field("pred_hidden")?,
        ];
        let normalize = match metadata.custom("normalize_type").as_deref() {
            Some("per_feature") => true,
            None | Some("" | "NA") => false,
            Some(other) => {
                return Err(Error::Model(format!(
                    "unsupported NeMo normalization {other}"
                )));
            }
        };
        let encoder_length_type = encoder
            .inputs()
            .get(1)
            .and_then(|v| v.dtype().tensor_type())
            .ok_or_else(|| Error::Model("missing Parakeet encoder length tensor".into()))?;
        drop(metadata);
        let mut tokens = vec![String::new(); blank + 1];
        for line in fs::read_to_string(path.join("tokens.txt"))?.lines() {
            let (token, id) = line
                .rsplit_once(' ')
                .ok_or_else(|| Error::Model("invalid Parakeet token row".into()))?;
            let id = id
                .parse::<usize>()
                .ok()
                .filter(|&id| id <= blank)
                .ok_or_else(|| Error::Model("invalid Parakeet token ID".into()))?;
            tokens[id] = token.replace('▁', " ");
        }
        if tokens.get(blank).map(String::as_str) != Some("<blk>") {
            return Err(Error::Model(
                "Parakeet tokens.txt does not end with the blank token".into(),
            ));
        }
        Ok(Self {
            encoder: Mutex::new(encoder),
            decoder: Mutex::new(decoder),
            joiner: Mutex::new(joiner),
            features: LogMel::new(channels, normalize),
            tokens,
            blank,
            state,
            encoder_length_type,
        })
    }

    fn decode(
        &self,
        id: usize,
        recurrent: (DynValue, DynValue),
    ) -> Result<(DynValue, (DynValue, DynValue))> {
        let target = [id as i32];
        let (h, c) = recurrent;
        let mut decoder = lock(&self.decoder)?;
        let result = model(decoder.run(ort::inputs![
            view([1, 1], &target)?,
            view([1], &[1i32])?,
            h,
            c
        ]))?;
        let mut result = result.into_iter().map(|(_, value)| value);
        let bad = || Error::Model("incomplete Parakeet decoder outputs".into());
        let predicted = result.next().ok_or_else(bad)?;
        let _length = result.next().ok_or_else(bad)?;
        Ok((
            predicted,
            (
                result.next().ok_or_else(bad)?,
                result.next().ok_or_else(bad)?,
            ),
        ))
    }
}

impl SttRecognizer for Parakeet {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let (features, frames) = self.features.compute(samples);
        if frames == 0 {
            return Ok(String::new());
        }
        let length = match self.encoder_length_type {
            TensorElementType::Int64 => tensor([1], vec![frames as i64])?.into_dyn(),
            TensorElementType::Int32 => tensor([1], vec![frames as i32])?.into_dyn(),
            _ => return Err(Error::Model("invalid Parakeet encoder length type".into())),
        };
        let (encoded, length) = {
            let mut encoder = lock(&self.encoder)?;
            let result = model(encoder.run(ort::inputs![
                tensor([1, self.features.filters.len(), frames], features)?,
                length
            ]))?;
            let mut result = result.into_iter().map(|(_, value)| value);
            let bad = || Error::Model("incomplete Parakeet encoder outputs".into());
            (
                result.next().ok_or_else(bad)?,
                result.next().ok_or_else(bad)?,
            )
        };
        let (shape, values) = model(encoded.try_extract_tensor::<f32>())?;
        let &[1, depth, stride] = shape.as_ref() else {
            return Err(Error::Model("invalid Parakeet encoder output shape".into()));
        };
        let bad_length = || Error::Model("invalid Parakeet output length".into());
        let frames = if let Ok((_, values)) = length.try_extract_tensor::<i64>() {
            *values.first().ok_or_else(bad_length)?
        } else {
            i64::from(
                *model(length.try_extract_tensor::<i32>())?
                    .1
                    .first()
                    .ok_or_else(bad_length)?,
            )
        };
        if !(0..=stride).contains(&frames) || stride <= 0 || depth <= 0 {
            return Err(bad_length());
        }
        let depth = depth as usize;
        let stride = stride as usize;
        let state_len: usize = self.state.iter().product();
        let mut decoded = self.decode(
            self.blank,
            (
                tensor(self.state, vec![0f32; state_len])?.into_dyn(),
                tensor(self.state, vec![0f32; state_len])?.into_dyn(),
            ),
        )?;
        let mut gathered = vec![0.; depth];
        let mut transcript = String::new();
        let mut frame = 0;
        let mut tokens_here = 0;
        while frame < frames as usize {
            for (channel, f) in gathered.iter_mut().enumerate() {
                *f = values[channel * stride + frame];
            }
            let logits = {
                let mut joiner = lock(&self.joiner)?;
                output(model(joiner.run(ort::inputs![
                    view([1, depth, 1], &gathered)?,
                    &decoded.0
                ]))?)?
            };
            let values = model(logits.try_extract_tensor::<f32>())?.1;
            let (tokens, durations) = values
                .split_at_checked(self.blank + 1)
                .ok_or_else(|| Error::Model("invalid Parakeet joiner logits".into()))?;
            let id =
                argmax(tokens).ok_or_else(|| Error::Model("empty Parakeet token logits".into()))?;
            let mut skip = argmax(durations)
                .ok_or_else(|| Error::Model("empty Parakeet duration logits".into()))?;
            if id != self.blank {
                transcript.push_str(&self.tokens[id]);
                decoded = self.decode(id, decoded.1)?;
                tokens_here += 1;
            }
            if skip == 0 && (tokens_here == 5 || id == self.blank) {
                skip = 1;
            }
            if skip > 0 {
                tokens_here = 0;
            }
            frame += skip;
        }
        Ok(format_transcript(transcript))
    }
}

fn format_transcript(mut text: String) -> String {
    // sherpa-onnx removes a SentencePiece space between CJK characters and before punctuation.
    let cjk = |c| {
        matches!(c,
        '\u{1100}'..='\u{11ff}' | '\u{2e80}'..='\u{a4cf}' | '\u{a840}'..='\u{d7af}' |
        '\u{f900}'..='\u{faff}' | '\u{fe30}'..='\u{fe4f}' | '\u{ff65}'..='\u{ffdc}' |
        '\u{20000}'..='\u{2ffff}')
    };
    let punct = |c: char| {
        c.is_ascii_punctuation()
            || matches!(c,
        '\u{3000}'..='\u{303f}' | '\u{ff01}'..='\u{ff0f}' | '\u{ff1a}'..='\u{ff20}' |
        '\u{ff3b}'..='\u{ff40}' | '\u{ff5b}'..='\u{ff65}')
    };
    text.truncate(text.trim_end_matches(crate::text::python_space).len());
    let start = text.len() - text.trim_start_matches(crate::text::python_space).len();
    let mut chars = text[start..].char_indices().peekable();
    let mut cut = start;
    let mut previous = None;
    let mut output = None;
    while let Some((i, c)) = chars.next() {
        if c == ' '
            && chars
                .peek()
                .is_some_and(|&(_, next)| punct(next) || (cjk(next) && previous.is_some_and(cjk)))
        {
            let pos = start + i;
            output
                .get_or_insert_with(|| String::with_capacity(text.len() - start))
                .push_str(&text[cut..pos]);
            cut = pos + 1;
        }
        previous = Some(c);
    }
    if let Some(mut output) = output {
        output.push_str(&text[cut..]);
        return output;
    }
    text.drain(..start);
    text
}

fn argmax(values: &[f32]) -> Option<usize> {
    let (first, rest) = values.split_first()?;
    let mut index = 0;
    let mut best = *first;
    for (i, &value) in rest.iter().enumerate() {
        if value > best {
            index = i + 1;
            best = value;
        }
    }
    Some(index)
}

struct LogMel {
    fft: Arc<dyn Fft<f32>>,
    weights: [f32; FRAME],
    filters: Vec<(usize, Vec<f32>)>,
    normalize: bool,
}

impl LogMel {
    fn new(channels: usize, normalize: bool) -> Self {
        // sherpa-onnx v1.13.8 uses Kaldi's reflected window placement, pre-emphasis and
        // Povey window with librosa-compatible Slaney mel filters for this NeMo export.
        let weights = std::array::from_fn(|i| {
            (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (FRAME - 1) as f64).cos()).powf(0.85)
                as f32
        });
        let ln_step = 6.4_f64.ln() / 27.;
        let max_mel = 15. + 7.6_f64.ln() / ln_step;
        let hz = |index: usize| {
            let mel = max_mel * index as f64 / (channels + 1) as f64;
            if mel < 15. {
                mel * (200. / 3.)
            } else {
                1000. * (ln_step * (mel - 15.)).exp()
            }
        };
        let filters = (0..channels)
            .map(|i| {
                let (left, center, right) = (hz(i), hz(i + 1), hz(i + 2));
                let step = 16_000. / FFT as f64;
                let begin = (left / step).ceil() as usize;
                let end = ((right / step).floor() as usize + 1).min(FFT / 2 + 1);
                let weights = (begin..end)
                    .map(|bin| {
                        let f = bin as f64 * step;
                        (((f - left) / (center - left))
                            .min((right - f) / (right - center))
                            .max(0.)
                            * 2.
                            / (right - left)) as f32
                    })
                    .collect();
                (begin, weights)
            })
            .collect();
        Self {
            fft: FftPlanner::new().plan_fft_forward(FFT),
            weights,
            filters,
            normalize,
        }
    }

    fn compute(&self, samples: &[f32]) -> (Vec<f32>, usize) {
        let frames = (samples.len() + HOP / 2) / HOP;
        let mut features = vec![0.; frames * self.filters.len()];
        if frames == 0 {
            return (features, 0);
        }
        let mut fft = [Complex32::ZERO; FFT];
        let mut power = [0.; FFT / 2 + 1];
        let mut scratch = vec![Complex32::ZERO; self.fft.get_inplace_scratch_len()];
        for i in 0..frames {
            let start = (i * HOP + HOP / 2) as isize - FRAME as isize / 2;
            let mut previous = reflected(samples, start);
            for (offset, (&weight, bin)) in self.weights.iter().zip(&mut fft).enumerate() {
                let sample = reflected(samples, start + offset as isize);
                *bin = Complex32::new((sample - 0.97 * previous) * weight, 0.);
                previous = sample;
            }
            fft[FRAME..].fill(Complex32::ZERO);
            self.fft.process_with_scratch(&mut fft, &mut scratch);
            for (p, bin) in power.iter_mut().zip(fft.iter()) {
                *p = bin.norm_sqr();
            }
            for (j, (begin, weights)) in self.filters.iter().enumerate() {
                let sum: f32 = power[*begin..]
                    .iter()
                    .zip(weights)
                    .map(|(&bin, &weight)| bin * weight)
                    .sum();
                features[j * frames + i] = sum.max(f32::EPSILON).ln();
            }
        }
        if self.normalize {
            for series in features.chunks_exact_mut(frames) {
                let mean = series.iter().sum::<f32>() / frames as f32;
                let variance = series
                    .iter()
                    .map(|&sample| (sample - mean).powi(2))
                    .sum::<f32>()
                    / frames as f32;
                let inverse = (variance.sqrt() + 1e-5).recip();
                for sample in series {
                    *sample = (*sample - mean) * inverse;
                }
            }
        }
        (features, frames)
    }
}

fn reflected(samples: &[f32], index: isize) -> f32 {
    let n = samples.len() as isize;
    if (0..n).contains(&index) {
        return samples[index as usize];
    }
    let i = index.rem_euclid(2 * n);
    samples[if i < n { i } else { 2 * n - 1 - i } as usize]
}

#[cfg(test)]
mod tests {
    use super::format_transcript;

    #[test]
    fn parakeet_joins_cjk_and_punctuation_without_collapsing_ordinary_word_spaces() {
        assert_eq!(
            format_transcript("  Hello there , 世 界 ！ नमस्ते world .  ".into()),
            "Hello there, 世界！ नमस्ते world."
        );
    }
}
