// Supertonic normalization and chunking adapted from Supertone Inc. (2025), MIT.
// See THIRD-PARTY.md for source and license.
use std::sync::LazyLock;

use fancy_regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::{Error, Result};

const LANGUAGES: &[&str] = &[
    "en", "ko", "ja", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi", "hr", "hu",
    "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi", "na",
];

pub(crate) fn python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn preprocess(text: &str, lang: &str) -> Result<String> {
    if !LANGUAGES.contains(&lang) {
        return Err(Error::Request(format!("Invalid language: {lang}")));
    }
    let normalized: String = text
        .nfkd()
        .filter_map(|c| {
            Some(match c {
                '\u{1F1E6}'..='\u{1F1FF}'
                | '\u{1F300}'..='\u{1F64F}'
                | '\u{1F680}'..='\u{1FAFF}'
                | '\u{2600}'..='\u{26FF}'
                | '\u{2700}'..='\u{27BF}'
                | '©'
                | '\\' => return None,
                '–' | '‑' | '—' => '-',
                '_' | '[' | ']' | '|' | '/' | '#' | '→' | '←' => ' ',
                '“' | '”' => '"',
                '‘' | '’' | '´' | '`' => '\'',
                other => other,
            })
        })
        .collect();
    let replaced = normalized
        .replace('@', " at ")
        .replace("e.g.,", "for example, ")
        .replace("i.e.,", "that is, ");
    let mut spaced = String::with_capacity(replaced.len());
    for c in replaced.chars() {
        if matches!(c, ',' | '.' | '!' | '?' | ';' | ':' | '\'') && spaced.ends_with(' ') {
            spaced.pop();
        }
        if !(matches!(c, '"' | '\'' | '`') && spaced.ends_with(c)) {
            spaced.push(c);
        }
    }
    // Quote collapse follows the punctuation fixes, before whitespace is collapsed.
    let mut result = String::with_capacity(spaced.len() + 2 * lang.len() + 6);
    result.push('<');
    result.push_str(lang);
    result.push('>');
    let mut last = None;
    let mut parts = spaced
        .split(python_space)
        .filter(|s| !s.is_empty())
        .peekable();
    while let Some(part) = parts.next() {
        result.push_str(part);
        last = part.chars().next_back();
        if parts.peek().is_some() {
            result.push(' ');
        }
    }
    if !last.is_some_and(|c| ".!?;:,'\")]}…。」』】〉》›»".contains(c)) {
        result.push('.');
    }
    result.push_str("</");
    result.push_str(lang);
    result.push('>');
    Ok(result)
}

pub fn chunk_text(text: &str, max_len: usize) -> Result<Vec<String>> {
    // INFALLIBLE: these fixed expressions are exercised by the text contract tests.
    static PARAGRAPH: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\n[\s\x1c-\x1f]*\n+").expect("valid paragraph boundary")
    });
    // INFALLIBLE: only bounded lookbehinds over fixed ASCII abbreviations are used.
    static SENTENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?<!Mr\.)(?<!Mrs\.)(?<!Ms\.)(?<!Dr\.)(?<!Prof\.)(?<!Sr\.)(?<!Jr\.)(?<!Ph\.D\.)(?<!etc\.)(?<!e\.g\.)(?<!i\.e\.)(?<!vs\.)(?<!Inc\.)(?<!Ltd\.)(?<!Co\.)(?<!Corp\.)(?<!St\.)(?<!Ave\.)(?<!Blvd\.)(?<!\b[A-Z]\.)(?<=[.!?])[\s\x1c-\x1f]+").expect("valid sentence boundary")
    });
    let mut result = Vec::new();
    for paragraph in PARAGRAPH.split(text.trim_matches(python_space)) {
        let paragraph = paragraph.trim_matches(python_space);
        if paragraph.is_empty() {
            continue;
        }
        let mut chunk = String::new();
        let mut len = 0usize;
        let mut begin = 0;
        for boundary in SENTENCE.find_iter(paragraph) {
            let boundary = boundary.map_err(|e| Error::Model(e.to_string()))?;
            append_sentence(
                &mut result,
                &mut chunk,
                &mut len,
                &paragraph[begin..boundary.start()],
                max_len,
            );
            begin = boundary.end();
        }
        append_sentence(
            &mut result,
            &mut chunk,
            &mut len,
            &paragraph[begin..],
            max_len,
        );
        if !chunk.is_empty() {
            result.push(chunk);
        }
    }
    Ok(result)
}

fn append_sentence(
    result: &mut Vec<String>,
    chunk: &mut String,
    len: &mut usize,
    sentence: &str,
    max_len: usize,
) {
    let size = sentence.chars().count();
    if !chunk.is_empty() {
        if *len + size < max_len {
            chunk.push(' ');
            *len += 1;
        } else {
            result.push(std::mem::take(chunk));
            *len = 0;
        }
    }
    chunk.push_str(sentence);
    *len += size;
}

pub fn sentences(text: &str) -> Result<Vec<String>> {
    chunk_text(text, 0)
}

pub struct UnicodeProcessor {
    index: Vec<i64>,
}

impl UnicodeProcessor {
    pub fn new(index: Vec<i64>) -> Self {
        Self { index }
    }

    pub fn encode(&self, text: &str, lang: &str) -> Result<Vec<i64>> {
        preprocess(text, lang)?
            .chars()
            .map(|c| {
                // NumPy's uint16 constructor rejects astral scalars rather than masking off their high bits.
                let id = u16::try_from(u32::from(c)).map_err(|_| {
                    Error::Model(format!(
                        "Python integer {} out of bounds for uint16",
                        u32::from(c)
                    ))
                })?;
                self.index.get(usize::from(id)).copied().ok_or_else(|| {
                    Error::Model(format!("Unicode index has no entry for U+{:04X}", c as u32))
                })
            })
            .collect()
    }
}

/// Contiguous (batch, 1, width) float32 padding mask.
pub fn length_to_mask(lengths: &[usize], max_len: Option<usize>) -> (usize, Vec<f32>) {
    let width = max_len
        .filter(|&n| n != 0)
        .unwrap_or_else(|| lengths.iter().copied().max().unwrap_or(0));
    let mut mask = vec![0.; lengths.len() * width];
    for (&length, row) in lengths.iter().zip(mask.chunks_exact_mut(width.max(1))) {
        row[..length.min(width)].fill(1.0);
    }
    (width, mask)
}

pub fn get_latent_mask(
    wav_lengths: &[usize],
    base: usize,
    compression: usize,
) -> Result<(usize, Vec<f32>)> {
    let size = base
        .checked_mul(compression)
        .filter(|&s| s > 0)
        .ok_or_else(|| Error::Model("invalid Supertonic latent chunk size".into()))?;
    let lengths: Vec<_> = wav_lengths.iter().map(|&n| n.div_ceil(size)).collect();
    Ok(length_to_mask(&lengths, None))
}
