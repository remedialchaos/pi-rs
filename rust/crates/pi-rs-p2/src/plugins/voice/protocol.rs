//! Voice mode wire shapes, validation, room naming, and binary frame parsing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_SPEAK_TEXT: usize = 2000;
pub const MIN_PAUSE: f64 = 0.5;
pub const MAX_PAUSE: f64 = 2.0;
pub const DEFAULT_PAUSE: f64 = 0.8;
pub const AGENT_AUDIO: u8 = 1;
pub const HUMAN_AUDIO: u8 = 2;
pub const HUMAN_RATE: u32 = 16_000;
pub const SAMPLE_TEXT: &str = "Hi. This is how I sound when I read my answers to you.";
pub const MAX_LOG_EVENTS: usize = 100;
pub const MAX_LOG_MESSAGE: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsEngine {
    pub id: String,
    pub name: String,
    pub streaming: bool,
    pub sample_rate: u32,
    pub voices: Vec<VoiceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<LanguageInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultVoice {
    pub engine: String,
    pub voice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Voices {
    pub engines: Vec<TtsEngine>,
    pub default: DefaultVoice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomFrame {
    pub kind: u8,
    pub id: String,
    pub pcm: Vec<u8>,
}

/// Validates `^[A-Za-z0-9_-]{1,64}$`.
pub fn is_valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Validates `^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8}){0,3}$`.
pub fn is_valid_language(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let mut count = 0usize;
    for sub in parts {
        count += 1;
        if count > 3
            || !(2..=8).contains(&sub.len())
            || !sub.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return false;
        }
    }
    true
}

/// Parses a binary room frame: `[kind, id_len, id_bytes..., pad_if_odd, pcm16le...]`.
pub fn parse_room_frame(frame: &[u8]) -> Option<RoomFrame> {
    let kind = *frame.first()?;
    let length = usize::from(*frame.get(1)?);
    let header = 2 + length;
    let offset = header + (header & 1);
    if frame.len() < offset {
        return None;
    }
    let id: String = frame[2..header].iter().map(|&b| char::from(b)).collect();
    Some(RoomFrame {
        kind,
        id,
        pcm: frame[offset..].to_vec(),
    })
}

/// Returns the service's room name for `(session, conversation)` if it matches `^p2-[A-Za-z0-9_-]{3,160}$`.
pub fn room_name(session: &str, conversation: u64) -> Option<String> {
    let name = format!("p2-{session}-{conversation}");
    let suffix = name.strip_prefix("p2-")?;
    if (3..=160).contains(&suffix.len())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(name)
    } else {
        None
    }
}

pub fn validate_voices(value: &Value) -> Option<Voices> {
    let parsed: Voices = serde_json::from_value(value.clone()).ok()?;
    if !is_valid_id(&parsed.default.engine) || !is_valid_id(&parsed.default.voice) {
        return None;
    }
    if let Some(lang) = &parsed.default.language
        && !is_valid_language(lang)
    {
        return None;
    }
    for engine in &parsed.engines {
        if !is_valid_id(&engine.id) {
            return None;
        }
        if let Some(lang) = &engine.default_language
            && !is_valid_language(lang)
        {
            return None;
        }
        for voice in &engine.voices {
            if !is_valid_id(&voice.id) {
                return None;
            }
        }
        if let Some(langs) = &engine.languages {
            for lang in langs {
                if !is_valid_language(&lang.id) {
                    return None;
                }
            }
        }
    }
    Some(parsed)
}

pub fn is_valid_room_personal(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("reset") => true,
        Some("vad") => obj
            .get("silence")
            .and_then(Value::as_f64)
            .is_some_and(|s| s.is_finite() && (MIN_PAUSE..=MAX_PAUSE).contains(&s)),
        _ => false,
    }
}

pub fn is_valid_room_command(value: &Value) -> bool {
    if is_valid_room_personal(value) {
        return true;
    }
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("config") => {
            let engine_ok = obj
                .get("engine")
                .is_none_or(|v| v.as_str().is_some_and(is_valid_id));
            let voice_ok = obj
                .get("voice")
                .is_none_or(|v| v.as_str().is_some_and(is_valid_id));
            let speed_ok = obj.get("speed").is_none_or(|v| {
                v.as_f64()
                    .is_some_and(|s| s.is_finite() && (0.5..=2.0).contains(&s))
            });
            let lang_ok = obj
                .get("language")
                .is_none_or(|v| v.as_str().is_some_and(is_valid_language));
            engine_ok && voice_ok && speed_ok && lang_ok
        }
        Some("speak") => {
            let id_ok = obj
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(is_valid_id);
            let text_ok = obj
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty() && t.chars().count() <= MAX_SPEAK_TEXT);
            id_ok && text_ok
        }
        Some("cancel") => obj
            .get("id")
            .is_none_or(|v| v.as_str().is_some_and(is_valid_id)),
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    pub speed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

pub fn validate_sample(value: &Value) -> Option<SampleRequest> {
    let parsed: SampleRequest = serde_json::from_value(value.clone()).ok()?;
    if !parsed.speed.is_finite() || !(0.5..=2.0).contains(&parsed.speed) {
        return None;
    }
    if let Some(engine) = &parsed.engine
        && !is_valid_id(engine)
    {
        return None;
    }
    if let Some(voice) = &parsed.voice
        && !is_valid_id(voice)
    {
        return None;
    }
    if let Some(lang) = &parsed.language
        && !is_valid_language(lang)
    {
        return None;
    }
    Some(parsed)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub t: f64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogBatch {
    pub client: String,
    pub events: Vec<LogEvent>,
}

pub fn validate_log_batch(value: &Value) -> Option<LogBatch> {
    let parsed: LogBatch = serde_json::from_value(value.clone()).ok()?;
    if parsed.client.is_empty()
        || parsed.client.len() > 16
        || !parsed
            .client
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    if parsed.events.len() > MAX_LOG_EVENTS {
        return None;
    }
    for ev in &parsed.events {
        if !ev.t.is_finite() || !(0.0..=8.64e15).contains(&ev.t) {
            return None;
        }
        if ev.message.chars().count() > MAX_LOG_MESSAGE {
            return None;
        }
    }
    Some(parsed)
}
