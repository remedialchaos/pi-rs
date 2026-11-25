use pi_rs_ai::{ImageContent, ImageContentKind, InputContent, Model};
use pi_rs_chord::{context::Context, output::character_end};
use pi_rs_schema::{Annotations, Properties, lit, obj, opt, str, union};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    env::{BinaryReader, ExecutionEnv, FileInfo, ScanLinesOptions, range_decoder, starts_with_bom},
    images::{
        INLINE_IMAGE_TYPES, ImageProcessor, base64_length, image_limits, prepared_notes, to_base64,
    },
    truncate::{
        DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TextTotals, TruncationLimit, TruncationOptions,
        TruncationResult, format_size, truncate_head_of,
    },
};

use super::image::{detect_supported_image_mime_type_of, image_dimensions};
use super::{
    DiagnosticSeverity, ToolDiagnostic, ToolError, ToolResult, ToolSpec, described_num,
    described_str, require_env, resolve_read_tool_path, text_output,
};

const READ_CHUNK: u64 = 64 * 1024;
const MAX_SAFE_INTEGER: f64 = ((1_u64 << 53) - 1) as f64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadToolInput {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTruncation {
    pub truncated: bool,
    pub truncated_by: Option<TruncationLimit>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl From<TruncationResult<'_>> for ReadTruncation {
    fn from(result: TruncationResult<'_>) -> Self {
        Self {
            truncated: result.truncated,
            truncated_by: result.truncated_by,
            total_lines: result.total_lines,
            total_bytes: result.total_bytes,
            output_lines: result.output_lines,
            output_bytes: result.output_bytes,
            last_line_partial: result.last_line_partial,
            first_line_exceeds_limit: result.first_line_exceeds_limit,
            max_lines: result.max_lines,
            max_bytes: result.max_bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ReadTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_unsupported_by_model: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct ReadToolOptions<'a> {
    pub images: Option<&'a dyn ImageProcessor>,
}

pub struct ReadTool<'a> {
    spec: ToolSpec,
    options: ReadToolOptions<'a>,
}

pub fn create_read_tool(options: ReadToolOptions<'_>) -> ReadTool<'_> {
    let formats = if options.images.is_some() {
        "jpg, png, gif, webp, bmp"
    } else {
        "jpg, png, gif, webp"
    };
    let annotations = Annotations::new();
    let image = obj(
        &Properties::new()
            .with("type", &lit(Value::String("image".into()), &annotations))
            .with("data", &str())
            .with("mimeType", &str()),
        &annotations,
        true,
    );
    let spec = ToolSpec {
        name: "read",
        description: format!(
            "Read the contents of a file. Supports text files and images ({formats}). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        ),
        parameters: obj(
            &Properties::new()
                .with(
                    "path",
                    &described_str("Path to the file to read (relative or absolute)"),
                )
                .with_optional(
                    "offset",
                    &opt(&described_num(
                        "Line number to start reading from (1-indexed)",
                    )),
                )
                .with_optional(
                    "limit",
                    &opt(&described_num("Maximum number of lines to read")),
                ),
            &annotations,
            true,
        ),
        structured_output_schema: Some(union(&[str(), image], &annotations)),
        output_limits: None,
    };
    ReadTool { spec, options }
}

impl ReadTool<'_> {
    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub async fn execute(
        &self,
        args: ReadToolInput,
        env: Option<&dyn ExecutionEnv>,
        context: &Context,
    ) -> Result<ToolResult<ReadToolDetails>, ToolError> {
        self.execute_with_model(args, env, context, || async { Ok(None) })
            .await
    }

    /// The model callback is only used when reading an image. Text reads do not inspect model state.
    pub async fn execute_with_model<F>(
        &self,
        args: ReadToolInput,
        env: Option<&dyn ExecutionEnv>,
        context: &Context,
        model: impl Fn() -> F,
    ) -> Result<ToolResult<ReadToolDetails>, ToolError>
    where
        F: Future<Output = Result<Option<Model>, ToolError>>,
    {
        let env = require_env(env)?;
        let absolute = resolve_read_tool_path(env, &args.path, context).await?;
        let mut reader = env.open_binary_reader(&absolute, None, context).await?;
        let result = async {
            for _ in 0..2 {
                let before = reader.info(context).await?;
                let mime_type =
                    detect_supported_image_mime_type_of(reader.as_mut(), before.size, context)
                        .await?;
                let result = if let Some(mime_type) = mime_type {
                    let model = model().await?;
                    read_image(
                        reader.as_mut(),
                        &before,
                        &args.path,
                        mime_type,
                        self.options.images,
                        model.as_ref(),
                        context,
                    )
                    .await?
                } else {
                    read_text(reader.as_mut(), &args, context).await?
                };
                let after = reader.info(context).await?;
                let unchanged = before.size == after.size && before.mtime_ms == after.mtime_ms;
                if unchanged || (mime_type.is_none() && after.size > before.size) {
                    return Ok(result);
                }
            }
            Err(ToolError::Message(format!(
                "{} changed while it was read",
                args.path
            )))
        }
        .await;
        reader.close(context).await;
        result
    }
}

fn image_error(message: String) -> ToolResult<ReadToolDetails> {
    ToolResult {
        output: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![ToolDiagnostic::new(
            DiagnosticSeverity::Error,
            Some("unsupported_image"),
            message,
        )]),
        ..ToolResult::default()
    }
}

async fn read_image(
    reader: &mut dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    mime_type: &str,
    processor: Option<&dyn ImageProcessor>,
    model: Option<&Model>,
    context: &Context,
) -> Result<ToolResult<ReadToolDetails>, ToolError> {
    let limits = image_limits(model);
    let mut notes = Vec::new();
    let image: ImageContent = if let Some(processor) = processor {
        let bytes = reader.read(0, info.size, context).await?;
        let Some(prepared) = processor.prepare(bytes.into(), mime_type, limits).await? else {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) that cannot be prepared for the model"
            )));
        };
        notes = prepared_notes(&prepared);
        prepared.into()
    } else {
        if !INLINE_IMAGE_TYPES.contains(&mime_type) {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) that needs converting, and no image processor is configured"
            )));
        }
        const UNSHRINKABLE: &str = "and no image processor is configured to shrink it";
        if base64_length(info.size) > limits.max_bytes {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {}, too large to send, {UNSHRINKABLE}",
                format_size(as_usize(info.size))
            )));
        }
        let bytes = reader.read(0, info.size, context).await?;
        let Some(size) = image_dimensions(&bytes, mime_type) else {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) whose size cannot be read"
            )));
        };
        if u64::from(size.width) > limits.max_width || u64::from(size.height) > limits.max_height {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {}x{}, larger than {}x{}, {UNSHRINKABLE}",
                size.width, size.height, limits.max_width, limits.max_height
            )));
        }
        ImageContent {
            kind: ImageContentKind::Image,
            data: to_base64(&bytes),
            mime_type: mime_type.into(),
        }
    };
    let mut message = format!("Read image file [{}].", image.mime_type);
    for note in notes {
        message.push(' ');
        message.push_str(&note);
    }
    Ok(ToolResult {
        structured_output: Some(serde_json::to_value(&image)?),
        output: Some(vec![InputContent::Image(image)]),
        details: model
            .filter(|model| !model.supports_images())
            .map(|model| ReadToolDetails {
                truncation: None,
                image_unsupported_by_model: Some(model.id.clone()),
            }),
        diagnostics: Some(vec![ToolDiagnostic::new(
            DiagnosticSeverity::Info,
            Some("image"),
            message,
        )]),
        ..ToolResult::default()
    })
}

fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

async fn read_head(
    reader: &mut dyn BinaryReader,
    start: u64,
    end: u64,
    skip_bom: bool,
    context: &Context,
) -> Result<String, ToolError> {
    let mut decoder = range_decoder();
    let mut text = String::new();
    let mut newlines = 0;
    let mut position = if skip_bom && start == 0 { 3 } else { start };
    while position < end {
        let bytes = reader
            .read(position, READ_CHUNK.min(end - position), context)
            .await?;
        if bytes.is_empty() {
            break;
        }
        position += bytes.len() as u64;
        let decoded = decoder.decode(Some(&bytes));
        newlines += decoded.bytes().filter(|&byte| byte == b'\n').count();
        text.push_str(&decoded);
        if newlines >= DEFAULT_MAX_LINES || text.len() > DEFAULT_MAX_BYTES + 1 {
            return Ok(text);
        }
    }
    text.push_str(&decoder.decode(None));
    Ok(text)
}

fn slice_index(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value.trunc() }
}

async fn read_text(
    reader: &mut dyn BinaryReader,
    args: &ReadToolInput,
    context: &Context,
) -> Result<ToolResult<ReadToolDetails>, ToolError> {
    let start = (args.offset.unwrap_or(1.0) - 1.0).max(0.0);
    let display_start = start + 1.0;
    let slice_start = slice_index(start);
    let scan_start = if slice_start <= MAX_SAFE_INTEGER {
        slice_start as u64
    } else {
        0
    };
    let scan_end = args
        .limit
        .map(|limit| slice_index(start + limit).max((scan_start + 1) as f64))
        .filter(|end| *end <= MAX_SAFE_INTEGER)
        .map(|end| end as u64);
    let mut scan = reader
        .scan_lines(
            ScanLinesOptions {
                start_line: scan_start,
                end_line: scan_end,
            },
            context,
        )
        .await?;
    let total = scan.newlines + 1;
    if start >= total as f64 {
        let offset = args.offset.unwrap_or(1.0);
        return Err(ToolError::Message(format!(
            "Offset {offset} is beyond end of file ({total} lines total)"
        )));
    }
    let mut selection = total - slice_start as u64;
    let mut limited_lines = None;
    if let Some(limit) = args.limit {
        let end = if limit.is_nan() {
            f64::NAN
        } else {
            (start + limit).min(total as f64)
        };
        limited_lines = Some(end - start);
        let relative_end = slice_index(end);
        let slice_end = if relative_end < 0.0 {
            (total as f64 + relative_end).max(0.0)
        } else {
            relative_end
        };
        selection = (slice_end as u64).saturating_sub(slice_start as u64);
        if selection != 0 && relative_end < 0.0 {
            scan = reader
                .scan_lines(
                    ScanLinesOptions {
                        start_line: scan_start,
                        end_line: Some(slice_end as u64),
                    },
                    context,
                )
                .await?;
        }
    }
    let trailing_newline =
        selection != 0 && scan.last_line_start == scan.end && scan.last_line_start > scan.start;
    let totals = TextTotals {
        lines: if scan.selected_bytes == 0 {
            0
        } else {
            as_usize(selection - u64::from(trailing_newline))
        },
        bytes: if selection == 0 {
            0
        } else {
            as_usize(scan.selected_bytes)
        },
    };
    let head = if selection == 0 {
        String::new()
    } else {
        let first = reader.read(0, 3, context).await?;
        read_head(
            reader,
            scan.start,
            scan.end,
            starts_with_bom(&first),
            context,
        )
        .await?
    };

    let cut = truncate_head_of(&head, totals, TruncationOptions::default());
    let mut diagnostics = Vec::new();
    let mut output = cut.content;
    let mut truncation = cut.truncated.then_some(ReadTruncation::from(cut));
    if cut.first_line_exceeds_limit {
        let integral = start.fract() == 0.0;
        let line = if integral {
            head.split('\n').next().unwrap_or("")
        } else {
            ""
        };
        let bytes = if integral {
            as_usize(scan.first_line_bytes)
        } else {
            0
        };
        let end = character_end(line.as_bytes(), DEFAULT_MAX_BYTES.min(line.len()));
        output = &line[..end];
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Warn, Some("truncated"), format!(
            "Line {display_start} is {}, exceeds the {} limit; showing its first {}. Use bash: sed -n '{display_start}p' {} | tail -c +{}",
            format_size(bytes), format_size(DEFAULT_MAX_BYTES), format_size(end), args.path, end + 1)));
        if let Some(details) = &mut truncation {
            details.output_bytes = end;
            details.output_lines = 1;
        }
    } else if cut.truncated {
        let display_end = display_start + cut.output_lines as f64 - 1.0;
        let next_offset = display_end + 1.0;
        let limit = if cut.truncated_by == Some(TruncationLimit::Lines) {
            String::new()
        } else {
            format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
        };
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, Some("truncated"), format!(
            "Showing lines {display_start}-{display_end} of {total}{limit}. Use offset={next_offset} to continue.")));
    } else if let Some(limited) = limited_lines.filter(|limited| start + limited < total as f64) {
        let remaining = total as f64 - (start + limited);
        let next_offset = start + limited + 1.0;
        diagnostics.push(ToolDiagnostic::new(
            DiagnosticSeverity::Info,
            None,
            format!("{remaining} more lines in file. Use offset={next_offset} to continue."),
        ));
    }
    let content = if output.is_empty() {
        Vec::new()
    } else {
        vec![text_output(output.into())]
    };
    Ok(ToolResult {
        output: Some(content),
        structured_output: Some(Value::String(output.into())),
        details: truncation.map(|truncation| ReadToolDetails {
            truncation: Some(truncation),
            image_unsupported_by_model: None,
        }),
        diagnostics: Some(diagnostics),
        ..ToolResult::default()
    })
}
