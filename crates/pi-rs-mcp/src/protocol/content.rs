//! MCP content blocks and tool results (`packages/mcp/src/protocol/content.ts`).
//!
//! Blocks are raw JSON: servers send shapes of any version, and every conversion in this crate
//! reads them structurally, like the TypeScript it replaces. The interfaces upstream declares are
//! documented on the functions that read them.

use serde::Deserialize;
use serde_json::Value;

/// Text and image content in the shape LLM APIs accept, matching `@earendil-works/pi-ai`'s
/// `TextContent` and `ImageContent`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum LlmContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
}

impl LlmContent {
    /// The text of a text block, or `None` for an image.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Image { .. } => None,
        }
    }
}

/// A `CallToolResult`: `content` blocks, any JSON value as `structuredContent` (MCP 2026-07-28;
/// earlier revisions allowed only objects), and the fields applications read off it. Fields the
/// struct does not name are kept in `extra`, verbatim.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CallToolResult {
    /// Required by the spec, but servers that only return `structuredContent` omit it; the client
    /// defaults it to an empty list.
    #[serde(default)]
    pub content: Vec<Value>,
    /// Any JSON value, including arrays and `null`. A `null` member stays `null`, which a plain
    /// `Option` would erase.
    #[serde(
        rename = "structuredContent",
        default,
        deserialize_with = "optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    /// Whether the tool failed, reported in the result instead of as a protocol error.
    #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

impl CallToolResult {
    /// Reads a result out of its JSON shape, with `content` defaulted like the client does.
    pub fn from_json(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

/// Deserializes an optional JSON value the way the wire carries it: an absent member is `None`,
/// a `null` member is `Some(null)`.
fn optional_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// One block of a result, as the model sees it: text, an image, or a short placeholder.
fn block_to_llm_content(block: &Value) -> LlmContent {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => LlmContent::Text {
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("image") => LlmContent::Image {
            data: block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mime_type: block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("audio") => LlmContent::Text {
            text: format!(
                "[audio {} omitted]",
                block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
        },
        Some("resource_link") => LlmContent::Text {
            text: format!(
                "{}: {}",
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                block.get("uri").and_then(Value::as_str).unwrap_or_default()
            ),
        },
        Some("resource") => {
            let Some(resource) = block.get("resource") else {
                return unsupported(block);
            };
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                return LlmContent::Text {
                    text: text.to_string(),
                };
            }
            let mime_type = resource.get("mimeType").and_then(Value::as_str);
            if mime_type.is_some_and(|mime| mime.starts_with("image/")) {
                return LlmContent::Image {
                    data: resource
                        .get("blob")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    mime_type: mime_type.unwrap_or_default().to_string(),
                };
            }
            LlmContent::Text {
                text: format!(
                    "[binary resource {} ({}) omitted]",
                    resource
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    mime_type.unwrap_or("unknown type"),
                ),
            }
        }
        _ => unsupported(block),
    }
}

fn unsupported(block: &Value) -> LlmContent {
    LlmContent::Text {
        text: format!(
            "[unsupported MCP content {}]",
            block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
    }
}

/// Convert a tool result to text and image content for a model. Text and images pass through,
/// embedded text resources become text, embedded image resources become images, and other blocks
/// (audio, resource links, binary resources) become a short text placeholder. A result without
/// content blocks but with `structuredContent` becomes its JSON, since servers should, but do not
/// always, mirror structured results as text.
pub fn to_llm_content(result: &CallToolResult) -> Vec<LlmContent> {
    let mut content: Vec<LlmContent> = result.content.iter().map(block_to_llm_content).collect();
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content.push(LlmContent::Text {
            text: serde_json::to_string_pretty(structured).unwrap_or_default(),
        });
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_without_content_defaults_to_an_empty_list() {
        let result = CallToolResult::from_json(&json!({ "structuredContent": { "n": 1 } }));
        assert_eq!(
            result,
            Some(CallToolResult {
                content: Vec::new(),
                structured_content: Some(json!({ "n": 1 })),
                ..CallToolResult::default()
            })
        );
    }
}
