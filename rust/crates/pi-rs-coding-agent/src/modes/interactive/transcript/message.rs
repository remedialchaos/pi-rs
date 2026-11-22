use std::ops::Range;
use std::rc::Rc;

use pi_rs_ai::{AssistantContent, AssistantMessage, PartialMessage, StopReason};
use pi_rs_tui::{Component, DefaultTextStyle, Markdown, MarkdownOptions};

use super::{
    RenderContext, Theme, mermaid,
    style::{self, bold, clean, text_rows},
};

#[derive(Clone, Default)]
pub(super) struct Metadata {
    pub(super) id: Option<String>,
    pub(super) timestamp: Option<String>,
}

pub(super) enum AssistantSource {
    Starting(std::sync::Arc<AssistantMessage>),
    Streaming(PartialMessage),
    Settled(std::sync::Arc<AssistantMessage>),
}

pub(super) struct Assistant {
    pub(super) source: AssistantSource,
    pub(super) visibility: Vec<Option<bool>>,
}

impl Assistant {
    pub(super) fn render(
        &self,
        ctx: &RenderContext<'_>,
        meta: &Metadata,
    ) -> (Vec<String>, Vec<(Range<usize>, usize)>) {
        match &self.source {
            AssistantSource::Starting(message) => self.render_message(message, true, ctx, meta),
            AssistantSource::Streaming(message) => {
                let message = message.read().unwrap_or_else(|error| error.into_inner());
                self.render_message(&message, true, ctx, meta)
            }
            AssistantSource::Settled(message) => self.render_message(message, false, ctx, meta),
        }
    }

    fn render_message(
        &self,
        message: &AssistantMessage,
        streaming: bool,
        ctx: &RenderContext<'_>,
        meta: &Metadata,
    ) -> (Vec<String>, Vec<(Range<usize>, usize)>) {
        let mut rows = Vec::new();
        let mut hits = Vec::new();
        let mut needs_space = true;
        let mut run_index = 0;
        let mut blocks = message.content.iter().peekable();
        let mut tool_calls = false;
        while let Some(block) = blocks.next() {
            match block {
                AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                    if needs_space {
                        rows.push(String::new());
                    }
                    rows.extend(markdown(
                        ctx,
                        meta,
                        text.text.trim(),
                        MarkdownSurface::Assistant(streaming),
                    ));
                    needs_space = false;
                }
                AssistantContent::Thinking(first) => {
                    let mut thinking = String::new();
                    let mut push = |text: &str| {
                        let text = text.trim();
                        if !text.is_empty() {
                            if !thinking.is_empty() {
                                thinking.push_str("\n\n");
                            }
                            thinking.push_str(text);
                        }
                    };
                    push(&first.thinking);
                    while let Some(AssistantContent::Thinking(next)) = blocks.peek() {
                        push(&next.thinking);
                        blocks.next();
                    }
                    if thinking.is_empty() {
                        continue;
                    }
                    if needs_space {
                        rows.push(String::new());
                    }
                    let visible = self
                        .visibility
                        .get(run_index)
                        .copied()
                        .flatten()
                        .unwrap_or(ctx.show_thinking);
                    let start = rows.len();
                    if visible {
                        rows.extend(markdown(
                            ctx,
                            meta,
                            &thinking,
                            MarkdownSurface::Thinking(streaming),
                        ));
                    } else {
                        let label = ctx.theme.fg("thinkingText", "\x1b[3mThinking...\x1b[23m");
                        rows.extend(text_rows(&label, ctx.width, ctx.pad));
                    }
                    hits.push((start..rows.len(), run_index));
                    run_index += 1;
                    needs_space = true;
                }
                AssistantContent::ToolCall(_) => tool_calls = true,
                _ => {}
            }
        }
        if !streaming {
            let error = match message.stop_reason {
                StopReason::Length => Some("Response was truncated before completion.".into()),
                StopReason::Aborted if !tool_calls => Some(abort_message(message).into()),
                StopReason::Error if !tool_calls => Some(format!(
                    "Error: {}",
                    message.error_message.as_deref().unwrap_or("Unknown error")
                )),
                _ => None,
            };
            if let Some(error) = error {
                rows.push(String::new());
                rows.extend(text_rows(
                    &ctx.theme.fg("error", &clean(&error)),
                    ctx.width,
                    ctx.pad,
                ));
            }
        }
        if !tool_calls {
            style::zone(&mut rows);
        }
        (rows, hits)
    }
}

pub(super) fn abort_message(message: &AssistantMessage) -> &str {
    message
        .error_message
        .as_deref()
        .filter(|error| *error != "Request was aborted")
        .unwrap_or("Operation aborted")
}

#[derive(Clone, Copy)]
pub(super) enum MarkdownSurface {
    User,
    Assistant(bool),
    Thinking(bool),
    Custom,
    Summary,
}

pub(super) fn markdown(
    ctx: &RenderContext<'_>,
    meta: &Metadata,
    text: &str,
    surface: MarkdownSurface,
) -> Vec<String> {
    let (kind, streaming, color) = match surface {
        MarkdownSurface::User => ("user", false, Some("userMessageText")),
        MarkdownSurface::Assistant(streaming) => ("assistant", streaming, None),
        MarkdownSurface::Thinking(streaming) => {
            ("assistant-thinking", streaming, Some("thinkingText"))
        }
        MarkdownSurface::Custom => ("custom", false, Some("customMessageText")),
        MarkdownSurface::Summary => ("summary", false, Some("customMessageText")),
    };
    let user = matches!(surface, MarkdownSurface::User);
    let boxed = matches!(surface, MarkdownSurface::Summary | MarkdownSurface::Custom);
    let width = if boxed {
        style::inner_width(ctx.width, ctx.pad)
    } else {
        ctx.width
    };
    let pad = if boxed {
        0
    } else {
        ctx.pad.min(width.saturating_sub(1) / 2)
    };
    let mut theme = ctx.theme.markdown();
    theme.code_block_indent.clone_from(ctx.code_block_indent);
    let theme_fg = ctx.theme.clone();
    let theme_bg = ctx.theme.clone();
    let style = DefaultTextStyle {
        color: color.map(|color| Rc::new(move |text: &str| theme_fg.fg(color, text)) as _),
        bg_color: user.then(|| Rc::new(move |text: &str| theme_bg.bg("userMessageBg", text)) as _),
        italic: matches!(surface, MarkdownSurface::Thinking(_)),
        ..DefaultTextStyle::default()
    };
    let available = style::inner_width(width, pad);
    let text = clean(text);
    let mut text = if matches!(
        surface,
        MarkdownSurface::User | MarkdownSurface::Assistant(_) | MarkdownSurface::Summary
    ) {
        mermaid::transform(ctx.theme, &text, available, ctx.mermaid, streaming)
    } else {
        text
    };
    if ctx
        .transformers
        .iter()
        .any(|transformer| applies(&transformer.message_types, kind))
    {
        let context = serde_json::json!({
            "messageType": kind,
            "isStreaming": streaming,
            "availableWidth": available,
            "messageId": meta.id,
            "timestamp": meta.timestamp,
        });
        let mut transformed = text.into_owned();
        for transformer in ctx.transformers {
            if applies(&transformer.message_types, kind) {
                transformed = (transformer.transform)(transformed, context.clone());
            }
        }
        text = transformed.into();
    }
    let options = MarkdownOptions {
        preserve_ordered_list_markers: user,
        preserve_backslash_escapes: user,
        ..MarkdownOptions::default()
    };
    let mut rows = Markdown::new(text, pad, usize::from(user), theme, style, options).render(width);
    if user {
        style::zone(&mut rows);
    }
    rows
}

fn applies(kinds: &[String], kind: &str) -> bool {
    kinds.is_empty() || kinds.iter().any(|candidate| candidate == kind)
}

pub(super) enum Card {
    Custom { label: String, text: String },
    Branch(String),
    Compaction { tokens: u64, text: String },
    Skill { name: String, text: String },
}

impl Card {
    pub(super) fn is_expandable(&self) -> bool {
        !matches!(self, Self::Custom { .. })
    }

    pub(super) fn render(
        &self,
        ctx: &RenderContext<'_>,
        meta: &Metadata,
        expanded: bool,
    ) -> Vec<String> {
        let theme = ctx.theme;
        let width = style::inner_width(ctx.width, ctx.pad);
        let (label, title, text, surface) = match self {
            Self::Custom { label, text } => (
                label.as_str(),
                String::new(),
                text.as_str(),
                MarkdownSurface::Custom,
            ),
            Self::Branch(text) => (
                "branch",
                if expanded {
                    "Branch Summary"
                } else {
                    "Branch summary"
                }
                .into(),
                text.as_str(),
                MarkdownSurface::Summary,
            ),
            Self::Compaction { tokens, text } => (
                "compaction",
                format!("Compacted from {} tokens", grouped(*tokens)),
                text.as_str(),
                MarkdownSurface::Summary,
            ),
            Self::Skill { name, text } => (
                "skill",
                clean(name).into_owned(),
                text.as_str(),
                MarkdownSurface::Custom,
            ),
        };
        let label = theme.fg("customMessageLabel", &bold(&format!("[{}]", clean(label))));
        let hint = || theme.fg("dim", &format!(" ({} to expand)", ctx.expand_key));
        let content = if !expanded && matches!(self, Self::Skill { .. }) {
            let title = theme.fg("customMessageText", &title);
            text_rows(&format!("{label} {title}{}", hint()), width, 0)
        } else {
            let mut content = text_rows(&label, width, 0);
            if !matches!(self, Self::Skill { .. }) {
                content.push(String::new());
            }
            if expanded || !self.is_expandable() {
                if !title.is_empty() {
                    let title = theme.fg("customMessageText", &bold(&title));
                    content.extend(text_rows(&title, width, 0));
                    content.push(String::new());
                }
                content.extend(markdown(ctx, meta, text, surface));
            } else {
                let title = theme.fg("customMessageText", &title);
                content.extend(text_rows(&format!("{title}{}", hint()), width, 0));
            }
            content
        };
        let mut rows = vec![String::new()];
        style::framed(&mut rows, content, ctx.width, ctx.pad, |line| {
            theme.bg("customMessageBg", line)
        });
        rows
    }
}

fn grouped(value: u64) -> String {
    let value = value.to_string();
    let mut result = String::with_capacity(value.len() + value.len() / 3);
    for (index, digit) in value.bytes().enumerate() {
        if index != 0 && (value.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(char::from(digit));
    }
    result
}

pub(super) fn notice(
    theme: &Theme,
    text: &str,
    color: &str,
    width: usize,
    pad: usize,
) -> Vec<String> {
    let mut rows = text_rows(&theme.fg(color, &clean(text)), width, pad);
    if !rows.is_empty() {
        rows.insert(0, String::new());
    }
    rows
}
