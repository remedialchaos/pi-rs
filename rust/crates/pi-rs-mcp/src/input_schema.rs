//! An MCP tool's input schema in the shape LLM tool APIs accept
//! (`packages/mcp/src/input-schema.ts`).

use serde_json::{Map, Value, json};

/// The tool's input schema as an object schema. Tool parameters must be objects, MCP servers may
/// omit `type`, and some providers reject object schemas without `properties`. Other keywords are
/// kept, in the order JavaScript's spread produces.
pub fn normalize_mcp_input_schema(schema: &Value) -> Result<Value, McpInputSchemaError> {
    let entries = spread_entries(schema)?;
    let mut out = Map::new();
    let mut has_type = false;
    let mut has_properties = false;
    for (key, value) in entries {
        has_type |= key == "type";
        // `schema.properties === undefined` adds the default; JSON `null` is a value and stays.
        has_properties |= key == "properties";
        // `schema.type ?? "object"`: `null` becomes `"object"`.
        let value = if key == "type" && value.is_null() {
            json!("object")
        } else {
            value
        };
        out.insert(key, value);
    }
    if !has_type {
        out.insert("type".into(), json!("object"));
    }
    if !has_properties {
        out.insert("properties".into(), json!({}));
    }
    Ok(Value::Object(out))
}

/// Why an input schema cannot be normalized: `null` (and `undefined`) spread to nothing but fail
/// when read, exactly like `{...null}` succeeds and `null.type` throws upstream.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpInputSchemaError {
    #[error("Cannot read properties of null (reading 'type')")]
    NullSchema,
    #[error("Cannot read properties of undefined (reading 'type')")]
    UndefinedSchema,
}

/// The own enumerable entries of a spread value: an object's keys in order, an array's or string's
/// indexes, and nothing for numbers and booleans.
fn spread_entries(schema: &Value) -> Result<Vec<(String, Value)>, McpInputSchemaError> {
    match schema {
        Value::Null => Err(McpInputSchemaError::NullSchema),
        Value::Object(object) => Ok(object
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()),
        Value::Array(items) => Ok(items
            .iter()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item.clone()))
            .collect()),
        // A string spreads its characters, keyed by position; a number or boolean has no entries.
        Value::String(text) => Ok(text
            .chars()
            .enumerate()
            .map(|(index, character)| (index.to_string(), Value::String(character.to_string())))
            .collect()),
        Value::Bool(_) | Value::Number(_) => Ok(Vec::new()),
    }
}
