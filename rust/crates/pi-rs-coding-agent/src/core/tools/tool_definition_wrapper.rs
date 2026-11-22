//! Helpers shared by the built-in `AgentTool` wrappers.
//!
//! Upstream keeps a `ToolDefinition` (prompt metadata, renderer, strict-sampling flag) and wraps it
//! into an `AgentTool` for the core runtime. Rust has no structural typing here, so this module
//! builds the `pi_rs_ai::Tool` declaration and the `AgentToolResult` content blocks directly.
//! Names, labels, schemas, and content-block shapes match
//! `packages/coding-agent/src/core/tools/tool-definition-wrapper.ts`.

use pi_rs_agent::AgentToolResult;
use pi_rs_ai::{
    ConstrainedSampling, ConstrainedSamplingConfig, ImageContent, ImageContentKind, InputContent,
    StrictPreference, TextContent, Tool,
};
use serde_json::{Map, Value, json};

use super::truncate::{TruncatedBy, TruncationResult};

/// Upstream's `{ type: "json_schema", strict: "prefer" }` flag. Only `read`, `bash`, `powershell`,
/// `edit`, and `write` carry it; `grep`, `find`, and `ls` do not.
pub fn strict_sampling() -> Option<ConstrainedSampling> {
    Some(ConstrainedSampling::Enabled(
        ConstrainedSamplingConfig::JsonSchema {
            strict: StrictPreference::Prefer,
        },
    ))
}

/// Build a tool declaration. `parameters` must be the JSON Schema object from upstream.
pub fn build_tool(name: &str, description: &str, parameters: Value, strict: bool) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
        constrained_sampling: if strict { strict_sampling() } else { None },
    }
}

/// A single text content block.
pub fn text_result(text: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![InputContent::Text(TextContent::from(text.into()))],
        ..AgentToolResult::default()
    }
}

/// A single text content block with a details payload (serialized tool details).
pub fn text_result_with_details(
    text: impl Into<String>,
    details: Option<Value>,
) -> AgentToolResult {
    AgentToolResult {
        content: vec![InputContent::Text(TextContent::from(text.into()))],
        details,
        ..AgentToolResult::default()
    }
}

/// A result with explicit error status, used by the shell tools for non-zero exits.
pub fn error_result(
    text: impl Into<String>,
    details: Option<Value>,
    structured: Option<Value>,
) -> AgentToolResult {
    AgentToolResult {
        content: vec![InputContent::Text(TextContent::from(text.into()))],
        details,
        structured_content: structured,
        is_error: Some(true),
        ..AgentToolResult::default()
    }
}

/// The read tool's image result: a note block followed by the inline image block, plus the
/// programmatic `structuredContent` form (`{ type, data, mimeType, note }`).
pub fn image_result(note: String, data: String, mime_type: String) -> AgentToolResult {
    let structured = json!({
        "type": "image",
        "data": data.clone(),
        "mimeType": mime_type.clone(),
        "note": note.clone(),
    });
    AgentToolResult {
        content: vec![
            InputContent::Text(TextContent::from(note)),
            InputContent::Image(ImageContent {
                kind: ImageContentKind::Image,
                data,
                mime_type,
            }),
        ],
        structured_content: Some(structured),
        ..AgentToolResult::default()
    }
}

/// Serialize a truncation snapshot with the exact keys of upstream's `TruncationResult`.
pub fn truncation_value(value: &TruncationResult) -> Value {
    let truncated_by = match value.truncated_by {
        TruncatedBy::Lines => json!("lines"),
        TruncatedBy::Bytes => json!("bytes"),
        TruncatedBy::None => Value::Null,
    };
    json!({
        "content": value.content,
        "truncated": value.truncated,
        "truncatedBy": truncated_by,
        "totalLines": value.total_lines,
        "totalBytes": value.total_bytes,
        "outputLines": value.output_lines,
        "outputBytes": value.output_bytes,
        "lastLinePartial": value.last_line_partial,
        "firstLineExceedsLimit": value.first_line_exceeds_limit,
        "maxLines": value.max_lines,
        "maxBytes": value.max_bytes,
    })
}

/// Build a JSON object without inserting optional keys whose value is absent.
pub fn details_object(entries: impl IntoIterator<Item = (&'static str, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_flag_matches_upstream_shape() {
        let serialized = serde_json::to_value(strict_sampling())
            .unwrap_or_else(|error| panic!("could not serialize strict sampling: {error}"));
        assert_eq!(
            serialized,
            json!({"type": "json_schema", "strict": "prefer"})
        );
    }

    #[test]
    fn build_tool_sets_declaration_fields() {
        let tool = build_tool(
            "read",
            "desc",
            json!({"type": "object", "required": ["path"]}),
            true,
        );
        assert_eq!(tool.name, "read");
        assert_eq!(tool.description, "desc");
        assert_eq!(tool.parameters["required"], json!(["path"]));
        assert!(tool.constrained_sampling.is_some());
        let plain = build_tool("ls", "desc", json!({"type": "object"}), false);
        assert!(plain.constrained_sampling.is_none());
    }

    #[test]
    fn text_and_image_results_have_expected_blocks() {
        let text = text_result("hello");
        assert_eq!(
            text.content,
            vec![InputContent::Text(TextContent::from("hello"))]
        );
        assert!(
            !text
                .content
                .iter()
                .any(|c| matches!(c, InputContent::Image(_)))
        );

        let image = image_result("note".into(), "data".into(), "image/png".into());
        assert_eq!(image.content.len(), 2);
        assert_eq!(
            image.structured_content,
            Some(json!({"type": "image", "data": "data", "mimeType": "image/png", "note": "note"}))
        );
    }

    #[test]
    fn details_object_omits_absent_keys() {
        let value = details_object([
            ("truncation", None),
            ("fullOutputPath", Some(json!("/tmp/full.txt"))),
        ]);
        assert_eq!(value, json!({"fullOutputPath": "/tmp/full.txt"}));
    }

    #[test]
    fn truncation_value_uses_camel_case_keys() {
        let value = TruncationResult {
            content: "x".into(),
            truncated: true,
            truncated_by: TruncatedBy::Lines,
            total_lines: 10,
            total_bytes: 20,
            output_lines: 5,
            output_bytes: 10,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines: 2000,
            max_bytes: 51200,
        };
        let serialized = truncation_value(&value);
        assert_eq!(serialized["truncatedBy"], json!("lines"));
        assert_eq!(serialized["totalLines"], json!(10));
        assert_eq!(serialized["maxBytes"], json!(51200));
    }
}
