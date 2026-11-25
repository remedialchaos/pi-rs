#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptAnchor {
    pub key: String,
    pub entry_id: Option<u64>,
    pub offset: i64,
}

use crate::render_context::safe_text;
use crate::theme::ThemeRef;
use pi_rs_tui::{Component, Markdown};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashMap};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct EntryRecord {
    pub id: u64,
    pub kind: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolView {
    pub id: String,
    pub args: crate::tools::ToolArgs,
    pub details: Option<serde_json::Value>,
    pub name: String,
    pub status: crate::tools::ToolStatus,
    #[serde(default)]
    pub output: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: crate::tools::ToolArgs,
    },
    Thinking {
        thinking: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    User {
        key: String,
        #[serde(rename = "entryId")]
        entry_id: Option<u64>,
        text: String,
    },
    Assistant {
        key: String,
        #[serde(rename = "entryId")]
        entry_id: Option<u64>,
        message: AssistantMessage,
        #[serde(default)]
        tools: Vec<ToolView>,
        #[serde(default)]
        streaming: bool,
    },
    Entry {
        key: String,
        entry: EntryRecord,
    },
    Failure {
        key: String,
        text: String,
    },
    Marker {
        key: String,
        label: String,
        detail: Option<String>,
    },
    Tool {
        key: String,
        tool: ToolView,
    },
    Gap {
        key: String,
        loading: bool,
    },
}
impl Item {
    fn identity(&self) -> (&str, Option<u64>) {
        let (key, entry_id) = match self {
            Self::User { key, entry_id, .. } | Self::Assistant { key, entry_id, .. } => {
                (key, *entry_id)
            }
            Self::Entry { key, entry } => (key, Some(entry.id)),
            Self::Failure { key, .. }
            | Self::Marker { key, .. }
            | Self::Tool { key, .. }
            | Self::Gap { key, .. } => (key, None),
        };
        (
            key,
            entry_id.or_else(|| key.split('-').next()?.parse().ok()),
        )
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
}

pub struct TranscriptView {
    pub theme: ThemeRef,
    pub expanded: bool,
    pub show_thinking: bool,
    pub transcript: Transcript,
    pub tool_renderers: indexmap::IndexMap<String, crate::tools::ToolRenderers>,
    pub entry_renderers: indexmap::IndexMap<String, super::TuiEntryRenderer>,
    pub(crate) rows: Vec<RefCell<Row>>,
}
#[derive(Default)]
pub(crate) struct Row {
    entry: Option<CachedEntry>,
    cards: HashMap<String, (ToolView, super::ToolExecutionComponent)>,
}
struct CachedEntry {
    record: EntryRecord,
    expanded: bool,
    view: Option<Box<dyn Component>>,
}
impl TranscriptView {
    /// Discard rendered components after changing the registered renderers.
    pub fn reset(&mut self) {
        self.rows.clear();
        self.transcript.items.clear();
    }
    pub fn sync(&mut self, transcript: &Transcript) {
        let mut old: HashMap<_, _> = self
            .transcript
            .items
            .drain(..)
            .zip(self.rows.drain(..))
            .map(|(item, row)| (item.identity().0.to_owned(), (item, row)))
            .collect();
        for item in &transcript.items {
            let mut row = Row::default();
            if let Some((previous, cached)) = old.remove(item.identity().0)
                && std::mem::discriminant(&previous) == std::mem::discriminant(item)
            {
                row = cached.into_inner();
                if let Item::Assistant {
                    tools,
                    message,
                    streaming,
                    ..
                } = item
                {
                    row.cards.retain(|id, _| {
                        if *streaming {
                            message
                                .content
                                .iter()
                                .any(|b| matches!(b,ContentBlock::ToolCall{id:call,..} if call==id))
                        } else {
                            tools.iter().any(|t| &t.id == id)
                        }
                    });
                }
            }
            self.rows.push(RefCell::new(row));
            self.transcript.items.push(item.clone());
        }
    }
    fn tool_lines(&self, row: &mut Row, tool: &ToolView, width: usize) -> Vec<String> {
        use crate::tools::{ToolResultContent, ToolResultView};
        if let Some((previous, card)) = row.cards.get_mut(&tool.id)
            && previous == tool
        {
            card.set_expanded(self.expanded);
            return card.render(width);
        }
        let result = (!tool.output.is_empty() || tool.details.is_some()).then(|| ToolResultView {
            content: vec![ToolResultContent::Text(safe_text(&tool.output).into())],
            details: tool.details.clone(),
        });
        let data = crate::tools::ToolView {
            id: tool.id.clone(),
            name: tool.name.clone(),
            args: tool.args.clone(),
            status: tool.status.clone(),
            result,
            calls: None,
        };
        let ctx = super::ToolCardContext {
            cwd: String::new(),
            output_pad: None,
            theme: self.theme.clone(),
            key_text: None,
            request_render: None,
        };
        let complete = !matches!(tool.status, crate::tools::ToolStatus::Pending);
        let (old, card) = row.cards.entry(tool.id.clone()).or_insert_with(|| {
            (
                tool.clone(),
                super::ToolExecutionComponent::new(
                    data.clone(),
                    self.tool_renderers.get(&tool.name).cloned(),
                    ctx.clone(),
                    complete,
                ),
            )
        });
        if old.name != tool.name {
            *card = super::ToolExecutionComponent::new(
                data,
                self.tool_renderers.get(&tool.name).cloned(),
                ctx,
                complete,
            );
        } else if old != tool {
            card.update(data, complete);
        }
        old.clone_from(tool);
        card.set_expanded(self.expanded);
        card.render(width)
    }
    pub fn anchor_at(&self, width: usize, top: i64) -> Option<TranscriptAnchor> {
        let mut start = 0;
        let mut anchor = None;
        for (index, item) in self.transcript.items.iter().enumerate() {
            if start > top && anchor.is_some() {
                break;
            }
            let (key, entry_id) = item.identity();
            anchor = Some(TranscriptAnchor {
                key: key.into(),
                entry_id,
                offset: top - start,
            });
            start += self.render_item(index, item, width).len() as i64;
        }
        anchor
    }
    pub fn restore_anchor(&self, width: usize, anchor: &TranscriptAnchor) -> Option<i64> {
        let mut start = 0;
        for (index, item) in self.transcript.items.iter().enumerate() {
            if item.identity().0 == anchor.key {
                return Some(start + anchor.offset);
            }
            start += self.render_item(index, item, width).len() as i64;
        }
        None
    }
    fn render_item(&self, index: usize, item: &Item, width: usize) -> Vec<String> {
        let fallback = RefCell::default();
        let mut row = self.rows.get(index).unwrap_or(&fallback).borrow_mut();
        let mut lines = Vec::new();
        let theme = self.theme.current();
        match item {
            Item::User { text, .. } => lines.extend(pi_rs_tui::wrap_text_with_ansi(
                &theme.fg("userMessageText", &safe_text(text)),
                width,
            )),
            Item::Assistant {
                message,
                tools,
                streaming,
                ..
            } => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let context = super::mermaid::MermaidTransformContext {
                                settings: None,
                                message_type: "assistant".into(),
                                is_streaming: *streaming,
                                available_width: width,
                                theme: theme.clone(),
                            };
                            let markdown = super::mermaid::mermaid_markdown_transformer(
                                &safe_text(text),
                                context,
                            );
                            lines.extend(
                                Markdown::new(
                                    markdown,
                                    0,
                                    0,
                                    self.theme.markdown(),
                                    Default::default(),
                                    Default::default(),
                                )
                                .render(width),
                            );
                        }
                        ContentBlock::Thinking { thinking } if self.show_thinking => {
                            lines.extend(pi_rs_tui::wrap_text_with_ansi(
                                &theme.fg("thinkingText", &safe_text(thinking)),
                                width,
                            ))
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } if *streaming => lines.extend(self.tool_lines(
                            &mut row,
                            &ToolView {
                                id: id.clone(),
                                name: name.clone(),
                                args: arguments.clone(),
                                status: crate::tools::ToolStatus::Pending,
                                output: String::new(),
                                details: None,
                            },
                            width,
                        )),
                        _ => {}
                    }
                }
                for tool in tools.iter().filter(|_| !streaming) {
                    lines.extend(self.tool_lines(&mut row, tool, width));
                }
            }
            Item::Entry { entry, .. } => {
                if row.entry.as_ref().is_none_or(|cached| {
                    cached.expanded != self.expanded || cached.record != *entry
                }) {
                    let ctx: Box<dyn crate::render_context::RenderContext> =
                        Box::new(super::ToolCardContext {
                            cwd: String::new(),
                            output_pad: None,
                            theme: self.theme.clone(),
                            key_text: None,
                            request_render: None,
                        });
                    let view = if let Some(renderer) = self.entry_renderers.get(&entry.kind) {
                        renderer(entry, ctx, self.expanded)
                    } else if entry.kind == "p2.shell" {
                        super::shell_entry_renderer(entry, ctx, self.expanded)
                    } else {
                        None
                    };
                    row.entry = Some(CachedEntry {
                        record: entry.clone(),
                        expanded: self.expanded,
                        view,
                    });
                }
                if let Some(view) = row.entry.as_ref().and_then(|cached| cached.view.as_ref()) {
                    lines.extend(view.render(width));
                }
            }
            Item::Failure { text, .. } => lines.push(theme.fg("error", &safe_text(text))),
            Item::Marker { label, detail, .. } => {
                lines.push(theme.fg("muted", &safe_text(label)));
                if let Some(text) = detail {
                    lines.push(safe_text(text));
                }
            }
            Item::Tool { tool, .. } => lines.extend(self.tool_lines(&mut row, tool, width)),
            Item::Gap { loading, .. } => lines.push(
                if *loading {
                    "Loading…"
                } else {
                    "Load missing entries"
                }
                .into(),
            ),
        }
        lines
    }
}
impl Component for TranscriptView {
    fn render(&self, width: usize) -> Vec<String> {
        self.transcript
            .items
            .iter()
            .enumerate()
            .flat_map(|(index, item)| self.render_item(index, item, width))
            .chain(std::iter::once(String::new()))
            .collect()
    }
    fn invalidate(&mut self) {
        for row in &mut self.rows {
            let row = row.get_mut();
            if let Some(entry) = row.entry.as_mut().and_then(|cached| cached.view.as_mut()) {
                entry.invalidate();
            }
            for (_, card) in row.cards.values_mut() {
                card.invalidate();
            }
        }
    }
}
