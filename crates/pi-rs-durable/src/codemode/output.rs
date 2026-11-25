//! Model-facing text, spill files and image paths for a sandbox result.

use std::{borrow::Cow, collections::HashMap, fmt::Write};

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use futures_util::future::join_all;
use pi_rs_ai::{ImageContent, InputContent, TextContent};
use pi_rs_chord::context::Context;
use pi_rs_codemode::{CodemodeResult, ErrorKind, OutputItem, StoreWrites};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

use super::{CodemodeDocuments, CodemodeNestedCall};
use crate::env::ExecutionEnv;

pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
const CHARS_PER_TOKEN: usize = 4;
const LINE_MARGIN: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeOutputLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

pub const OUTPUT_LIMITS: CodemodeOutputLimits = CodemodeOutputLimits {
    max_bytes: 4 * 1024 * 1024,
    max_lines: 100_000,
};

pub(super) struct OutputFiles<'a> {
    env: Option<&'a dyn ExecutionEnv>,
    documents: &'a dyn CodemodeDocuments,
    context: &'a Context,
    directory: OnceCell<Result<String, String>>,
}

impl<'a> OutputFiles<'a> {
    pub fn new(
        env: Option<&'a dyn ExecutionEnv>,
        documents: &'a dyn CodemodeDocuments,
        context: &'a Context,
    ) -> Self {
        Self {
            env,
            documents,
            context,
            directory: OnceCell::new(),
        }
    }

    async fn write(&self, name: &str, data: &[u8]) -> Result<String, String> {
        let env = self.env.ok_or("no execution environment")?;
        let directory = self
            .directory
            .get_or_init(|| async {
                if let Some(path) = self
                    .documents
                    .output_dir(None, self.context)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    return Ok(path);
                }
                let fresh = env
                    .create_temp_dir(Some("pi-codemode-"), self.context)
                    .await
                    .map_err(|error| error.to_string())?;
                self.documents
                    .output_dir(Some(fresh), self.context)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "output directory was not recorded".into())
            })
            .await;
        let directory = directory.as_ref().map_err(String::as_str)?;
        let path = env
            .join_path(&[directory, name], self.context)
            .await
            .map_err(|error| error.to_string())?;
        env.write_file(&path, data, self.context)
            .await
            .map_err(|error| error.to_string())?;
        Ok(path)
    }
}

fn value_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        value => value.to_string(),
    }
}

fn format_items(output: Vec<OutputItem>) -> Vec<InputContent> {
    let total = output
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { console: false, .. }))
        .count();
    let mut items = Vec::with_capacity(output.len());
    let mut console = String::new();
    let mut index = 0;
    for item in output {
        match item {
            OutputItem::Image { data, mime_type } => {
                items.push(InputContent::Image(ImageContent {
                    kind: Default::default(),
                    data,
                    mime_type,
                }))
            }
            OutputItem::Text {
                text,
                console: true,
            } => {
                if console.is_empty() {
                    console.push_str("<console_output>");
                }
                console.push('\n');
                console.push_str(&text);
            }
            OutputItem::Text {
                text,
                console: false,
            } => {
                index += 1;
                items.push(InputContent::Text(if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}").into()
                } else {
                    text.into()
                }));
            }
        }
    }
    if !console.is_empty() {
        console.push_str("\n</console_output>");
        items.push(InputContent::Text(console.into()));
    }
    items
}

fn join_adjacent_text(mut items: Vec<InputContent>) -> Vec<InputContent> {
    let mut count = 0;
    for idx in 0..items.len() {
        if count > 0
            && matches!(items[count - 1], InputContent::Text(_))
            && let InputContent::Text(content) = &mut items[idx]
        {
            let text = std::mem::take(&mut content.text);
            if let InputContent::Text(previous) = &mut items[count - 1] {
                if !previous.text.is_empty() && !previous.text.ends_with('\n') {
                    previous.text.push('\n');
                }
                previous.text.push_str(&text);
            }
        } else {
            items.swap(count, idx);
            count += 1;
        }
    }
    items.truncate(count);
    items
}

pub(super) fn byte_end_for_utf16(text: &str, units: usize) -> usize {
    let mut left = units;
    for (offset, ch) in text.char_indices() {
        let Some(remaining) = left.checked_sub(ch.len_utf16()) else {
            return offset;
        };
        left = remaining;
    }
    text.len()
}

fn byte_start_for_utf16(text: &str, units: usize) -> usize {
    let mut left = units;
    for (offset, ch) in text.char_indices().rev() {
        let Some(remaining) = left.checked_sub(ch.len_utf16()) else {
            return offset + ch.len_utf8();
        };
        left = remaining;
    }
    0
}

async fn truncate(
    mut items: Vec<InputContent>,
    max_tokens: u64,
    files: &OutputFiles<'_>,
) -> (Vec<InputContent>, Option<String>) {
    // At most three UTF-8 bytes per UTF-16 unit; leave room for headers and labels.
    let token_budget = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(CHARS_PER_TOKEN);
    let budget = token_budget.min(OUTPUT_LIMITS.max_bytes / 4);
    let line_budget = (OUTPUT_LIMITS.max_lines - LINE_MARGIN) / 2;
    let count = items
        .iter()
        .filter(|item| matches!(item, InputContent::Text(_)))
        .count();
    if count == 0 {
        return (items, None);
    }
    let texts = || {
        items.iter().filter_map(|item| match item {
            InputContent::Text(block) => Some(block.text.as_str()),
            InputContent::Image(_) => None,
        })
    };
    let (units, lines, bytes) = texts().fold(
        (count - 1, count, count - 1),
        |(units, lines, bytes), text| {
            (
                units + text.encode_utf16().count(),
                lines + text.split('\n').count() - 1,
                bytes + text.len(),
            )
        },
    );
    if units <= budget && lines <= 2 * line_budget {
        return (items, None);
    }
    let combined = if count == 1 {
        Cow::Borrowed(texts().next().unwrap_or_default())
    } else {
        let mut combined = String::with_capacity(bytes);
        for (index, text) in texts().enumerate() {
            if index > 0 {
                combined.push('\n');
            }
            combined.push_str(text);
        }
        Cow::Owned(combined)
    };
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let head = &combined[..byte_end_for_utf16(&combined, head_chars)];
    let head = head
        .splitn(line_budget + 1, '\n')
        .nth(line_budget)
        .map_or(head, |rest| &head[..head.len() - rest.len() - 1]);
    let tail = &combined[byte_start_for_utf16(&combined, tail_chars)..];
    let tail = tail
        .rsplitn(line_budget + 1, '\n')
        .nth(line_budget)
        .map_or(tail, |rest| &tail[rest.len() + 1..]);
    let removed = units.saturating_sub(head.encode_utf16().count() + tail.encode_utf16().count());
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {lines}\n\n{head}…{} tokens truncated…{tail}",
        units.div_ceil(CHARS_PER_TOKEN),
        removed.div_ceil(CHARS_PER_TOKEN)
    );
    let path = match files.write("output.txt", combined.as_bytes()).await {
        Ok(path) => {
            let _ = write!(text, "\n\n[Full output: {path} (read with offset/limit)]");
            Some(path)
        }
        Err(error) => {
            let _ = write!(text, "\n\n[Could not save the full output: {error}]");
            None
        }
    };
    drop(combined);
    items.retain(|item| matches!(item, InputContent::Image(_)));
    items.insert(0, InputContent::Text(text.into()));
    (items, path)
}

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

async fn image_label(image: &ImageContent, index: usize, files: &OutputFiles<'_>) -> String {
    let extension = match image.mime_type.as_str() {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => ".bin",
    };
    let whitespace = |byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c);
    let raw = image.data.as_bytes();
    let encoded = if raw.iter().copied().any(whitespace) {
        Cow::Owned(
            raw.iter()
                .copied()
                .filter(|byte| !whitespace(*byte))
                .collect::<Vec<_>>(),
        )
    } else {
        Cow::Borrowed(raw)
    };
    let bytes = match BASE64.decode(encoded) {
        Ok(bytes) => bytes,
        Err(error) => return format!("[Image ({}) could not be saved: {error}]", image.mime_type),
    };
    let kind = format!("{}, {} bytes", image.mime_type, bytes.len());
    match files
        .write(&format!("image-{index}{extension}"), &bytes)
        .await
    {
        Ok(path) => format!("[Image saved to {path} ({kind})]"),
        Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
    }
}

async fn save_images(items: Vec<InputContent>, files: &OutputFiles<'_>) -> Vec<InputContent> {
    let count = items
        .iter()
        .filter(|item| matches!(item, InputContent::Image(_)))
        .count();
    if count == 0 {
        return items;
    }
    let mut images = HashMap::new();
    let mut indices = Vec::with_capacity(count);
    let mut pending = Vec::new();
    let mut uses = Vec::new();
    for image in items.iter().filter_map(|item| match item {
        InputContent::Image(image) => Some(image),
        _ => None,
    }) {
        let index = pending.len();
        let known = match images.entry(image.data.as_str()) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(index);
                pending.push(image_label(image, index + 1, files));
                uses.push(0);
                index
            }
        };
        uses[known] += 1;
        indices.push(known);
    }
    drop(images);
    let mut labels = join_all(pending).await;
    let mut indices = indices.into_iter();
    let mut content = Vec::with_capacity(items.len() + count);
    for item in items {
        if matches!(item, InputContent::Image(_))
            && let Some(index) = indices.next()
        {
            uses[index] -= 1;
            let label = if uses[index] == 0 {
                std::mem::take(&mut labels[index])
            } else {
                labels[index].clone()
            };
            content.push(InputContent::Text(label.into()));
        }
        content.push(item);
    }
    content
}

pub(super) struct FormattedOutput {
    pub output: Vec<InputContent>,
    pub full_output_path: Option<String>,
    pub writes: Option<StoreWrites>,
    pub is_error: bool,
}

pub(super) async fn format_result(
    result: CodemodeResult,
    generated_images: usize,
    wall_ms: u64,
    max_output_tokens: u64,
    calls: &[CodemodeNestedCall],
    files: &OutputFiles<'_>,
) -> FormattedOutput {
    let (output, error, writes) = match result {
        CodemodeResult::Success {
            mut output,
            value,
            store_writes,
            ..
        } => {
            if let Some(value) = value {
                output.push(OutputItem::Text {
                    text: value_text(value),
                    console: false,
                });
            }
            (output, None, Some(store_writes))
        }
        CodemodeResult::Failure { output, error, .. } => (output, Some(error), None),
    };
    let is_error = error.is_some();
    let mut items = format_items(output);
    if let Some(error) = error {
        let error = match error.kind {
            ErrorKind::Script => error.stack.unwrap_or_else(|| {
                format!(
                    "{}: {}",
                    error.name.as_deref().unwrap_or("Error"),
                    error.message
                )
            }),
            ErrorKind::Timeout => format!("Script timed out: {}", error.message),
            ErrorKind::Aborted => format!("Script aborted: {}", error.message),
            ErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
        };
        let mut text = format!("Script error:\n{error}\n\n");
        if calls.is_empty() {
            text.push_str("No tool calls were made.");
        } else {
            text.push_str("Tool calls made before the failure (they are not undone): ");
            for (index, call) in calls.iter().enumerate() {
                if index > 0 {
                    text.push_str(", ");
                }
                let _ = write!(text, "{} ({})", call.name, call.status.as_str());
            }
        }
        items.push(InputContent::Text(text.into()));
    }
    if generated_images > 0
        && !items
            .iter()
            .any(|item| matches!(item, InputContent::Image(_)))
    {
        let plural = if generated_images == 1 { "" } else { "s" };
        items.push(InputContent::Text(format!("Note: models.generateImages() returned {generated_images} image{plural} that the script did not show. Show each image block of result.output with image(block).").into()));
    }
    let (items, full_output_path) =
        truncate(join_adjacent_text(items), max_output_tokens, files).await;
    let mut output = join_adjacent_text(save_images(items, files).await);
    let state = if is_error { "failed" } else { "completed" };
    let tenths = wall_ms.saturating_add(50) / 100;
    output.insert(
        0,
        InputContent::Text(TextContent::from(format!(
            "Script {state}\nWall time {}.{} seconds\nOutput:\n",
            tenths / 10,
            tenths % 10
        ))),
    );
    FormattedOutput {
        output,
        full_output_path,
        writes,
        is_error,
    }
}
