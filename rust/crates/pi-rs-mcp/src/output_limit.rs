//! The size limit of the model-facing text of MCP results (`packages/mcp/src/output-limit.ts`).
//! Longer text keeps its start and end with the middle cut out, like Codex does, and the caller
//! may save the full text where the model can read it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::protocol::LlmContent;

/// Model-facing text of an MCP result beyond this is cut in the middle.
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

/// What `save_output` receives: the full text of a truncated result, or the bytes of a binary
/// resource, with the file extension (including the dot) to save it under.
pub type McpOutputSaver = Arc<
    dyn Fn(SaveOutput, &str) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// The data handed to an [`McpOutputSaver`].
#[derive(Clone, Debug, PartialEq)]
pub enum SaveOutput {
    Text(String),
    Bytes(Vec<u8>),
}

/// Model-facing content within the limit, and where the full text went when it was saved.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitedMcpContent {
    pub content: Vec<LlmContent>,
    pub full_output_path: Option<String>,
}

/// Keep model-facing text within [`MCP_OUTPUT_MAX_BYTES`]. Longer text becomes one text block in
/// Codex's truncation format, followed by the path of the file with the full text when
/// `save_output` is given; images follow it. Content within the limit is returned as it is.
pub async fn limit_mcp_content(
    content: Vec<LlmContent>,
    save_output: Option<McpOutputSaver>,
) -> LimitedMcpContent {
    let combined = content
        .iter()
        .filter_map(LlmContent::text)
        .collect::<Vec<_>>()
        .join("\n");
    let Some(truncation) = truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES) else {
        return LimitedMcpContent {
            content,
            full_output_path: None,
        };
    };
    let mut full_output_path = None;
    let mut where_note = String::new();
    if let Some(save_output) = save_output {
        match save_output(SaveOutput::Text(combined.clone()), ".txt").await {
            Ok(path) => {
                full_output_path = Some(path.clone());
                where_note = format!("\n\n[Full output: {path} (read it with offset/limit)]");
            }
            Err(error) => {
                where_note = format!("\n\n[Could not save the full output: {error}]");
            }
        }
    }
    let tokens = (truncation.total_bytes as f64 / 4.0).ceil();
    let text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}{}",
        format_number(tokens),
        truncation.total_lines,
        truncation.content,
        where_note
    );
    let mut out = vec![LlmContent::Text { text }];
    out.extend(
        content
            .into_iter()
            .filter(|block| matches!(block, LlmContent::Image { .. })),
    );
    LimitedMcpContent {
        content: out,
        full_output_path,
    }
}

/// Text cut in the middle, with the size of the whole text.
struct MiddleTruncation {
    /// The start and the end of the text with a `…N chars truncated…` marker between them.
    content: String,
    total_bytes: usize,
    total_lines: usize,
}

/// Keep the start and the end of `text`, half of `max_bytes` each, and replace the middle with a
/// `…N chars truncated…` marker. Cuts only at character boundaries. `None` when `text` fits.
fn truncate_middle(text: &str, max_bytes: usize) -> Option<MiddleTruncation> {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return None;
    }
    // Continuation bytes (10xxxxxx) are not character starts.
    let is_boundary = |index: usize| index >= bytes.len() || (bytes[index] & 0xc0) != 0x80;
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !is_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = bytes.len() - (max_bytes - max_bytes / 2);
    while tail_start < bytes.len() && !is_boundary(tail_start) {
        tail_start += 1;
    }
    let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
    let tail = String::from_utf8_lossy(&bytes[tail_start..]).into_owned();
    let removed = String::from_utf8_lossy(&bytes[head_end..tail_start])
        .chars()
        .count();
    // A final newline ends the last line instead of starting another.
    let lines = text.split('\n').count() - usize::from(text.ends_with('\n'));
    Some(MiddleTruncation {
        content: format!("{head}…{removed} chars truncated…{tail}"),
        total_bytes: bytes.len(),
        total_lines: lines,
    })
}

/// `Math.ceil`'s result as JavaScript prints it: whole numbers without a fraction.
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}
