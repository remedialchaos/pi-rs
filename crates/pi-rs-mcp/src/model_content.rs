//! The model-facing content of an MCP result as pi's coding agent shows it
//! (`packages/mcp/src/model-content.ts`): text and images, embedded text resources as text,
//! binary resources saved where the model can read them, and resource links that name the tool
//! reading them.

use serde_json::Value;
use url::Url;

use crate::output_limit::{McpOutputSaver, SaveOutput};
use crate::protocol::{CallToolResult, LlmContent, is_object, to_llm_content};

/// How [`to_mcp_model_content`] handles what the model cannot read inline.
#[derive(Clone, Default)]
pub struct McpModelContentOptions {
    /// Saves binary resources other than images where the model can read them. Without it they
    /// are left out.
    pub save_output: Option<McpOutputSaver>,
    /// The tool that reads the server's resources, which resource links then name.
    pub read_resource_tool: Option<String>,
}

/// Model-facing content of `server`'s result, before the output limit
/// ([`limit_mcp_content`](crate::limit_mcp_content)). A result without content blocks becomes its
/// structured content as JSON, as in `to_llm_content`.
pub async fn to_mcp_model_content(
    server: &str,
    result: &CallToolResult,
    options: McpModelContentOptions,
) -> Vec<LlmContent> {
    if result.content.is_empty() {
        return to_llm_content(result);
    }
    let mut out = Vec::new();
    for block in &result.content {
        out.extend(block_to_content(server, block, &options).await);
    }
    out
}

/// Model-facing content of one block of `server`'s result.
async fn block_to_content(
    server: &str,
    block: &Value,
    options: &McpModelContentOptions,
) -> Vec<LlmContent> {
    if block.get("type").and_then(Value::as_str) == Some("resource_link") {
        let mime_type = block.get("mimeType").and_then(Value::as_str);
        let size = block.get("size").and_then(Value::as_f64).map(format_size);
        let details = [mime_type, size.as_deref()]
            .into_iter()
            .flatten()
            .filter(|detail| !detail.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let read = options
            .read_resource_tool
            .as_ref()
            .map(|tool| format!(". Read it with {tool} (server \"{server}\")"))
            .unwrap_or_default();
        let description = block
            .get("description")
            .and_then(Value::as_str)
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        let quoted_name = block
            .get("title")
            .or_else(|| block.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return vec![LlmContent::Text {
            text: format!(
                "[Resource {} \"{}\"{}{}{}]",
                block.get("uri").and_then(Value::as_str).unwrap_or_default(),
                quoted_name,
                if details.is_empty() {
                    String::new()
                } else {
                    format!(" ({details})")
                },
                description,
                read,
            ),
        }];
    }
    if block.get("type").and_then(Value::as_str) == Some("resource")
        && block.get("resource").is_some_and(is_object)
    {
        let resource = block.get("resource").unwrap_or(&Value::Null);
        let has_blob = resource.get("blob").is_some_and(|blob| blob.is_string());
        let mime_type = resource.get("mimeType").and_then(Value::as_str);
        if has_blob && !mime_type.is_some_and(|mime| mime.starts_with("image/")) {
            let uri = resource
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let blob = resource
                .get("blob")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let data = decode_base64_lenient(blob);
            if is_text_mime_type(mime_type) {
                return vec![LlmContent::Text {
                    text: String::from_utf8_lossy(&data).into_owned(),
                }];
            }
            let Some(save_output) = &options.save_output else {
                return to_llm_content(&CallToolResult {
                    content: vec![block.clone()],
                    ..CallToolResult::default()
                });
            };
            let kind = format!(
                "{}, {}",
                mime_type.unwrap_or("unknown type"),
                format_size(data.len() as f64)
            );
            return match save_output(SaveOutput::Bytes(data.clone()), &extension_of(uri)).await {
                Ok(path) => vec![LlmContent::Text {
                    text: format!("[Binary resource {uri} ({kind}) saved to {path}]"),
                }],
                Err(reason) => {
                    vec![LlmContent::Text {
                        text: format!(
                            "[Binary resource {uri} ({kind}) could not be saved: {reason}]"
                        ),
                    }]
                }
            };
        }
    }
    to_llm_content(&CallToolResult {
        content: vec![block.clone()],
        ..CallToolResult::default()
    })
}

/// A byte count as pi's coding agent shows sizes: `512B`, `2.0KB`, `1.5MB`.
fn format_size(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{}B", format_number(bytes));
    }
    if bytes < 1024.0 * 1024.0 {
        return format!("{:.1}KB", bytes / 1024.0);
    }
    format!("{:.1}MB", bytes / (1024.0 * 1024.0))
}

/// `Math.round`'s result as JavaScript prints it: whole numbers without a fraction.
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Blobs of these types are shown as text.
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let kind = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || kind == "application/json"
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}

/// File extension for a saved binary resource: the one its URI ends in, else `.bin`.
fn extension_of(uri: &str) -> String {
    let path = Url::parse(uri)
        .map(|url| url.path().to_string())
        .unwrap_or_else(|_| uri.to_string());
    path.rfind('.')
        .filter(|&dot| {
            let extension = &path[dot + 1..];
            (1..=8).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(|dot| path[dot..].to_string())
        .unwrap_or_else(|| ".bin".to_string())
}

/// Base64 as Node's `Buffer.from(data, "base64")` decodes it: characters outside the alphabet are
/// skipped, padding is optional, and a trailing group of two or three characters yields one or
/// two bytes.
fn decode_base64_lenient(data: &str) -> Vec<u8> {
    let mut collected = Vec::new();
    for byte in data.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => collected.push(byte),
            b'=' => break,
            _ => {}
        }
    }
    let mut out = Vec::with_capacity(collected.len() / 4 * 3 + 3);
    let mut chunk = [0u8; 4];
    let mut filled = 0;
    for byte in collected {
        chunk[filled] = byte;
        filled += 1;
        if filled == 4 {
            out.extend(decode_chunk(&chunk));
            filled = 0;
        }
    }
    match filled {
        2 | 3 => out.extend(decode_chunk(&chunk[..filled])),
        _ => {}
    }
    out
}

/// Decodes one 4-character (or 2/3-character trailing) base64 group.
fn decode_chunk(chunk: &[u8]) -> Vec<u8> {
    let value = |byte: u8| -> u32 {
        match byte {
            b'A'..=b'Z' => u32::from(byte) - u32::from(b'A'),
            b'a'..=b'z' => u32::from(byte) - u32::from(b'a') + 26,
            b'0'..=b'9' => u32::from(byte) - u32::from(b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let mut bits = 0u32;
    for byte in chunk {
        bits = (bits << 6) | value(*byte);
    }
    match chunk.len() {
        2 => vec![(bits >> 4) as u8],
        3 => vec![(bits >> 10) as u8, (bits >> 2) as u8],
        4 => vec![(bits >> 16) as u8, (bits >> 8) as u8, bits as u8],
        _ => Vec::new(),
    }
}
