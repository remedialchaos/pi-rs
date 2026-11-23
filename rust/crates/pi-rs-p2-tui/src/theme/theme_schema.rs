use crate::theme::theme_tokens::THEME_TOKENS;
use pi_rs_schema::{Validator, compile};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    String(String),
    Number(f64),
    Array(Vec<f64>),
    Object(HashMap<String, serde_json::Value>),
}

pub type ThemeColorValue = ColorValue;

static COMPILED_THEME_SCHEMA: LazyLock<Result<Validator, pi_rs_schema::CompileError>> =
    LazyLock::new(|| {
        let color =
            json!({"anyOf":[{"type":"string"},{"type":"integer","minimum":0,"maximum":255}]});
        let properties: Map<String, Value> = THEME_TOKENS
            .keys()
            .map(|k| (k.clone(), color.clone()))
            .collect();
        let required: Vec<_> = THEME_TOKENS
            .iter()
            .filter(|(_, v)| v.fallback.is_none())
            .map(|(k, _)| k)
            .collect();
        compile(
            &json!({"type":"object","required":["name","colors"],"properties":{"$schema":{"type":"string"},"name":{"type":"string","pattern":"^[^/]+$"},"appearance":{"enum":["dark","light"]},"vars":{"type":"object","additionalProperties":color},"colors":{"type":"object","properties":properties,"required":required},"export":{"type":"object","properties":{"pageBg":color,"cardBg":color,"infoBg":color}}}}),
        )
    });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedThemeJson {
    pub name: String,
    #[serde(default)]
    pub appearance: String,
    #[serde(default)]
    pub vars: HashMap<String, ColorValue>,
    pub colors: HashMap<String, ColorValue>,
    #[serde(default)]
    pub export: HashMap<String, ColorValue>,
}

#[derive(Debug, Error)]
#[error("Theme validation failed for {label}: {message}")]
pub struct ThemeValidationError {
    pub label: String,
    pub message: String,
}

pub fn validate_theme_json(
    label: &str,
    json: &serde_json::Value,
) -> Result<ValidatedThemeJson, ThemeValidationError> {
    let fail = |message| ThemeValidationError {
        label: label.into(),
        message,
    };
    let schema = COMPILED_THEME_SCHEMA
        .as_ref()
        .map_err(|e| fail(e.to_string()))?;
    let value = pi_rs_schema::Value::from(json);
    if !schema.check(&value) {
        return Err(fail(
            schema
                .errors(&value)
                .iter()
                .map(|e| format!("{} {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    serde_json::from_value(json.clone()).map_err(|e| fail(e.to_string()))
}
