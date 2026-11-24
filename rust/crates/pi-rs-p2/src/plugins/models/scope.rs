//! Model scope resolution (`enabledModels` glob/exact patterns with optional `:thinkingLevel`).

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::plugins::models::resources::ModelInfo;

const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedModel {
    pub provider: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeResult {
    pub models: Vec<ScopedModel>,
    pub unmatched: Vec<String>,
}

fn glob_regex(pattern: &str) -> Option<Regex> {
    let mut source = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => source.push_str(".*"),
            '?' => source.push('.'),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                source.push('\\');
                source.push(ch);
            }
            other => source.push(other),
        }
    }
    source.push('$');
    regex::RegexBuilder::new(&source)
        .case_insensitive(true)
        .build()
        .ok()
}

fn exact<'a>(reference: &str, models: &'a [ModelInfo]) -> Option<&'a ModelInfo> {
    let lower = reference.to_lowercase();
    models
        .iter()
        .find(|m| format!("{}/{}", m.provider, m.model_id).to_lowercase() == lower)
        .or_else(|| models.iter().find(|m| m.model_id.to_lowercase() == lower))
}

pub fn resolve_model_scope(patterns: &[String], models: &[ModelInfo]) -> ScopeResult {
    let mut result = ScopeResult::default();
    let mut add = |model: &ModelInfo, thinking_level: Option<&str>| {
        if result
            .models
            .iter()
            .any(|item| item.provider == model.provider && item.model_id == model.model_id)
        {
            return;
        }
        result.models.push(ScopedModel {
            provider: model.provider.clone(),
            model_id: model.model_id.clone(),
            thinking_level: thinking_level.filter(|s| !s.is_empty()).map(str::to_owned),
        });
    };

    for pattern in patterns {
        let mut reference = pattern.trim();
        let mut level: Option<&str> = None;
        if exact(reference, models).is_none()
            && let Some(colon) = reference.rfind(':')
        {
            let suffix = &reference[colon + 1..];
            if LEVELS.contains(&suffix) {
                level = Some(suffix);
                reference = &reference[..colon];
            }
        }
        if let Some(single) = exact(reference, models) {
            add(single, level);
            continue;
        }
        if !reference.contains('*') && !reference.contains('?') {
            result.unmatched.push(pattern.clone());
            continue;
        }
        let Some(matcher) = glob_regex(reference) else {
            result.unmatched.push(pattern.clone());
            continue;
        };
        let mut matched = false;
        for model in models {
            let full = format!("{}/{}", model.provider, model.model_id);
            if matcher.is_match(&full) || matcher.is_match(&model.model_id) {
                add(model, level);
                matched = true;
            }
        }
        if !matched {
            result.unmatched.push(pattern.clone());
        }
    }
    result
}
