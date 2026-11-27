//! Embedded JSON Schemas shipped with `pi-coding-agent` and compiled validators.

use std::sync::LazyLock;

use pi_rs_schema::{ValidationError, Validator, compile};
use serde_json::{Value, json};

pub const SETTINGS_SCHEMA_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/schemas/settings.schema.json"
));

pub const KEYBINDINGS_SCHEMA_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/schemas/keybindings.schema.json"
));

pub const MODELS_SCHEMA_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/schemas/models.schema.json"
));

static KEYBINDINGS_VALIDATOR: LazyLock<Result<Validator, String>> = LazyLock::new(|| {
    let schema: Value =
        serde_json::from_str(KEYBINDINGS_SCHEMA_JSON).map_err(|error| error.to_string())?;
    compile(&schema).map_err(|error| error.to_string())
});

static KEYBINDING_VALUE_VALIDATOR: LazyLock<Result<Validator, String>> = LazyLock::new(|| {
    let schema: Value =
        serde_json::from_str(KEYBINDINGS_SCHEMA_JSON).map_err(|error| error.to_string())?;
    let defs = schema
        .get("$defs")
        .cloned()
        .ok_or_else(|| "Keybindings schema missing $defs".to_owned())?;
    let subschema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": defs,
        "$ref": "#/$defs/KeybindingValue",
    });
    compile(&subschema).map_err(|error| error.to_string())
});

static SETTINGS_VALIDATOR: LazyLock<Result<Validator, String>> = LazyLock::new(|| {
    let schema: Value =
        serde_json::from_str(SETTINGS_SCHEMA_JSON).map_err(|error| error.to_string())?;
    compile(&schema).map_err(|error| error.to_string())
});

static MODELS_VALIDATOR: LazyLock<Result<Validator, String>> = LazyLock::new(|| {
    let schema: Value =
        serde_json::from_str(MODELS_SCHEMA_JSON).map_err(|error| error.to_string())?;
    compile(&schema).map_err(|error| error.to_string())
});

pub fn validate_keybinding_value(value: &Value) -> Result<bool, String> {
    let validator = KEYBINDING_VALUE_VALIDATOR.as_ref().map_err(Clone::clone)?;
    Ok(validator.check(&pi_rs_schema::Value::from(value)))
}

pub fn validate_keybindings_json(value: &Value) -> Result<Vec<ValidationError>, String> {
    let validator = KEYBINDINGS_VALIDATOR.as_ref().map_err(Clone::clone)?;
    Ok(validator.errors(&pi_rs_schema::Value::from(value)))
}

pub fn validate_settings_json(value: &Value) -> Result<Vec<ValidationError>, String> {
    let validator = SETTINGS_VALIDATOR.as_ref().map_err(Clone::clone)?;
    Ok(validator.errors(&pi_rs_schema::Value::from(value)))
}

pub fn validate_models_json(value: &Value) -> Result<Vec<ValidationError>, String> {
    let validator = MODELS_VALIDATOR.as_ref().map_err(Clone::clone)?;
    Ok(validator.errors(&pi_rs_schema::Value::from(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_schemas_compile_and_validate() -> Result<(), String> {
        assert!(validate_keybinding_value(&json!("ctrl+shift+p"))?);
        assert!(validate_keybinding_value(&json!(["ctrl++", "+"]))?);
        assert!(!validate_keybinding_value(&json!("ctrl+ctrl+p"))?);
        assert!(!validate_keybinding_value(&json!(123))?);

        assert!(validate_keybindings_json(&json!({"app.interrupt": "ctrl+c"}))?.is_empty());
        assert!(!validate_keybindings_json(&json!({"app.interrupt": 42}))?.is_empty());

        assert!(validate_settings_json(&json!({"theme": "dark"})).is_ok());
        assert!(validate_models_json(&json!({"providers": {}}))?.is_empty());
        Ok(())
    }
}
