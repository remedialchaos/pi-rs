use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::{Arc, Once};

use pi_rs_agent::AgentToolResult;
use pi_rs_ai::InputContent;
use pi_rs_tui::{
    Component, Image, ImageOptions, ImageTheme, allocate_image_id, get_image_dimensions,
    image_fallback, set_image_transcoder, utils::truncate_with_ellipsis,
};
use serde_json::Value;

use crate::core::{
    bash_executor::{BashOutput, MAX_OUTPUT_BYTES},
    messages::BashExecutionMessage,
    tools::{
        renderers,
        truncate::{TruncationOptions, truncate_tail},
    },
};
use crate::utils::{image_convert::convert_to_png, shell::sanitize_binary_output};

use super::{
    RenderContext, Theme, diff,
    style::{self, Keep, bold, clean, text_rows},
};

pub(super) struct Tool {
    pub(super) name: String,
    pub(super) args: Value,
    pub(super) result: Option<Arc<AgentToolResult>>,
    pub(super) pending: bool,
    pub(super) is_error: bool,
    pub(super) duration_ms: Option<u64>,
    image_ids: Vec<u32>,
}

impl Tool {
    pub(super) fn new(name: &str, args: Value) -> Self {
        Self {
            name: name.to_owned(),
            args,
            result: None,
            pending: true,
            is_error: false,
            duration_ms: None,
            image_ids: Vec::new(),
        }
    }

    pub(super) fn has_result(&self) -> bool {
        self.result.is_some()
    }

    pub(super) fn render(&mut self, ctx: &RenderContext<'_>, expanded: bool) -> Vec<String> {
        let args = &self.args;
        let extension = ctx
            .extensions
            .and_then(|ext| ext.resolve_tool_renderers(&self.name));
        let shell = matches!(self.name.as_str(), "bash" | "powershell");
        let self_rendered = extension
            .as_ref()
            .and_then(|ext| ext.render_shell.as_deref())
            == Some("self");
        let theme = ctx.theme;
        let label = if let Some(call) = extension.as_ref().and_then(|ext| ext.call.as_deref()) {
            theme.fg("toolTitle", &bold(&clean(call)))
        } else {
            match self.name.as_str() {
                "read" => styled_call(theme, &renderers::render_read_call(args)),
                "write" => {
                    // The shared renderer supplies the header and validates arguments. The
                    // transcript owns how many *visual* rows of the body to show.
                    let path = args
                        .get("file_path")
                        .filter(|path| path.is_string())
                        .or_else(|| args.get("path"));
                    let header_args = serde_json::json!({"path": path, "content": ""});
                    styled_call(theme, &renderers::render_write_call(&header_args))
                }
                "edit" => styled_call(theme, &renderers::render_edit_call(args)),
                "find" => styled_call(theme, &renderers::render_find_call(args)),
                "grep" => styled_call(theme, &renderers::render_grep_call(args)),
                "ls" => styled_call(theme, &renderers::render_ls_call(args)),
                "bash" => theme.fg(
                    "toolTitle",
                    &bold(&clean(&renderers::render_bash_call(args))),
                ),
                "powershell" => theme.fg(
                    "toolTitle",
                    &bold(&clean(&renderers::render_powershell_call(args))),
                ),
                name => generic_call(theme, name, args, expanded),
            }
        };
        let width = if self_rendered {
            ctx.width
        } else {
            style::inner_width(ctx.width, ctx.pad)
        };
        let mut content = text_rows(&label, width, 0);
        if self.name == "write" && extension.is_none() {
            let output = args.get("content").and_then(Value::as_str);
            if let Some(output) = output {
                let output = output.trim_end_matches(['\n', '\r']);
                if !output.is_empty() {
                    content.push(String::new());
                    let language = args
                        .get("file_path")
                        .or_else(|| args.get("path"))
                        .and_then(Value::as_str)
                        .and_then(|path| path.rsplit_once('.'))
                        .map(|(_, ext)| ext);
                    let sanitized = clean(output);
                    let output = theme.highlight_code(&sanitized, language).join("\n");
                    append_preview(
                        &mut content,
                        text_rows(&output, width, 0),
                        expanded,
                        10,
                        Keep::Start,
                        ctx,
                        width,
                    );
                }
            } else if !self.pending {
                content.push(String::new());
                content.extend(text_rows(
                    &theme.fg("error", "[invalid content arg - expected string]"),
                    width,
                    0,
                ));
            }
        }
        let mut images = Vec::new();
        if let Some(result) = &self.result {
            let raw = match (extension.is_some(), self.name.as_str(), self.is_error) {
                (_, "edit" | "write", true) => renderers::text_of(result),
                (false, "read", _) => renderers::render_read_result(result),
                (false, "write", _) => renderers::render_write_result(result),
                (false, "edit", _) => renderers::render_edit_result(result),
                (false, "find", _) => renderers::render_find_result(result),
                (false, "grep", _) => renderers::render_grep_result(result),
                (false, "ls", _) => renderers::render_ls_result(result),
                (false, "bash", _) => renderers::render_bash_result(result),
                (false, "powershell", _) => renderers::render_powershell_result(result),
                _ => renderers::text_of(result),
            };
            let mut sanitized = clean(&raw);
            for block in &result.content {
                let InputContent::Image(image) = block else {
                    continue;
                };
                let mime = clean(&image.mime_type);
                if ctx.show_images && ctx.capabilities.images.is_some() && !image.data.is_empty() {
                    images.push(image);
                } else {
                    let fallback =
                        image_fallback(&mime, get_image_dimensions(&image.data, &mime), None);
                    let sanitized = sanitized.to_mut();
                    if !sanitized.is_empty() {
                        sanitized.push('\n');
                    }
                    sanitized.push_str(&fallback);
                }
            }
            if !sanitized.trim().is_empty() {
                content.push(String::new());
                let formatted = if self.name == "edit" && !self.is_error {
                    diff::render(theme, &sanitized)
                } else {
                    theme.fg(
                        if self.is_error { "error" } else { "toolOutput" },
                        &sanitized,
                    )
                };
                let (limit, keep) = if shell {
                    (5, Keep::End)
                } else {
                    (10, Keep::Start)
                };
                append_preview(
                    &mut content,
                    text_rows(&formatted, width, 0),
                    expanded || self.name == "edit",
                    limit,
                    keep,
                    ctx,
                    width,
                );
            } else if self.is_error {
                content.extend(text_rows(&theme.fg("error", "Tool failed"), width, 0));
            }
        }
        if shell && let Some(ms) = self.duration_ms {
            content.extend(text_rows(
                &theme.fg("muted", &format!("Took {}", duration(ms))),
                width,
                0,
            ));
        }
        let mut rows = vec![String::new()];
        if self_rendered {
            rows.extend(content);
        } else {
            let bg = if self.pending {
                "toolPendingBg"
            } else if self.is_error {
                "toolErrorBg"
            } else {
                "toolSuccessBg"
            };
            style::framed(&mut rows, content, ctx.width, ctx.pad, |line| {
                theme.bg(bg, line)
            });
        }
        if !images.is_empty() {
            static REGISTER: Once = Once::new();
            REGISTER.call_once(|| {
                set_image_transcoder(Some(Arc::new(|data, mime| {
                    convert_to_png(data, mime)
                        .ok()
                        .flatten()
                        .map(|png| png.data)
                })))
            });
            self.image_ids.resize_with(images.len(), allocate_image_id);
            for (image, &id) in images.into_iter().zip(&self.image_ids) {
                let theme = theme.clone();
                let component = Image::new(
                    Arc::<str>::from(image.data.as_str()),
                    clean(&image.mime_type).into_owned(),
                    ImageTheme {
                        fallback_color: Rc::new(move |text| theme.fg("toolOutput", text)),
                    },
                    ImageOptions {
                        max_width_cells: Some(ctx.image_width),
                        image_id: Some(id),
                        ..ImageOptions::default()
                    },
                    None,
                );
                rows.push(String::new());
                rows.extend(component.render(ctx.width));
            }
        }
        rows
    }
}

fn styled_call(theme: &Theme, call: &str) -> String {
    match call.split_once(' ') {
        Some((name, tail)) => format!(
            "{} {}",
            theme.fg("toolTitle", &bold(name)),
            theme.fg("accent", &clean(tail))
        ),
        None => theme.fg("toolTitle", &bold(call)),
    }
}

fn generic_call(theme: &Theme, name: &str, args: &Value, expanded: bool) -> String {
    let mut header = theme.fg("toolTitle", &bold(&clean(name)));
    let Some(args) = args.as_object().filter(|args| !args.is_empty()) else {
        return header;
    };
    let mut body = String::new();
    for (key, value) in args {
        let key = clean(key);
        if expanded {
            let value = value.as_str().map_or_else(
                || serde_json::to_string_pretty(value).unwrap_or_default(),
                str::to_owned,
            );
            let value = clean(&value);
            let mut lines = value.split('\n');
            let _ = write!(body, "\n  {key}: {}", lines.next().unwrap_or_default());
            for line in lines {
                let _ = write!(body, "\n    {line}");
            }
        } else {
            let _ = write!(body, " {key}={value}");
            if body.len() >= 100 {
                break;
            }
        }
    }
    if !expanded {
        let truncated = truncate_with_ellipsis(&body, 100, "...", false);
        header.push_str(&theme.fg("muted", &truncated));
    } else {
        header.push_str(&theme.fg("muted", &body));
    }
    header
}

fn append_preview(
    rows: &mut Vec<String>,
    output: Vec<String>,
    expanded: bool,
    limit: usize,
    keep: Keep,
    ctx: &RenderContext<'_>,
    width: usize,
) {
    if expanded {
        rows.extend(output);
        return;
    }
    let (output, hidden) = style::truncate_visual(output, limit, keep);
    let hint = (hidden != 0).then(|| {
        let hint = ctx.theme.fg(
            "muted",
            &format!("... ({hidden} more lines, {} to expand)", ctx.expand_key),
        );
        truncate_with_ellipsis(&hint, width, "...", false)
    });
    match keep {
        Keep::Start => {
            rows.extend(output);
            rows.extend(hint);
        }
        Keep::End => {
            rows.extend(hint);
            rows.extend(output);
        }
    }
}

fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

pub(super) struct Bash {
    command: String,
    output: String,
    status: BashStatus,
    excluded: bool,
    truncated: bool,
    full_output_path: Option<String>,
    trailing_cr: bool,
}

enum BashStatus {
    Running,
    Complete(Option<i32>),
    Cancelled,
    Failed(String),
}

impl Bash {
    pub(super) fn new(command: String, excluded: bool) -> Self {
        Self {
            command,
            output: String::new(),
            status: BashStatus::Running,
            excluded,
            truncated: false,
            full_output_path: None,
            trailing_cr: false,
        }
    }

    pub(super) fn restore(message: BashExecutionMessage) -> Self {
        Self {
            command: message.command,
            output: normalized_shell_output(&message.output),
            status: if message.cancelled {
                BashStatus::Cancelled
            } else {
                BashStatus::Complete(message.exit_code)
            },
            excluded: message.exclude_from_context.unwrap_or_default(),
            truncated: message.truncated,
            full_output_path: message.full_output_path,
            trailing_cr: false,
        }
    }

    pub(super) fn append(&mut self, chunk: &str) {
        let chunk = if self.trailing_cr {
            chunk.strip_prefix('\n').unwrap_or(chunk)
        } else {
            chunk
        };
        self.trailing_cr = chunk.ends_with('\r');
        self.output.push_str(&normalized_shell_output(chunk));
        if self.output.len() > MAX_OUTPUT_BYTES {
            let start = self
                .output
                .ceil_char_boundary(self.output.len() - MAX_OUTPUT_BYTES);
            self.output.drain(..start);
            self.truncated = true;
        }
    }

    pub(super) fn finish(&mut self, result: Result<&BashOutput, &str>) {
        match result {
            Ok(result) => {
                self.output = normalized_shell_output(&result.output);
                self.status = if result.cancelled {
                    BashStatus::Cancelled
                } else {
                    BashStatus::Complete(result.exit_code)
                };
                self.truncated = result.truncated;
                self.full_output_path.clone_from(&result.full_output_path);
            }
            Err(error) => self.status = BashStatus::Failed(clean(error).into_owned()),
        }
    }

    pub(super) fn render(&self, ctx: &RenderContext<'_>, expanded: bool) -> Vec<String> {
        let theme = ctx.theme;
        let color = if self.excluded { "dim" } else { "bashMode" };
        let border = style::border(theme, color, ctx.width);
        let mut rows = vec![String::new(), border.clone()];
        let header = theme.fg(color, &bold(&format!("$ {}", clean(&self.command))));
        rows.extend(text_rows(&header, ctx.width, ctx.pad));
        let report = truncate_tail(&self.output, &TruncationOptions::default());
        if !report.content.is_empty() {
            rows.push(String::new());
            let output = theme.fg("muted", &report.content);
            let mut display = text_rows(&output, ctx.width, ctx.pad);
            if !expanded {
                let hidden;
                (display, hidden) = style::truncate_visual(display, 20, Keep::End);
                if hidden != 0 {
                    let hint = format!("... {hidden} more lines ({} to expand)", ctx.expand_key);
                    rows.extend(text_rows(&theme.fg("muted", &hint), ctx.width, ctx.pad));
                }
            }
            rows.append(&mut display);
        }
        let status = match &self.status {
            BashStatus::Running => Some(theme.fg("muted", "Running... (esc to cancel)")),
            BashStatus::Cancelled => Some(theme.fg("warning", "(cancelled)")),
            BashStatus::Complete(Some(code)) if *code != 0 => {
                Some(theme.fg("error", &format!("(exit {code})")))
            }
            BashStatus::Failed(error) => Some(theme.fg("error", error)),
            BashStatus::Complete(_) => None,
        };
        if let Some(status) = status {
            rows.extend(text_rows(&status, ctx.width, ctx.pad));
        }
        if (self.truncated || report.truncated)
            && let Some(path) = &self.full_output_path
        {
            let message = format!("Output truncated. Full output: {}", clean(path));
            rows.extend(text_rows(
                &theme.fg("warning", &message),
                ctx.width,
                ctx.pad,
            ));
        }
        rows.push(border);
        rows
    }
}

fn normalized_shell_output(output: &str) -> String {
    if output.contains('\r') {
        sanitize_binary_output(&clean(&output.replace("\r\n", "\n").replace('\r', "\n")))
    } else {
        sanitize_binary_output(&clean(output))
    }
}
