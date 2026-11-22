//! The output schema of an MCP tool for programs that call it: the whole `CallToolResult`, with
//! the tool's own output schema as `structuredContent` (`packages/mcp/src/result-schema.ts`).

use serde_json::{Map, Value, json};

/// `schema` as a subschema at the JSON pointer of `structuredContent`.
const STRUCTURED_CONTENT_POINTER: &str = "#/properties/structuredContent";

/// Keywords whose values are data, so a `$ref` key in them is not a reference.
const SCHEMA_DATA_KEYWORDS: [&str; 4] = ["const", "enum", "default", "examples"];

/// Keywords whose values map names to schemas, so their keys are names, not keywords.
const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "$defs",
    "definitions",
];

/// Output schema of an MCP tool: the `CallToolResult` programs receive (without `_meta`, which
/// callers drop), with the tool's own output schema as `structuredContent`. Without one, structured
/// content may be any JSON value.
pub fn create_mcp_result_schema(structured_content_schema: Option<&Value>) -> Value {
    let structured_content = match structured_content_schema {
        Some(schema) if !is_js_falsy(schema) => relocate_schema(schema, STRUCTURED_CONTENT_POINTER),
        _ => json!({}),
    };
    json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": content_block_schema() },
            "structuredContent": structured_content,
            "isError": { "type": "boolean" },
        },
        "required": ["content"],
        // Fields newer protocol versions add.
        "additionalProperties": true,
    })
}

/// MCP content blocks as one compact type with the fields of text and image blocks, which scripts
/// read most. Rendered into every MCP tool's declaration.
fn content_block_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "enum": ["text", "image", "audio", "resource_link", "resource"] },
            "text": { "type": "string" },
            "data": { "type": "string" },
            "mimeType": { "type": "string" },
        },
        "required": ["type"],
        "additionalProperties": true,
    })
}

/// Whether a JSON value is falsy in JavaScript: `null`, `false`, a zero number or `""`.
fn is_js_falsy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::Number(number) => number.as_f64().is_some_and(|number| number == 0.0),
        Value::String(text) => text.is_empty(),
        _ => false,
    }
}

/// `schema` as a subschema at the JSON pointer `pointer` of another schema: its local references
/// (`#`, `#/$defs/Item`) are rewritten to point below `pointer`, so they resolve to the same
/// subschemas as before.
fn relocate_schema(schema: &Value, pointer: &str) -> Value {
    match schema {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| relocate_schema(item, pointer))
                .collect(),
        ),
        Value::Object(map) => {
            let mut relocated = Map::new();
            for (key, value) in map {
                // A local reference: `#` or `#/...`. `rest` is what follows the `#`.
                if key == "$ref"
                    && let Some(reference) = value.as_str()
                    && let Some(rest) = reference.strip_prefix('#')
                    && (rest.is_empty() || rest.starts_with('/'))
                {
                    relocated.insert(key.clone(), Value::String(format!("{pointer}{rest}")));
                } else if SCHEMA_DATA_KEYWORDS.contains(&key.as_str()) {
                    relocated.insert(key.clone(), value.clone());
                } else if SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) && value.as_object().is_some()
                {
                    let mut moved = Map::new();
                    for (name, subschema) in value.as_object().into_iter().flatten() {
                        moved.insert(name.clone(), relocate_schema(subschema, pointer));
                    }
                    relocated.insert(key.clone(), Value::Object(moved));
                } else {
                    relocated.insert(key.clone(), relocate_schema(value, pointer));
                }
            }
            Value::Object(relocated)
        }
        other => other.clone(),
    }
}
