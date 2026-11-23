//! BM25 tool discovery with independent name, namespace, and description fields.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexSet;
use regex::Regex;

use crate::unicode::{nfc, words::for_each_word};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchDocument<'a> {
    pub name: &'a str,
    pub text: &'a str,
    pub namespace_text: Option<&'a str>,
}

struct IndexedDocument<'a> {
    name: &'a str,
    names: HashSet<String>,
    local: Vec<String>,
    text: HashMap<String, usize>,
    text_length: usize,
    namespace: HashSet<String>,
}

pub fn rank_documents<'a>(
    query: &str,
    documents: &[SearchDocument<'a>],
    limit: usize,
) -> Vec<&'a str> {
    if documents.is_empty() || limit == 0 {
        return Vec::new();
    }
    let terms: IndexSet<_> = tokenize(query).into_iter().collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let indexed: Vec<_> = documents
        .iter()
        .map(|document| {
            let text = tokenize(document.text);
            let text_length = text.len();
            let mut counts = HashMap::new();
            for term in text {
                *counts.entry(term).or_insert(0) += 1;
            }
            let local = document
                .name
                .rsplit_once("__")
                .map_or(document.name, |(_, name)| name);
            IndexedDocument {
                name: document.name,
                names: tokenize(document.name).into_iter().collect(),
                local: tokenize(local),
                text: counts,
                text_length,
                namespace: document
                    .namespace_text
                    .map(tokenize)
                    .into_iter()
                    .flatten()
                    .collect(),
            }
        })
        .collect();
    let total: usize = indexed.iter().map(|doc| doc.text_length).sum();
    let n = indexed.len() as f64;
    let avg = if total == 0 { 1.0 } else { total as f64 / n };
    let idf: Vec<_> = terms
        .iter()
        .map(|term| {
            let count = indexed
                .iter()
                .filter(|doc| {
                    doc.names.contains(term)
                        || doc.text.contains_key(term)
                        || doc.namespace.contains(term)
                })
                .count() as f64;
            (1.0 + (n - count + 0.5) / (count + 0.5)).ln()
        })
        .collect();
    let full_name_bonus = idf.iter().sum::<f64>();
    let exact = normalize_name(query);
    let mut matches = Vec::new();
    for doc in indexed {
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let norm = K1 * (1.0 - B + B * doc.text_length as f64 / avg);
        let mut score = 0.0;
        let mut covered = 0;
        for (term, idf) in terms.iter().zip(&idf) {
            let count = doc.text.get(term).copied().unwrap_or(0) as f64;
            let weight = (if doc.names.contains(term) { 3.0 } else { 0.0 })
                + (if doc.namespace.contains(term) {
                    0.5
                } else {
                    0.0
                })
                + count * (K1 + 1.0) / (count + norm);
            covered += usize::from(weight > 0.0);
            score += idf * weight;
        }
        if covered == 0 {
            continue;
        }
        if !doc.local.is_empty() && doc.local.iter().all(|term| terms.contains(term)) {
            score += full_name_bonus;
        }
        let normalized = normalize_name(doc.name);
        let is_exact = normalized == exact
            || normalized
                .strip_suffix(&exact)
                .is_some_and(|prefix| prefix.ends_with("__"));
        matches.push((doc.name, is_exact, covered, score));
    }
    // A stable sort preserves provider registration order for tied scores.
    matches.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.total_cmp(&a.3))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(name, ..)| name)
        .collect()
}

fn normalize_name(name: &str) -> String {
    name.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
        .to_lowercase()
        .encode_utf16()
        .map(|unit| {
            let ascii = unit as u8;
            if unit < 128 && (ascii.is_ascii_alphanumeric() || matches!(ascii, b'_' | b'$')) {
                char::from(ascii)
            } else {
                '_'
            }
        })
        .collect()
}

fn boundary(previous: u8, current: u8, next: u8) -> bool {
    current.is_ascii_uppercase()
        && (previous.is_ascii_lowercase()
            || previous.is_ascii_digit()
            || (previous.is_ascii_uppercase() && next.is_ascii_lowercase()))
}

fn push_word(word: &str, terms: &mut Vec<String>) {
    if matches!(
        word,
        "" | "a"
            | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "with"
    ) {
        return;
    }
    let term = if word.is_ascii() && word.len() > 3 {
        if word.len() > 4
            && let Some(stem) = word.strip_suffix("ies")
        {
            terms.push(format!("{stem}y"));
            return;
        }
        if word.len() > 4
            && ["ches", "shes", "sses", "xes", "zes"]
                .iter()
                .any(|suffix| word.ends_with(suffix))
        {
            &word[..word.len() - 2]
        } else if word.ends_with('s') && !word.ends_with("ss") {
            &word[..word.len() - 1]
        } else {
            word
        }
    } else {
        word
    };
    terms.push(term.to_owned());
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    if text.is_ascii() {
        let mut start = 0;
        let bytes = text.as_bytes();
        let mut word = |part: &str| {
            if part.bytes().any(|byte| byte.is_ascii_uppercase()) {
                push_word(&part.to_ascii_lowercase(), &mut terms);
            } else {
                push_word(part, &mut terms);
            }
        };
        for (index, &byte) in bytes.iter().enumerate() {
            if !byte.is_ascii_alphanumeric() {
                word(&text[start..index]);
                start = index + 1;
            } else if index > start
                && boundary(
                    bytes[index - 1],
                    byte,
                    bytes.get(index + 1).copied().unwrap_or(0),
                )
            {
                word(&text[start..index]);
                start = index;
            }
        }
        word(&text[start..]);
        return terms;
    }
    let normalized = nfc(text);
    let bytes = normalized.as_bytes();
    let mut spaced = String::with_capacity(normalized.len());
    let mut start = 0;
    for index in 1..bytes.len() {
        if boundary(
            bytes[index - 1],
            bytes[index],
            bytes.get(index + 1).copied().unwrap_or(0),
        ) {
            spaced.push_str(&normalized[start..index]);
            spaced.push(' ');
            start = index;
        }
    }
    spaced.push_str(&normalized[start..]);
    static WORD_RUN: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: These are standard literal Unicode general categories.
        Regex::new(r"[\p{L}\p{M}\p{Nd}\p{Nl}]+").expect("static search word pattern")
    });
    let lower = spaced.to_lowercase();
    for part in WORD_RUN.find_iter(&lower) {
        for_each_word(part.as_str(), |word| push_word(word, &mut terms));
    }
    terms
}
