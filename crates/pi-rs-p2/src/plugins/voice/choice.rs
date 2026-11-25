//! Resolves a conversation's voice settings against the speech service's offered engines.

use serde::{Deserialize, Serialize};

use super::protocol::{LanguageInfo, TtsEngine, Voices};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub engine: String,
    pub voice: String,
    pub speed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedVoice<'a> {
    pub engine: Option<&'a TtsEngine>,
    pub voice: String,
    pub language: Option<String>,
}

/// Resolves engine, voice, and language against `config`, falling back to service/engine defaults.
pub fn resolve_voice<'a>(
    config: Option<&'a Voices>,
    settings: &VoiceSettings,
) -> ResolvedVoice<'a> {
    let Some(config) = config else {
        return ResolvedVoice {
            engine: None,
            voice: settings.voice.clone(),
            language: settings.language.clone(),
        };
    };
    let engine = config
        .engines
        .iter()
        .find(|c| c.id == settings.engine)
        .or_else(|| {
            config
                .engines
                .iter()
                .find(|c| c.id == config.default.engine)
        })
        .or_else(|| config.engines.first());

    let has_voice = |id: &str| engine.is_some_and(|e| e.voices.iter().any(|v| v.id == id));
    let voice = if has_voice(&settings.voice) {
        settings.voice.clone()
    } else if engine.is_some_and(|e| e.id == config.default.engine)
        && has_voice(&config.default.voice)
    {
        config.default.voice.clone()
    } else {
        engine
            .and_then(|e| e.voices.first())
            .map_or_else(String::new, |v| v.id.clone())
    };

    let speaks = |id: Option<&str>| -> bool {
        id.is_some_and(|lang| {
            engine
                .and_then(|e| e.languages.as_ref())
                .is_some_and(|langs| langs.iter().any(|l| l.id == lang))
        })
    };
    let language = if speaks(settings.language.as_deref()) {
        settings.language.clone()
    } else if engine.is_some_and(|e| e.id == config.default.engine)
        && speaks(config.default.language.as_deref())
    {
        config.default.language.clone()
    } else {
        engine.and_then(|e| e.default_language.clone())
    };

    ResolvedVoice {
        engine,
        voice,
        language,
    }
}

/// Orders an engine's languages with English (`en`) and Deutsch (`de`) first, then service order.
pub fn languages_of(engine: Option<&TtsEngine>) -> Vec<LanguageInfo> {
    let Some(all) = engine.and_then(|e| e.languages.as_ref()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(all.len());
    for priority in ["en", "de"] {
        for item in all {
            if item.id == priority {
                out.push(item.clone());
            }
        }
    }
    for item in all {
        if item.id != "en" && item.id != "de" {
            out.push(item.clone());
        }
    }
    out
}

pub fn resolved(config: Option<&Voices>, settings: &VoiceSettings) -> VoiceSettings {
    let r = resolve_voice(config, settings);
    VoiceSettings {
        engine: r
            .engine
            .map_or_else(|| settings.engine.clone(), |e| e.id.clone()),
        voice: r.voice,
        speed: settings.speed,
        language: r.language,
    }
}

pub fn engine_label(engine: &TtsEngine) -> String {
    let mut chars = engine.id.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}
