//! Bounded UTF-16 output with streaming UTF-8 decoding and raw-stream accounting.
use crate::{Error, Result, json::JsonString};
use std::collections::VecDeque;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retain {
    Head,
    Tail,
}
#[derive(Clone, Copy, Debug)]
pub struct OutputLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub retain: Retain,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSlice {
    pub text: JsonString,
    pub bytes: usize,
    pub dropped_bytes: usize,
    pub dropped_lines: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: JsonString,
    pub dropped_bytes: usize,
    pub dropped_lines: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct OutputSkip {
    pub bytes: usize,
    pub newlines: usize,
    pub ends_with_newline: bool,
}
pub const PROGRESS_BYTES_PER_SECOND: usize = 100 * 1024;
pub fn sanitize_output(text: &JsonString) -> JsonString {
    let text = &text.0;
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let mut end = None;
        if text[i] == 27
            && let Some(&next) = text.get(i + 1)
        {
            if [93, 80, 88, 94, 95].contains(&next) {
                let mut j = i + 2;
                while j < text.len() && text[j] != 7 && text[j] != 27 {
                    j += 1;
                }
                if text.get(j) == Some(&7) {
                    end = Some(j + 1);
                } else if text.get(j..j + 2) == Some(&[27, 92]) {
                    end = Some(j + 2);
                }
            }
            if end.is_none() && next == 91 {
                let mut j = i + 2;
                while text.get(j).is_some_and(|c| (0x30..=0x3f).contains(c)) {
                    j += 1;
                }
                while text.get(j).is_some_and(|c| (0x20..=0x2f).contains(c)) {
                    j += 1;
                }
                if text.get(j).is_some_and(|c| (0x40..=0x7e).contains(c)) {
                    end = Some(j + 1);
                }
            }
            if end.is_none() {
                let mut j = i + 1;
                while text.get(j).is_some_and(|c| (0x20..=0x2f).contains(c)) {
                    j += 1;
                }
                if text.get(j).is_some_and(|c| (0x30..=0x7e).contains(c)) {
                    end = Some(j + 1);
                }
            }
        }
        if let Some(next) = end {
            i = next;
            continue;
        }
        let c = text[i];
        if !((0..=8).contains(&c) || (11..=31).contains(&c) || (0xfff9..=0xfffb).contains(&c)) {
            out.push(c);
        }
        i += 1;
    }
    JsonString(out)
}
pub fn utf8_byte_length(text: &JsonString) -> usize {
    String::from_utf16_lossy(&text.0).len()
}
pub fn character_end(bytes: &[u8], mut index: usize) -> usize {
    while index > 0 && bytes.get(index).is_some_and(|b| b & 0xc0 == 0x80) {
        index -= 1;
    }
    index
}
fn character_start(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] & 0xc0 == 0x80 {
        index += 1;
    }
    index
}
fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
}
pub fn bound_output(text: &JsonString, limits: OutputLimits) -> OutputSlice {
    let decoded = String::from_utf16_lossy(&text.0);
    let bytes = decoded.as_bytes();
    let mut from = 0;
    let mut to = bytes.len();
    if limits.max_bytes == 0 || limits.max_lines == 0 {
        to = 0;
    } else if limits.retain == Retain::Head {
        let mut lines = 0;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                lines += 1;
                if lines == limits.max_lines {
                    to = i + 1;
                    break;
                }
            }
        }
        if to > limits.max_bytes {
            to = bytes[..limits.max_bytes]
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or_else(|| character_end(bytes, limits.max_bytes), |i| i + 1);
        }
    } else {
        let mut lines = 1;
        let end = bytes
            .len()
            .saturating_sub(usize::from(bytes.last() == Some(&b'\n')));
        for i in (0..end).rev() {
            if bytes[i] == b'\n' {
                if lines == limits.max_lines {
                    from = i + 1;
                    break;
                }
                lines += 1;
            }
        }
        if bytes.len() - from > limits.max_bytes {
            let start = bytes.len() - limits.max_bytes;
            from = match bytes[start - 1..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + start - 1)
            {
                Some(i) if i + 1 < bytes.len() => i + 1,
                _ => character_start(bytes, start),
            };
        }
    }
    let kept = &bytes[from..to];
    OutputSlice {
        text: if kept.len() == bytes.len() {
            text.clone()
        } else {
            String::from_utf8_lossy(kept).as_ref().into()
        },
        bytes: kept.len(),
        dropped_bytes: bytes.len() - kept.len(),
        dropped_lines: line_count(bytes) - line_count(kept),
    }
}
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}
impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8], flush: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut pos = 0;
        while pos < self.pending.len() {
            match std::str::from_utf8(&self.pending[pos..]) {
                Ok(s) => {
                    out.push_str(s);
                    pos = self.pending.len();
                }
                Err(e) => {
                    let end = pos + e.valid_up_to();
                    if let Ok(valid) = std::str::from_utf8(&self.pending[pos..end]) {
                        out.push_str(valid);
                    }
                    pos = end;
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{fffd}');
                            pos += n;
                        }
                        None => {
                            if flush {
                                out.push('\u{fffd}');
                                pos = self.pending.len();
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.pending.drain(..pos);
        out
    }
}
struct Chunk {
    text: JsonString,
    bytes: usize,
    newlines: usize,
}
pub struct OutputBuffer {
    limits: OutputLimits,
    decoder: Utf8Decoder,
    started: bool,
    chunks: VecDeque<Chunk>,
    stored_bytes: usize,
    stored_newlines: usize,
    full: bool,
    total_bytes: usize,
    total_newlines: usize,
    ends_with_newline: bool,
}
impl OutputBuffer {
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            decoder: Utf8Decoder::default(),
            started: false,
            chunks: VecDeque::new(),
            stored_bytes: 0,
            stored_newlines: 0,
            full: false,
            total_bytes: 0,
            total_newlines: 0,
            ends_with_newline: true,
        }
    }
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }
    pub fn push_text(&mut self, text: &JsonString, skipped: Option<OutputSkip>) -> Result<bool> {
        let pending = self.decoder.decode(&[], true);
        self.push_decoded(pending.into(), text.clone(), skipped, false)
    }
    pub fn push_bytes(&mut self, bytes: &[u8], skipped: Option<OutputSkip>) -> Result<bool> {
        let pending = if skipped.is_some() {
            self.decoder.decode(&[], true)
        } else {
            String::new()
        };
        let text = self.decoder.decode(bytes, false);
        self.push_decoded(pending.into(), text.into(), skipped, true)
    }
    fn push_decoded(
        &mut self,
        mut pending: JsonString,
        mut text: JsonString,
        skipped: Option<OutputSkip>,
        byte_chunk: bool,
    ) -> Result<bool> {
        let first = !self.started && pending.0.is_empty() && skipped.is_none();
        if !pending.0.is_empty() || !text.0.is_empty() || skipped.is_some() {
            self.started = true;
        }
        if first && byte_chunk && text.0.first() == Some(&0xfeff) {
            text.0.remove(0);
        }
        let Some(skipped) = skipped else {
            pending.0.extend(text.0);
            return Ok(self.accept(pending));
        };
        if self.limits.retain != Retain::Tail {
            return Err(Error::Invalid(
                "Skipped output requires tail retention".into(),
            ));
        }
        self.accept(pending);
        if skipped.bytes != 0 {
            self.total_bytes += skipped.bytes;
            self.total_newlines += skipped.newlines;
            self.ends_with_newline = skipped.ends_with_newline;
            self.chunks.clear();
            self.stored_bytes = 0;
            self.stored_newlines = 0;
        }
        self.accept(text);
        Ok(true)
    }
    pub fn end(&mut self) {
        let rest = self.decoder.decode(&[], true);
        self.accept(rest.into());
    }
    fn accept(&mut self, text: JsonString) -> bool {
        if text.0.is_empty() {
            return false;
        }
        let bytes = utf8_byte_length(&text);
        let newlines = text.0.iter().filter(|&&c| c == 10).count();
        self.total_bytes += bytes;
        self.total_newlines += newlines;
        self.ends_with_newline = text.0.last() == Some(&10);
        if self.full {
            return true;
        }
        self.chunks.push_back(Chunk {
            text,
            bytes,
            newlines,
        });
        self.stored_bytes += bytes;
        self.stored_newlines += newlines;
        if self.limits.retain == Retain::Head {
            self.full = self.stored_bytes > self.limits.max_bytes
                || self.stored_newlines >= self.limits.max_lines;
            return true;
        }
        while self.chunks.len() > 1 {
            let Some(first) = self.chunks.front() else {
                break;
            };
            let after_bytes = self.stored_bytes - first.bytes;
            let after_lines = self.stored_newlines - first.newlines;
            if after_bytes <= self.limits.max_bytes.saturating_add(1)
                && after_lines <= self.limits.max_lines.saturating_add(1)
            {
                break;
            }
            self.chunks.pop_front();
            self.stored_bytes = after_bytes;
            self.stored_newlines = after_lines;
        }
        true
    }
    pub fn snapshot(&mut self) -> BoundedOutput {
        let stored = JsonString(
            self.chunks
                .iter()
                .flat_map(|c| c.text.0.iter().copied())
                .collect(),
        );
        let kept = bound_output(&stored, self.limits);
        let stored_lines = self.stored_newlines
            + usize::from(!stored.0.is_empty() && stored.0.last() != Some(&10));
        let kept_lines = stored_lines - kept.dropped_lines;
        if self.limits.retain == Retain::Tail || self.chunks.len() > 1 {
            let text = if self.limits.retain == Retain::Tail {
                tail_margin(&stored, self.limits)
            } else {
                stored
            };
            let bytes = if self.limits.retain == Retain::Tail {
                utf8_byte_length(&text)
            } else {
                self.stored_bytes
            };
            let newlines = text.0.iter().filter(|&&c| c == 10).count();
            self.chunks.clear();
            if !text.0.is_empty() {
                self.chunks.push_back(Chunk {
                    text,
                    bytes,
                    newlines,
                });
            }
            self.stored_bytes = bytes;
            self.stored_newlines = newlines;
        }
        BoundedOutput {
            text: sanitize_output(&kept.text),
            dropped_bytes: self.total_bytes - kept.bytes,
            dropped_lines: self.total_newlines + usize::from(!self.ends_with_newline) - kept_lines,
        }
    }
}
fn tail_margin(text: &JsonString, limits: OutputLimits) -> JsonString {
    let decoded = String::from_utf16_lossy(&text.0);
    let bytes = decoded.as_bytes();
    let byte_start = if bytes.len() > limits.max_bytes {
        character_end(bytes, bytes.len() - limits.max_bytes - 1)
    } else {
        0
    };
    let mut line_start = 0;
    let mut newlines = 0;
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'\n' {
            newlines += 1;
            if newlines > limits.max_lines {
                line_start = i;
                break;
            }
        }
    }
    let start = byte_start.max(line_start);
    if start == 0 {
        text.clone()
    } else {
        String::from_utf8_lossy(&bytes[start..]).as_ref().into()
    }
}
