//! Frame terminal keyboard, mouse, and paste input across read boundaries.
//! Based on OpenTUI, MIT License, copyright (c) 2025 opentui.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

const ESC: &str = "\x1b";
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

static REPLY_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: A fixed ASCII terminal protocol header.
    Regex::new(
        r"^\x1b(?:\](?:1[01];|4;[0-9]{1,3};|7501;\?)|P>\||_G(?:[a-z]=[0-9]+,)*[a-z]=[0-9]+;)",
    )
    .expect("valid reply grammar")
});

static KITTY_PRINTABLE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: A fixed ASCII CSI-u protocol grammar.
    Regex::new(r"^\x1b\[([0-9]+)(?::[0-9]*)?(?::[0-9]+)?u$").expect("valid printable grammar")
});

#[derive(Debug, PartialEq, Eq)]
pub enum StdinEvent {
    Data(String),
    Paste(String),
}

#[derive(Debug, Clone, Copy)]
pub struct StdinBufferOptions {
    pub timeout: Duration,
    pub escape_timeout: Duration,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(50),
            escape_timeout: Duration::from_millis(10),
        }
    }
}

#[derive(Debug, Default)]
pub struct StdinBuffer {
    pending: String,
    in_paste: bool,
    paste_searched: usize,
    kitty_duplicate: Option<char>,
    deadline: Option<Instant>,
    options: StdinBufferOptions,
}

impl StdinBuffer {
    pub fn new(options: StdinBufferOptions) -> Self {
        Self {
            options,
            ..Self::default()
        }
    }

    pub fn process(&mut self, text: &str, now: Instant) -> Vec<StdinEvent> {
        let mut events = Vec::new();
        self.deadline = None;
        if (self.in_paste || self.pending.is_empty()) && text.is_empty() {
            emit_data(&mut self.kitty_duplicate, "", &mut events);
            return events;
        }
        self.pending.push_str(text);
        self.extract(now, &mut events);
        events
    }

    pub fn process_bytes(&mut self, data: &[u8], now: Instant) -> Vec<StdinEvent> {
        if let [high @ 128..=255] = data {
            let bytes = [0x1b, high - 128];
            return self.process(&String::from_utf8_lossy(&bytes), now);
        }
        self.process(&String::from_utf8_lossy(data), now)
    }

    fn extract(&mut self, now: Instant, events: &mut Vec<StdinEvent>) {
        loop {
            if self.in_paste {
                let Some(end) = self.pending[self.paste_searched..].find(PASTE_END) else {
                    self.paste_searched = self.pending.len().saturating_sub(PASTE_END.len() - 1);
                    while !self.pending.is_char_boundary(self.paste_searched) {
                        self.paste_searched -= 1;
                    }
                    return;
                };
                let end = self.paste_searched + end;
                let remainder = self.pending.split_off(end + PASTE_END.len());
                self.pending.truncate(end);
                events.push(StdinEvent::Paste(std::mem::replace(
                    &mut self.pending,
                    remainder,
                )));
                self.in_paste = false;
                self.paste_searched = 0;
                self.kitty_duplicate = None;
                if self.pending.is_empty() {
                    return;
                }
            } else if let Some(start) = self.pending.find(PASTE_START) {
                extract_sequences(&self.pending[..start], &mut self.kitty_duplicate, events);
                self.pending.drain(..start + PASTE_START.len());
                self.in_paste = true;
                self.kitty_duplicate = None;
            } else {
                let consumed = extract_sequences(&self.pending, &mut self.kitty_duplicate, events);
                self.pending.drain(..consumed);
                if !self.pending.is_empty() && !REPLY_HEADER.is_match(&self.pending) {
                    let timeout = if self.pending == ESC {
                        self.options.escape_timeout
                    } else {
                        self.options.timeout
                    };
                    self.deadline = now.checked_add(timeout);
                }
                return;
            }
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Release a timed-out ambiguous legacy escape sequence.
    pub fn expire(&mut self, now: Instant) -> Vec<StdinEvent> {
        let mut events = Vec::new();
        if self.deadline.is_some_and(|deadline| now >= deadline)
            && let Some(sequence) = self.take_pending()
        {
            if sequence.len() > 2 && is_control_string(&sequence) {
                emit_data(&mut self.kitty_duplicate, &sequence[..2], &mut events);
                self.pending.push_str(&sequence[2..]);
                self.extract(now, &mut events);
            } else {
                emit_data(&mut self.kitty_duplicate, &sequence, &mut events);
            }
        }
        events
    }

    fn take_pending(&mut self) -> Option<String> {
        self.deadline = None;
        if self.pending.is_empty() || self.in_paste {
            return None;
        }
        self.kitty_duplicate = None;
        Some(std::mem::take(&mut self.pending))
    }

    pub fn flush(&mut self) -> Vec<String> {
        self.take_pending().into_iter().collect()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.in_paste = false;
        self.paste_searched = 0;
        self.kitty_duplicate = None;
        self.deadline = None;
    }

    pub fn buffer(&self) -> &str {
        if self.in_paste { "" } else { &self.pending }
    }
}

fn emit_data(pending_kitty: &mut Option<char>, text: &str, events: &mut Vec<StdinEvent>) {
    if pending_kitty
        .is_some_and(|c| c.len_utf16() == 1 && text.len() == c.len_utf8() && text.starts_with(c))
    {
        *pending_kitty = None;
        return;
    }
    *pending_kitty = if text.starts_with(ESC) {
        KITTY_PRINTABLE
            .captures(text)
            .and_then(|captures| captures[1].parse().ok())
            .filter(|&codepoint| codepoint >= 32)
            .and_then(char::from_u32)
    } else {
        None
    };
    events.push(StdinEvent::Data(text.to_owned()));
}

fn is_control_string(text: &str) -> bool {
    matches!(text.as_bytes(), [27, b']' | b'P' | b'_', ..])
}

fn extract_sequences(
    text: &str,
    duplicate: &mut Option<char>,
    events: &mut Vec<StdinEvent>,
) -> usize {
    let mut offset = 0;
    while let Some(c) = text[offset..].chars().next() {
        let rest = &text[offset..];
        let (end, emit) = if c == '\x1b' {
            let Some(sequence) = escape_end(rest) else {
                break;
            };
            sequence
        } else {
            (c.len_utf8(), true)
        };
        if emit {
            emit_data(duplicate, &rest[..end], events);
        }
        offset += end;
    }
    offset
}

/// Return the next sequence boundary, and whether it's a completed input event.
fn escape_end(text: &str) -> Option<(usize, bool)> {
    let bytes = text.as_bytes();
    match *bytes.get(1)? {
        b'[' if bytes.get(2) == Some(&b'M') => text[3..]
            .char_indices()
            .nth(2)
            .map(|(i, c)| (3 + i + c.len_utf8(), true)),
        b'[' if bytes.get(2) == Some(&b'<') => {
            let mut numbers = 0;
            let mut digit = false;
            for (i, &byte) in bytes[3..].iter().enumerate() {
                match byte {
                    b'0'..=b'9' => digit = true,
                    b';' if digit && numbers < 2 => {
                        numbers += 1;
                        digit = false;
                    }
                    b'm' | b'M' if digit && numbers == 2 => return Some((i + 4, true)),
                    _ => return None,
                }
            }
            None
        }
        b'[' => bytes[2..]
            .iter()
            .position(|b| (b'@'..=b'~').contains(b))
            .map(|i| (i + 3, true)),
        b'O' => text[2..].chars().next().map(|c| (2 + c.len_utf8(), true)),
        kind @ (b']' | b'P' | b'_') => {
            let reply = REPLY_HEADER.is_match(text);
            for (i, &byte) in bytes.iter().enumerate().skip(2) {
                match byte {
                    b'\x07' if kind == b']' => return Some((i + 1, true)),
                    b'\x18' | b'\x1a' if reply => return Some((i + 1, false)),
                    27 if bytes.get(i + 1) == Some(&b'\\') => return Some((i + 2, true)),
                    27 if reply && i + 1 < bytes.len() => return Some((i, false)),
                    _ => {}
                }
            }
            None
        }
        27 if matches!(bytes.get(2), Some(b'[' | b']' | b'O' | b'P' | b'_')) => Some((1, true)),
        _ => text[1..].chars().next().map(|c| (1 + c.len_utf8(), true)),
    }
}
