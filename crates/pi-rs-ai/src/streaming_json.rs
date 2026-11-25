use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::types::JsonObject;

const SIMPLE: &[(char, char)] = &[
    ('"', '"'),
    ('\\', '\\'),
    ('/', '/'),
    ('b', '\u{8}'),
    ('f', '\u{c}'),
    ('n', '\n'),
    ('r', '\r'),
    ('t', '\t'),
];

pub fn repair_json(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut written = 0;
    let mut in_string = false;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            continue;
        }
        if c == '\\' {
            if let Some(&(_, next)) = chars.peek() {
                if SIMPLE.iter().any(|(escaped, _)| *escaped == next) {
                    chars.next();
                    continue;
                }
                if next == 'u'
                    && text
                        .as_bytes()
                        .get(at + 2..at + 6)
                        .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
                {
                    for _ in 0..5 {
                        chars.next();
                    }
                    continue;
                }
            }
            out.push_str(&text[written..at]);
            out.push_str("\\\\");
            written = at + 1;
        } else if c < '\u{20}' {
            out.push_str(&text[written..at]);
            if let Some((escape, _)) = SIMPLE.iter().find(|(_, decoded)| *decoded == c) {
                out.push('\\');
                out.push(*escape);
            } else {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            written = at + 1;
        }
    }
    if written == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[written..]);
        Cow::Owned(out)
    }
}

pub fn parse_json_with_repair(text: &str) -> serde_json::Result<Value> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(error) => match repair_json(text) {
            Cow::Borrowed(_) => Err(error),
            Cow::Owned(repaired) => serde_json::from_str(&repaired),
        },
    }
}

pub fn parse_tool_call_arguments(text: &str) -> (JsonObject, Option<String>) {
    if text.trim().is_empty() {
        return (Map::new(), None);
    }
    let message = match parse_json_with_repair(text) {
        Ok(Value::Object(fields)) => return (fields, None),
        Ok(_) => "Tool arguments must be a JSON object".to_owned(),
        Err(e) => e.to_string(),
    };
    (
        Map::new(),
        Some(format!("{message}\n\nReceived arguments:\n{text}")),
    )
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JsonPath {
    Key(String),
    Index(usize),
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum JsonChange {
    Set { path: Vec<JsonPath>, value: Value },
    Append { path: Vec<JsonPath>, text: String },
}

#[derive(Clone, Debug)]
enum Slot {
    Root,
    Key(String),
    Index(usize),
    Drop,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Key,
    Colon,
    Value,
    Comma,
}
struct Frame {
    place: Slot,
    value: Value,
    key: Option<String>,
    phase: Phase,
    path: Option<Vec<JsonPath>>,
}
#[derive(Default)]
struct OpenString {
    key: bool,
    value: Vec<u16>,
    place: Option<Slot>,
    escape: Vec<char>,
    path: Option<Vec<JsonPath>>,
    reported_text: usize,
    started_piece: bool,
}

/// Each fragment is read once. Snapshots are owned and charged against a byte-based budget
/// because Rust's JSON value owns even closed containers, unlike the reference's shared objects.
#[derive(Default)]
pub struct StreamingJsonParser {
    frames: Vec<Frame>,
    root: Option<Value>,
    string: Option<OpenString>,
    scalar: Option<(Slot, String)>,
    snapshot: Option<Arc<Value>>,
    done: bool,
    dirty: bool,
    credit: usize,
    bytes: usize,
}
impl StreamingJsonParser {
    fn slot(&self) -> Slot {
        let Some(frame) = self.frames.last() else {
            return if self.root.is_none() {
                Slot::Root
            } else {
                Slot::Drop
            };
        };
        if let Value::Array(items) = &frame.value {
            return Slot::Index(items.len());
        }
        if matches!(frame.phase, Phase::Colon | Phase::Value)
            && let Some(key) = &frame.key
        {
            return Slot::Key(key.clone());
        }
        Slot::Drop
    }
    fn path(&self, slot: &Slot) -> Option<Vec<JsonPath>> {
        if matches!(slot, Slot::Root) {
            return Some(vec![]);
        }
        let parent = self.frames.last()?.path.as_ref()?;
        if parent.len() >= 32 {
            return None;
        }
        let last = match slot {
            Slot::Key(k) => JsonPath::Key(k.clone()),
            Slot::Index(i) => JsonPath::Index(*i),
            _ => return None,
        };
        let mut path = parent.clone();
        path.push(last);
        Some(path)
    }
    fn place(&mut self, slot: Slot, value: Value) {
        if matches!(slot, Slot::Root) {
            self.root = Some(value);
            self.dirty = true;
        } else if let Some(frame) = self.frames.last_mut()
            && place(&mut frame.value, slot, value)
        {
            self.dirty = true;
        }
    }
    fn ended_value(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.phase = Phase::Comma;
        } else if self.root.is_some() {
            self.done = true;
        }
    }
    fn start_string(&mut self) {
        let key = self
            .frames
            .last()
            .is_some_and(|f| f.value.is_object() && matches!(f.phase, Phase::Key | Phase::Comma));
        let slot = if key { Slot::Drop } else { self.slot() };
        let path = self.path(&slot);
        self.dirty |= !matches!(slot, Slot::Drop);
        self.string = Some(OpenString {
            key,
            place: Some(slot),
            path,
            started_piece: true,
            ..Default::default()
        });
    }
    fn emit_string(string: &mut OpenString, complete: bool, changes: &mut Vec<JsonChange>) {
        let stable_end = if !complete
            && string
                .value
                .last()
                .is_some_and(|u| (0xd800..=0xdbff).contains(u))
        {
            string.value.len() - 1
        } else {
            string.value.len()
        };
        if let Some(path) = &string.path {
            if string.started_piece {
                changes.push(JsonChange::Set {
                    path: path.clone(),
                    value: Value::String(String::from_utf16_lossy(&string.value[..stable_end])),
                });
            } else if stable_end > string.reported_text {
                changes.push(JsonChange::Append {
                    path: path.clone(),
                    text: String::from_utf16_lossy(&string.value[string.reported_text..stable_end]),
                });
            }
        }
        string.started_piece = false;
        string.reported_text = stable_end;
    }
    fn finish_string(&mut self, mut string: OpenString, changes: &mut Vec<JsonChange>) {
        Self::emit_string(&mut string, true, changes);
        let value = String::from_utf16_lossy(&string.value);
        if string.key {
            if let Some(frame) = self.frames.last_mut() {
                frame.key = Some(value);
                frame.phase = Phase::Colon;
            }
        } else {
            self.place(string.place.unwrap_or(Slot::Drop), Value::String(value));
            self.ended_value();
        }
    }
    fn push_string_char(&mut self, mut string: OpenString, c: char, changes: &mut Vec<JsonChange>) {
        self.dirty |= !matches!(string.place, Some(Slot::Drop));
        if !string.escape.is_empty() {
            let len = string.escape.len();
            if len == 1 {
                if let Some((_, decoded)) = SIMPLE.iter().find(|(escape, _)| *escape == c) {
                    string
                        .value
                        .extend_from_slice(decoded.encode_utf16(&mut [0; 2]));
                    string.escape.clear();
                    self.string = Some(string);
                    return;
                }
                if c == 'u' {
                    string.escape.push(c);
                    self.string = Some(string);
                    return;
                }
            } else if c.is_ascii_hexdigit() {
                string.escape.push(c);
                if len == 5 {
                    let mut unit = 0u16;
                    for c in &string.escape[2..] {
                        unit = (unit << 4) | c.to_digit(16).unwrap_or_default() as u16;
                    }
                    string.value.push(unit);
                    string.escape.clear();
                }
                self.string = Some(string);
                return;
            }
            for literal in string.escape.drain(..) {
                string
                    .value
                    .extend_from_slice(literal.encode_utf16(&mut [0; 2]));
            }
        }
        match c {
            '"' => {
                self.finish_string(string, changes);
            }
            '\\' => {
                string.escape.push(c);
                self.string = Some(string);
            }
            _ => {
                string.value.extend_from_slice(c.encode_utf16(&mut [0; 2]));
                self.string = Some(string);
            }
        }
    }
    fn emit_scalar(&mut self, slot: Slot, text: String, changes: &mut Vec<JsonChange>) {
        if let Some(value) = scalar(&text, false) {
            if let Some(path) = self.path(&slot) {
                changes.push(JsonChange::Set {
                    path,
                    value: value.clone(),
                });
            }
            self.place(slot, value);
        }
        self.ended_value();
    }
    pub fn append(&mut self, text: &str) -> Vec<JsonChange> {
        self.bytes = self.bytes.saturating_add(text.len());
        self.credit = self
            .credit
            .saturating_add(256usize.saturating_add(text.len().saturating_mul(8)));
        if let Some(string) = &mut self.string {
            string.started_piece = false;
        }
        let mut changes = Vec::new();
        for c in text.chars() {
            if self.done {
                break;
            }
            if let Some(string) = self.string.take() {
                self.push_string_char(string, c, &mut changes);
                continue;
            }
            if let Some((slot, mut value)) = self.scalar.take() {
                if !delimiter(c) {
                    value.push(c);
                    self.dirty |= !matches!(slot, Slot::Drop);
                    self.scalar = Some((slot, value));
                    continue;
                }
                self.emit_scalar(slot, value, &mut changes);
                if self.done {
                    break;
                }
            }
            match c {
                ' ' | '\n' | '\r' | '\t' => {}
                '"' => self.start_string(),
                '{' | '[' => {
                    let slot = self.slot();
                    let path = self.path(&slot);
                    let (value, phase) = if c == '{' {
                        (Value::Object(Map::new()), Phase::Key)
                    } else {
                        (Value::Array(vec![]), Phase::Value)
                    };
                    if let Some(path) = &path {
                        changes.push(JsonChange::Set {
                            path: path.clone(),
                            value: value.clone(),
                        });
                    }
                    self.frames.push(Frame {
                        place: slot,
                        value,
                        key: None,
                        phase,
                        path,
                    });
                    self.dirty = true;
                }
                '}' | ']' => {
                    if let Some(frame) = self.frames.pop() {
                        if let Some(path) = frame.path.filter(|path| path.len() == 32) {
                            changes.push(JsonChange::Set {
                                path,
                                value: frame.value.clone(),
                            });
                        }
                        self.place(frame.place, frame.value);
                        self.ended_value();
                    }
                }
                ':' => {
                    if let Some(frame) = self.frames.last_mut()
                        && frame.phase == Phase::Colon
                    {
                        frame.phase = Phase::Value;
                    }
                }
                ',' => {
                    if let Some(frame) = self.frames.last_mut() {
                        frame.phase = if frame.value.is_object() {
                            Phase::Key
                        } else {
                            Phase::Value
                        };
                    }
                }
                _ => {
                    let slot = self.slot();
                    self.dirty |= !matches!(slot, Slot::Drop);
                    self.scalar = Some((slot, c.to_string()));
                }
            }
        }
        if let Some(string) = &mut self.string {
            Self::emit_string(string, false, &mut changes);
        }
        changes
    }
    pub fn snapshot(&mut self) -> Option<Arc<Value>> {
        if !self.dirty || self.bytes > self.credit {
            return self.snapshot.clone();
        }
        self.credit -= self.bytes;
        self.dirty = false;
        let mut child: Option<(Slot, Value)> =
            if let Some(s) = self.string.as_ref().filter(|s| !s.key) {
                let stable = if s
                    .value
                    .last()
                    .is_some_and(|u| (0xd800..=0xdbff).contains(u))
                {
                    &s.value[..s.value.len() - 1]
                } else {
                    &s.value
                };
                s.place
                    .clone()
                    .map(|slot| (slot, Value::String(String::from_utf16_lossy(stable))))
            } else {
                self.scalar
                    .as_ref()
                    .filter(|(_, s)| s.len() <= 64)
                    .and_then(|(slot, s)| scalar(s, true).map(|v| (slot.clone(), v)))
            };
        for frame in self.frames.iter().rev() {
            let mut value = frame.value.clone();
            if let Some((slot, child)) = child.take() {
                place(&mut value, slot, child);
            }
            child = Some((frame.place.clone(), value));
        }
        self.snapshot = match child {
            Some((Slot::Root, value)) => Some(Arc::new(value)),
            _ => self.root.clone().map(Arc::new),
        };
        self.snapshot.clone()
    }
}
fn place(container: &mut Value, slot: Slot, value: Value) -> bool {
    match (container, slot) {
        (Value::Object(fields), Slot::Key(key)) => {
            fields.insert(key, value);
            true
        }
        (Value::Array(items), Slot::Index(index)) if index == items.len() => {
            items.push(value);
            true
        }
        _ => false,
    }
}
fn delimiter(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | ',' | ':' | '{' | '}' | '[' | ']' | '"'
    )
}
fn scalar(text: &str, pending: bool) -> Option<Value> {
    for (literal, value) in [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
    ] {
        if text == literal || pending && literal.starts_with(text) {
            return Some(value);
        }
    }
    if let Ok(Value::Number(n)) = serde_json::from_str::<Value>(text) {
        return Some(Value::Number(n));
    }
    if pending
        && let Some((mantissa, exp)) = text.split_once(['e', 'E'])
        && matches!(exp, "" | "+" | "-")
    {
        return serde_json::from_str::<Value>(mantissa)
            .ok()
            .filter(Value::is_number);
    }
    None
}
pub fn parse_streaming_json(text: &str) -> Value {
    if let Ok(v) = parse_json_with_repair(text)
        && !v.is_null()
    {
        return v;
    }
    let mut p = StreamingJsonParser::default();
    p.append(text);
    p.snapshot()
        .filter(|v| !v.is_null())
        .map(Arc::unwrap_or_clone)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

#[derive(Default)]
pub struct ToolCallArgumentsStream {
    text: String,
    parser: StreamingJsonParser,
}
impl ToolCallArgumentsStream {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn append(&mut self, delta: &str) -> Option<Arc<Value>> {
        self.text.push_str(delta);
        self.parser.append(delta);
        self.parser.snapshot().filter(|v| v.is_object())
    }
    pub fn replace(&mut self, text: &str) -> (Option<String>, Option<Arc<Value>>) {
        if let Some(delta) = text.strip_prefix(&self.text) {
            return (Some(delta.to_owned()), self.append(delta));
        }
        *self = Self::default();
        (None, self.append(text))
    }
    pub fn finish(&self) -> (JsonObject, Option<String>) {
        parse_tool_call_arguments(&self.text)
    }
}
