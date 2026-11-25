//! Markdown-to-speech sentence chunking and voice stop-command detection.

use std::sync::LazyLock;

use regex::Regex;

static HR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-*_=]{3,}$").expect("INFALLIBLE: static regex"));
static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#{1,6}\s+").expect("INFALLIBLE: static regex"));
static BLOCKQUOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^>\s?").expect("INFALLIBLE: static regex"));
static LIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?").expect("INFALLIBLE: static regex")
});
static IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").expect("INFALLIBLE: static regex"));
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").expect("INFALLIBLE: static regex"));
static ANGLE_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<https?://[^>]+>").expect("INFALLIBLE: static regex"));
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://\S+").expect("INFALLIBLE: static regex"));
static CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]*)`").expect("INFALLIBLE: static regex"));
static STRIKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~~(.+?)~~").expect("INFALLIBLE: static regex"));
static CLEANUP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[*#`|]+").expect("INFALLIBLE: static regex"));
static SPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("INFALLIBLE: static regex"));
static ABBREVIATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:e\.g|i\.e|etc|vs|approx|e\.g\.|i\.e\.|Mr|Mrs|Ms|Dr|No|cf)\.$")
        .expect("INFALLIBLE: static regex")
});
static LIST_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+[.)]$").expect("INFALLIBLE: static regex"));
static STOP_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:ok(?:ay)?|please|hey) stop$").expect("INFALLIBLE: static regex")
});

fn strip_bold_pairs(input: &str) -> String {
    let mut out = input.to_owned();
    for delim in ["**", "__"] {
        let mut next = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(start) = rest.find(delim) {
            let after_start = &rest[start + delim.len()..];
            if let Some(end) = after_start.find(delim)
                && end > 0
            {
                next.push_str(&rest[..start]);
                next.push_str(&after_start[..end]);
                rest = &after_start[end + delim.len()..];
                continue;
            }
            next.push_str(&rest[..start + delim.len()]);
            rest = after_start;
        }
        next.push_str(rest);
        out = next;
    }
    out
}

fn strip_italic_pairs(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let boundary_before = i == 0 || {
            let prev = input[..i].chars().next_back().unwrap_or(' ');
            prev.is_whitespace() || prev == '('
        };
        if (b == b'*' || b == b'_') && boundary_before {
            let delim = b as char;
            let after = &input[i + 1..];
            if let Some(first) = after.chars().next()
                && first != '*'
                && first != '_'
                && !first.is_whitespace()
                && let Some(rel_end) = after.find(delim)
                && rel_end > 0
            {
                let tail = &after[rel_end + 1..];
                let boundary_after = tail.is_empty()
                    || tail.chars().next().is_some_and(|c| {
                        c.is_whitespace() || matches!(c, ')' | '.' | ',' | '!' | '?' | ':' | ';')
                    });
                if boundary_after {
                    out.push_str(&after[..rel_end]);
                    i += 1 + rel_end + 1;
                    continue;
                }
            }
        }
        let ch = input[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Converts one Markdown sentence or line to plain speakable text; empty when nothing is worth saying.
pub fn speakable(markdown: &str) -> String {
    let trimmed = markdown.trim();
    if trimmed.starts_with('|') || HR_RE.is_match(trimmed) {
        return String::new();
    }
    let s = HEADING_RE.replace(trimmed, "");
    let s = BLOCKQUOTE_RE.replace(&s, "");
    let s = LIST_RE.replace(&s, "");
    let s = IMG_RE.replace_all(&s, "$1");
    let s = LINK_RE.replace_all(&s, "$1");
    let s = ANGLE_URL_RE.replace_all(&s, "a link");
    let s = URL_RE.replace_all(&s, "a link");
    let s = CODE_RE.replace_all(&s, "$1");
    let s = strip_bold_pairs(&s);
    let s = strip_italic_pairs(&s);
    let s = STRIKE_RE.replace_all(&s, "$1");
    let s = CLEANUP_RE.replace_all(&s, " ");
    let s = SPACE_RE.replace_all(&s, " ");
    let text = s.trim();
    if text.chars().any(char::is_alphanumeric) {
        text.to_owned()
    } else {
        String::new()
    }
}

#[derive(Default)]
pub struct SpeechChunker {
    position: usize,
    fence: bool,
}

impl SpeechChunker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns sentences of `text` (growing prefix) completed since the last call.
    pub fn push(&mut self, text: &str, final_chunk: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut emit = |raw: &str| {
            let sentence = speakable(raw);
            if !sentence.is_empty() {
                out.push(sentence);
            }
        };
        while self.position < text.len() {
            let position = self.position;
            let newline = text[position..].find('\n').map(|rel| position + rel);
            let complete = newline.is_some();
            let line_end = newline.unwrap_or(text.len());
            if position == 0 || text.as_bytes().get(position - 1) == Some(&b'\n') {
                let line = text[position..line_end].trim();
                if line.starts_with("```") || line.starts_with("~~~") {
                    if !complete && !final_chunk {
                        break;
                    }
                    self.fence = !self.fence;
                    self.position = newline.map_or(text.len(), |n| n + 1);
                    continue;
                }
            }
            if self.fence {
                if !complete && !final_chunk {
                    break;
                }
                self.position = newline.map_or(text.len(), |n| n + 1);
                continue;
            }
            if let Some(end) = self.sentence_end(text, position, line_end) {
                emit(&text[position..end]);
                self.position = end;
                while self.position < line_end {
                    let Some(ch) = text[self.position..].chars().next() else {
                        break;
                    };
                    if !ch.is_whitespace() {
                        break;
                    }
                    self.position += ch.len_utf8();
                }
                continue;
            }
            if let Some(nl) = newline {
                emit(&text[position..nl]);
                self.position = nl + 1;
                continue;
            }
            if final_chunk {
                emit(&text[position..]);
                self.position = text.len();
            }
            break;
        }
        out
    }

    fn sentence_end(&self, text: &str, start: usize, line_end: usize) -> Option<usize> {
        let line = &text[start..line_end];
        let mut idx = 0usize;
        while idx < line.len() {
            let ch = line[idx..].chars().next()?;
            if matches!(ch, '.' | '!' | '?' | '…') {
                let mut end = idx + ch.len_utf8();
                while end < line.len() {
                    let next = line[end..].chars().next()?;
                    if matches!(next, '.' | '!' | '?' | '…') {
                        end += next.len_utf8();
                    } else {
                        break;
                    }
                }
                while end < line.len() {
                    let next = line[end..].chars().next()?;
                    if matches!(next, '"' | '\'' | ')' | ']') {
                        end += next.len_utf8();
                    } else {
                        break;
                    }
                }
                let followed_by_ws = line[end..].chars().next().is_some_and(char::is_whitespace);
                if followed_by_ws {
                    let before = &line[..end];
                    let backticks = before.bytes().filter(|&b| b == b'`').count();
                    let at_line_start =
                        start == 0 || text.as_bytes().get(start - 1) == Some(&b'\n');
                    if backticks % 2 == 0
                        && !ABBREVIATION_RE.is_match(before)
                        && !(at_line_start && LIST_NUM_RE.is_match(before))
                    {
                        return Some(start + end);
                    }
                }
                idx = end;
                continue;
            }
            idx += ch.len_utf8();
        }
        None
    }
}

const STOP_PHRASES: [&str; 9] = [
    "stop",
    "stop it",
    "stop stop",
    "be quiet",
    "quiet",
    "shut up",
    "cancel",
    "never mind",
    "nevermind",
];

pub fn is_stop_command(text: &str) -> bool {
    let mut normalized = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch.is_whitespace() {
            for lower in ch.to_lowercase() {
                normalized.push(lower);
            }
        } else {
            normalized.push(' ');
        }
    }
    let collapsed = SPACE_RE.replace_all(&normalized, " ");
    let trimmed = collapsed.trim();
    STOP_PHRASES.contains(&trimmed) || STOP_PREFIX_RE.is_match(trimmed) || trimmed == "stop please"
}
