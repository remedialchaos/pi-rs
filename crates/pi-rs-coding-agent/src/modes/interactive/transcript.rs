//! Session transcript state and width-dependent presentation. The renderer owns viewport and input routing.

mod diff;
mod execution;
mod mermaid;
mod message;
mod style;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use pi_rs_agent::{AgentEvent, AgentMessage, AgentToolResult};
use pi_rs_ai::{
    AssistantContent, AssistantMessage, PartialMessage, StopReason, StringOrBlocks,
    ToolResultMessage,
};
use pi_rs_tui::{
    CellDimensions, TerminalCapabilities, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult,
    TuiMouseEventType, get_capabilities, get_cell_dimensions,
};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::core::{
    agent_session::AgentSessionEvent,
    bash_executor::BashOutput,
    export_html::parse_skill_block,
    extensions::{runner::ExtensionRunner, types::MarkdownTransformerDefinition},
    messages::BashExecutionMessage,
    session_manager::SessionEntry,
};

use super::theme::Theme;
use execution::{Bash, Tool};
use mermaid::MermaidMode;
use message::{Assistant, AssistantSource, Card, MarkdownSurface, Metadata};

pub(super) struct Transcript {
    theme: Theme,
    rows: Vec<Row>,
    active_assistant: Option<usize>,
    tools: HashMap<String, usize>,
    pending_tools: HashSet<usize>,
    live_bash: HashMap<u64, usize>,
    next_bash_id: u64,
    seen_entries: HashSet<String>,
    last_assistant_text: Option<String>,
    options: Options,
    default_expanded: bool,
    extensions: Option<Arc<ExtensionRunner>>,
    transformers: Vec<MarkdownTransformerDefinition>,
}

struct Row {
    body: Body,
    metadata: Metadata,
    expanded: Option<bool>,
    cache: Option<CachedRow>,
}

struct UserMessageRow {
    text: String,
    images: Vec<pi_rs_ai::ImageContent>,
    image_ids: Vec<u32>,
}

enum Body {
    User(UserMessageRow),
    Assistant(Assistant),
    Card(Card),
    Tool(Tool),
    Bash(Bash),
    Notice(String, &'static str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    width: usize,
    theme: u64,
    terminal: TerminalCapabilities,
    cell: CellDimensions,
}

struct CachedRow {
    key: RenderKey,
    lines: Vec<String>,
    thinking: Vec<(Range<usize>, usize)>,
}

#[derive(PartialEq, Eq)]
struct Options {
    pad: usize,
    show_thinking: bool,
    show_images: bool,
    image_width: usize,
    image_height: Option<usize>,
    code_block_indent: Option<String>,
    mermaid: MermaidMode,
}

impl Options {
    fn from_settings(settings: &Value) -> Self {
        Self {
            pad: usize::from(settings["outputPad"].as_i64() != Some(0)),
            show_thinking: !settings["hideThinkingBlock"].as_bool().unwrap_or(false),
            show_images: settings["terminal"]["showImages"].as_bool().unwrap_or(true),
            image_width: settings["terminal"]["imageWidthCells"]
                .as_u64()
                .and_then(|width| usize::try_from(width).ok())
                .unwrap_or(60)
                .max(1),
            image_height: settings["terminal"]["imageHeightCells"]
                .as_u64()
                .and_then(|height| usize::try_from(height).ok())
                .map(|height| height.max(1)),
            code_block_indent: Some(
                settings["markdown"]["codeBlockIndent"]
                    .as_str()
                    .unwrap_or("  ")
                    .to_owned(),
            ),
            mermaid: match settings["markdown"]["mermaid"].as_str() {
                Some("off") => MermaidMode::Off,
                Some("final") => MermaidMode::Final,
                _ => MermaidMode::Streaming,
            },
        }
    }
}

struct RenderContext<'a> {
    theme: &'a Theme,
    width: usize,
    capabilities: TerminalCapabilities,
    pad: usize,
    show_thinking: bool,
    show_images: bool,
    image_width: usize,
    image_height: Option<usize>,
    code_block_indent: &'a Option<String>,
    mermaid: MermaidMode,
    expand_key: &'a str,
    extensions: Option<&'a ExtensionRunner>,
    transformers: &'a [MarkdownTransformerDefinition],
}

impl Transcript {
    pub(super) fn new(theme: Theme) -> Self {
        Self {
            theme,
            rows: Vec::new(),
            active_assistant: None,
            tools: HashMap::new(),
            pending_tools: HashSet::new(),
            live_bash: HashMap::new(),
            next_bash_id: 0,
            seen_entries: HashSet::new(),
            last_assistant_text: None,
            options: Options::from_settings(&Value::Null),
            default_expanded: false,
            extensions: None,
            transformers: Vec::new(),
        }
    }

    /// Apply the current persisted settings without resetting local expansion state.
    pub(super) fn configure(&mut self, settings: &Value) {
        let options = Options::from_settings(settings);
        if options == self.options {
            return;
        }
        if options.show_thinking != self.options.show_thinking {
            self.set_show_thinking(options.show_thinking);
        }
        self.set_show_images(options.show_images);
        self.options = options;
        self.invalidate();
    }

    pub(super) fn set_extensions(&mut self, runner: Option<Arc<ExtensionRunner>>) {
        self.transformers = runner
            .as_ref()
            .map_or_else(Vec::new, |runner| runner.markdown_transformers());
        self.extensions = runner;
        self.invalidate();
    }

    /// Replace the transcript with entries from the active, projected session branch.
    pub(super) fn restore(&mut self, entries: &[SessionEntry]) {
        self.clear();
        self.rows.reserve(entries.len());
        for entry in entries {
            self.entry(entry);
        }
    }

    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.pending_tools.clear();
        self.tools.clear();
        self.live_bash.clear();
        self.seen_entries.clear();
        self.active_assistant = None;
        self.last_assistant_text = None;
    }

    pub(super) fn notice(&mut self, text: String, color: &'static str) {
        if !text.trim().is_empty() {
            self.push(Body::Notice(text, color), Metadata::default());
        }
    }

    pub(super) fn last_assistant_text(&self) -> Option<&str> {
        self.last_assistant_text.as_deref()
    }

    pub(super) fn toggle_tools(&mut self) -> bool {
        self.default_expanded = !self.default_expanded;
        for row in &mut self.rows {
            if row.can_expand() {
                row.expanded = None;
                row.cache = None;
            }
        }
        self.default_expanded
    }

    pub(super) fn toggle_thinking(&mut self) -> bool {
        self.set_show_thinking(!self.options.show_thinking);
        self.options.show_thinking
    }

    fn set_show_thinking(&mut self, show: bool) {
        self.options.show_thinking = show;
        for row in &mut self.rows {
            if let Body::Assistant(assistant) = &mut row.body {
                assistant.visibility.clear();
                row.cache = None;
            }
        }
    }

    pub(super) fn set_show_images(&mut self, show: bool) {
        if self.options.show_images != show {
            self.options.show_images = show;
            for row in &mut self.rows {
                if matches!(row.body, Body::Tool(_)) {
                    row.cache = None;
                }
            }
        }
    }

    pub(super) fn invalidate(&mut self) {
        for row in &mut self.rows {
            row.cache = None;
        }
    }

    pub(super) fn event(&mut self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::EntryAppended { entry } => self.entry(entry),
            AgentSessionEvent::Core(event) => self.core_event(event),
            AgentSessionEvent::CompactionEnd {
                result: Some(result),
                ..
            } => {
                self.push(
                    Body::Card(Card::Compaction {
                        tokens: result.tokens_before,
                        text: result.summary.clone(),
                    }),
                    Metadata::default(),
                );
            }
            AgentSessionEvent::AgentSettled { aborted } => self.settle_pending(*aborted),
            _ => {}
        }
    }

    pub(super) fn begin_bash(&mut self, command: String, exclude_from_context: bool) -> u64 {
        let id = self.next_bash_id;
        self.next_bash_id = self.next_bash_id.wrapping_add(1);
        let row = self.push(
            Body::Bash(Bash::new(command, exclude_from_context)),
            Metadata::default(),
        );
        self.live_bash.insert(id, row);
        id
    }

    pub(super) fn bash_chunk(&mut self, id: u64, chunk: &str) {
        if let Some(&index) = self.live_bash.get(&id)
            && let Some(Row {
                body: Body::Bash(bash),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            bash.append(chunk);
            *cache = None;
        }
    }

    pub(super) fn end_bash(&mut self, id: u64, result: Result<&BashOutput, &str>) {
        if let Some(index) = self.live_bash.remove(&id)
            && let Some(Row {
                body: Body::Bash(bash),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            bash.finish(result);
            *cache = None;
        }
    }

    pub(super) fn render(&mut self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let key = RenderKey {
            width,
            theme: self.theme.revision(),
            terminal: get_capabilities(),
            cell: get_cell_dimensions(),
        };
        let ctx = RenderContext {
            theme: &self.theme,
            width,
            capabilities: key.terminal,
            pad: self.options.pad,
            show_thinking: self.options.show_thinking,
            show_images: self.options.show_images,
            image_width: self.options.image_width,
            image_height: self.options.image_height,
            code_block_indent: &self.options.code_block_indent,
            mermaid: self.options.mermaid,
            expand_key: "ctrl+o",
            extensions: self.extensions.as_deref(),
            transformers: &self.transformers,
        };
        let mut lines = Vec::new();
        for row in &mut self.rows {
            if !row.cache.as_ref().is_some_and(|cache| cache.key == key) {
                row.refresh(&ctx, key, self.default_expanded);
            }
            if let Some(cache) = &row.cache {
                lines.extend_from_slice(&cache.lines);
            }
        }
        lines
    }

    pub(super) fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if event.event_type != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
            return None;
        }
        let mut y = event.y;
        for row in &mut self.rows {
            let cache = row.cache.as_ref()?;
            if y >= cache.lines.len() {
                y -= cache.lines.len();
                continue;
            }
            if let Body::Assistant(assistant) = &mut row.body {
                let &(_, index) = cache
                    .thinking
                    .iter()
                    .find(|(range, _)| range.contains(&y))?;
                if assistant.visibility.len() <= index {
                    assistant.visibility.resize(index + 1, None);
                }
                let previous = assistant.visibility[index].unwrap_or(self.options.show_thinking);
                assistant.visibility[index] = Some(!previous);
            } else if y != 0 && row.can_click() {
                row.expanded = Some(!row.expanded.unwrap_or(self.default_expanded));
            } else {
                return None;
            }
            row.cache = None;
            return Some(TuiMouseEventResult {
                handled: true,
                render: Some(true),
                ..Default::default()
            });
        }
        None
    }

    fn push(&mut self, body: Body, metadata: Metadata) -> usize {
        let index = self.rows.len();
        self.rows.push(Row {
            body,
            metadata,
            expanded: None,
            cache: None,
        });
        index
    }

    fn entry(&mut self, entry: &SessionEntry) {
        if !self.seen_entries.insert(entry.id().to_owned()) {
            return;
        }
        let metadata = Metadata {
            id: Some(entry.id().to_owned()),
            timestamp: Some(entry.timestamp().to_owned()),
        };
        match entry {
            SessionEntry::Message { message, .. } => {
                if let Ok(message) = AgentMessage::deserialize(message) {
                    self.stored_message(message, metadata);
                }
            }
            SessionEntry::CustomMessage {
                custom_type,
                content,
                display: true,
                ..
            } => self.custom(custom_type, content, metadata),
            SessionEntry::Custom {
                custom_type, data, ..
            } => {
                // The Rust extension API registers entry labels, not component factories.
                // Match upstream by leaving unregistered extension state out of the transcript.
                if self
                    .extensions
                    .as_ref()
                    .and_then(|runner| runner.get_entry_renderer(custom_type))
                    .is_some()
                {
                    let text = data.as_ref().map_or_else(String::new, |data| {
                        data.as_str()
                            .map_or_else(|| format!("```json\n{data:#}\n```"), str::to_owned)
                    });
                    self.push(
                        Body::Card(Card::Custom {
                            label: custom_type.clone(),
                            text,
                        }),
                        metadata,
                    );
                }
            }
            SessionEntry::BranchSummary { summary, .. } => {
                self.push(Body::Card(Card::Branch(summary.clone())), metadata);
            }
            SessionEntry::Compaction {
                summary,
                tokens_before,
                ..
            } => {
                self.push(
                    Body::Card(Card::Compaction {
                        tokens: *tokens_before,
                        text: summary.clone(),
                    }),
                    metadata,
                );
            }
            _ => {}
        }
    }

    fn custom(&mut self, label: &str, content: &Value, metadata: Metadata) {
        let text = match content {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        self.push(
            Body::Card(Card::Custom {
                label: label.to_owned(),
                text,
            }),
            metadata,
        );
    }

    fn stored_message(&mut self, message: AgentMessage, metadata: Metadata) {
        match message {
            AgentMessage::Assistant(message) => self.settled_assistant(message, metadata),
            AgentMessage::ToolResult(message) => self.stored_tool_result(message),
            _ => self.ordinary_message(&message, metadata),
        }
    }

    fn ordinary_message(&mut self, message: &AgentMessage, metadata: Metadata) {
        let role = message.role();
        match message {
            AgentMessage::User(user) => {
                let (text, images) = match &user.content {
                    StringOrBlocks::String(text) => (text.clone(), Vec::new()),
                    StringOrBlocks::Blocks(blocks) => {
                        let text = pi_rs_ai::transcript::input_text(blocks, "");
                        let images = blocks
                            .iter()
                            .filter_map(|block| match block {
                                pi_rs_ai::InputContent::Image(image) => Some(image.clone()),
                                pi_rs_ai::InputContent::Text(_) => None,
                            })
                            .collect::<Vec<_>>();
                        (text, images)
                    }
                };
                if text.trim().is_empty() && images.is_empty() {
                    return;
                }
                if let Some(skill) = parse_skill_block(&text) {
                    self.push(
                        Body::Card(Card::Skill {
                            name: skill.name,
                            text: skill.content,
                        }),
                        metadata.clone(),
                    );
                    if let Some(user_text) = skill.user_message {
                        self.push(
                            Body::User(UserMessageRow {
                                text: user_text,
                                images,
                                image_ids: Vec::new(),
                            }),
                            metadata,
                        );
                    } else if !images.is_empty() {
                        self.push(
                            Body::User(UserMessageRow {
                                text: String::new(),
                                images,
                                image_ids: Vec::new(),
                            }),
                            metadata,
                        );
                    }
                } else {
                    self.push(
                        Body::User(UserMessageRow {
                            text,
                            images,
                            image_ids: Vec::new(),
                        }),
                        metadata,
                    );
                }
            }
            AgentMessage::Custom(message) => match role {
                "custom"
                    if message.fields.get("display").and_then(Value::as_bool) == Some(true) =>
                {
                    let label = message
                        .fields
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or("custom");
                    let content = message.fields.get("content").unwrap_or(&Value::Null);
                    self.custom(label, content, metadata);
                }
                "bashExecution" => {
                    if let Ok(bash) =
                        BashExecutionMessage::deserialize(serde::de::value::MapDeserializer::<
                            _,
                            serde_json::Error,
                        >::new(
                            message
                                .fields
                                .iter()
                                .map(|(key, value)| (key.as_str(), value)),
                        ))
                    {
                        self.push(Body::Bash(Bash::restore(bash)), metadata);
                    }
                }
                "branchSummary" => {
                    if let Some(text) = message.fields.get("summary").and_then(Value::as_str) {
                        self.push(Body::Card(Card::Branch(text.to_owned())), metadata);
                    }
                }
                "compactionSummary" => {
                    if let Some(text) = message.fields.get("summary").and_then(Value::as_str) {
                        let tokens = message
                            .fields
                            .get("tokensBefore")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        self.push(
                            Body::Card(Card::Compaction {
                                tokens,
                                text: text.to_owned(),
                            }),
                            metadata,
                        );
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn core_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(message),
            } => {
                self.live_assistant(AssistantSource::Starting(message.clone()));
                self.calls(message, false);
            }
            AgentEvent::MessageStart { message } => {
                self.ordinary_message(message, Metadata::default())
            }
            AgentEvent::MessageUpdate { message, .. } => self.assistant_update(message),
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
            } => {
                self.settled_assistant(message.clone(), Metadata::default());
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(message),
            } => {
                if self
                    .tools
                    .get(&message.tool_call_id)
                    .and_then(|&index| self.rows.get(index))
                    .is_none_or(|row| matches!(&row.body, Body::Tool(tool) if tool.pending))
                {
                    self.stored_tool_result(message.clone());
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.tool_call(tool_call_id, tool_name, args, None);
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                let index = self.tool_call(tool_call_id, tool_name, args, None);
                if let Some(Row {
                    body: Body::Tool(tool),
                    cache,
                    ..
                }) = self.rows.get_mut(index)
                    && tool.pending
                {
                    tool.result = Some(Arc::new(partial_result.clone()));
                    *cache = None;
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
            } => {
                self.tool_result(
                    tool_call_id,
                    tool_name,
                    result.clone(),
                    *is_error,
                    *duration_ms,
                );
            }
            _ => {}
        }
    }

    fn assistant_update(&mut self, message: &PartialMessage) {
        self.live_assistant(AssistantSource::Streaming(message.clone()));
        let message = message.read().unwrap_or_else(|error| error.into_inner());
        self.calls(&message, false);
    }

    fn live_assistant(&mut self, source: AssistantSource) {
        if let Some(index) = self.active_assistant
            && let Some(Row {
                body: Body::Assistant(assistant),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            assistant.source = source;
            *cache = None;
            return;
        }
        let index = self.push(
            Body::Assistant(Assistant {
                source,
                visibility: Vec::new(),
            }),
            Metadata::default(),
        );
        self.active_assistant = Some(index);
    }

    fn settled_assistant(&mut self, message: Arc<AssistantMessage>, metadata: Metadata) {
        let source = AssistantSource::Settled(message.clone());
        if let Some(index) = self.active_assistant
            && let Some(Row {
                body: Body::Assistant(assistant),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            assistant.source = source;
            *cache = None;
        } else {
            self.active_assistant = Some(self.push(
                Body::Assistant(Assistant {
                    source,
                    visibility: Vec::new(),
                }),
                metadata,
            ));
        }
        self.calls(&message, true);
        self.active_assistant = None;
        let mut text = String::new();
        for block in &message.content {
            if let AssistantContent::Text(block) = block {
                text.push_str(&block.text);
            }
        }
        let trimmed = text.trim();
        if !trimmed.is_empty() || message.stop_reason == StopReason::Stop {
            self.last_assistant_text = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
    }

    fn calls(&mut self, message: &AssistantMessage, settled: bool) {
        for block in &message.content {
            let AssistantContent::ToolCall(call) = block else {
                continue;
            };
            if call.id.is_empty() || call.name.is_empty() {
                continue;
            }
            let index =
                self.tool_call(&call.id, &call.name, &call.arguments, self.active_assistant);
            if !settled {
                continue;
            }
            let failure = match message.stop_reason {
                StopReason::Aborted => Some(message::abort_message(message)),
                StopReason::Error => Some(message.error_message.as_deref().unwrap_or("Error")),
                StopReason::Length => Some("Response was truncated before completion."),
                _ => None,
            };
            if let Some(error) = failure
                && let Some(Row {
                    body: Body::Tool(tool),
                    cache,
                    ..
                }) = self.rows.get_mut(index)
                && tool.pending
            {
                tool.result = Some(Arc::new(AgentToolResult::text(error)));
                tool.is_error = true;
                tool.pending = false;
                self.pending_tools.remove(&index);
                *cache = None;
            }
        }
    }

    fn tool_call(
        &mut self,
        id: &str,
        name: &str,
        args: &Map<String, Value>,
        after: Option<usize>,
    ) -> usize {
        if let Some(&index) = self.tools.get(id)
            && after.is_none_or(|after| index > after)
            && let Some(Row {
                body: Body::Tool(tool),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            if tool.pending && tool.args.as_object() != Some(args) {
                tool.args = Value::Object(args.clone());
                *cache = None;
            }
            if tool.pending && tool.name != name {
                name.clone_into(&mut tool.name);
                *cache = None;
            }
            return index;
        }
        self.push_tool(id, Tool::new(name, Value::Object(args.clone())))
    }

    fn push_tool(&mut self, id: &str, tool: Tool) -> usize {
        let index = self.push(Body::Tool(tool), Metadata::default());
        self.tools.insert(id.to_owned(), index);
        self.pending_tools.insert(index);
        index
    }

    fn stored_tool_result(&mut self, message: Arc<ToolResultMessage>) {
        let ToolResultMessage {
            tool_call_id,
            tool_name,
            content,
            details,
            usage,
            is_error,
            duration_ms,
            ..
        } = Arc::unwrap_or_clone(message);
        let result = AgentToolResult {
            content,
            details,
            usage,
            is_error: Some(is_error),
            ..AgentToolResult::default()
        };
        self.tool_result(
            &tool_call_id,
            &tool_name,
            Arc::new(result),
            is_error,
            duration_ms,
        );
    }

    fn tool_result(
        &mut self,
        id: &str,
        name: &str,
        result: Arc<AgentToolResult>,
        is_error: bool,
        duration_ms: Option<u64>,
    ) {
        let index = self
            .tools
            .get(id)
            .copied()
            .unwrap_or_else(|| self.push_tool(id, Tool::new(name, Value::Null)));
        self.pending_tools.remove(&index);
        if let Some(Row {
            body: Body::Tool(tool),
            cache,
            ..
        }) = self.rows.get_mut(index)
        {
            tool.result = Some(result);
            tool.pending = false;
            tool.is_error = is_error;
            tool.duration_ms = duration_ms;
            *cache = None;
        }
    }

    fn settle_pending(&mut self, aborted: bool) {
        if let Some(index) = self.active_assistant.take()
            && let Some(Row {
                body: Body::Assistant(assistant),
                cache,
                ..
            }) = self.rows.get_mut(index)
        {
            let settled = match &assistant.source {
                AssistantSource::Starting(message) | AssistantSource::Settled(message) => {
                    message.clone()
                }
                AssistantSource::Streaming(message) => Arc::new(
                    message
                        .read()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone(),
                ),
            };
            assistant.source = AssistantSource::Settled(settled);
            *cache = None;
        }
        for index in self.pending_tools.drain() {
            if let Some(Row {
                body: Body::Tool(tool),
                cache,
                ..
            }) = self.rows.get_mut(index)
                && tool.pending
            {
                tool.pending = false;
                tool.is_error = true;
                if tool.result.is_none() {
                    let error = if aborted {
                        "Operation aborted"
                    } else {
                        "Tool execution did not finish"
                    };
                    tool.result = Some(Arc::new(AgentToolResult::text(error)));
                }
                *cache = None;
            }
        }
    }
}

impl Row {
    fn can_expand(&self) -> bool {
        match &self.body {
            Body::Card(card) => card.is_expandable(),
            Body::Bash(_) | Body::Tool(_) => true,
            _ => false,
        }
    }

    fn can_click(&self) -> bool {
        match &self.body {
            Body::Tool(tool) => tool.has_result(),
            _ => self.can_expand(),
        }
    }

    fn refresh(&mut self, ctx: &RenderContext<'_>, key: RenderKey, default_expanded: bool) {
        let expanded = self.expanded.unwrap_or(default_expanded);
        let mut thinking = Vec::new();
        let lines = match &mut self.body {
            Body::User(user) => {
                let mut lines = if user.text.trim().is_empty() {
                    Vec::new()
                } else {
                    message::markdown(ctx, &self.metadata, &user.text, MarkdownSurface::User)
                };
                if !lines.is_empty() {
                    lines.insert(0, String::new());
                }
                lines.extend(execution::render_inline_images(
                    &user.images,
                    &mut user.image_ids,
                    ctx,
                    "userMessageText",
                ));
                lines
            }
            Body::Assistant(assistant) => {
                let lines;
                (lines, thinking) = assistant.render(ctx, &self.metadata);
                lines
            }
            Body::Card(card) => card.render(ctx, &self.metadata, expanded),
            Body::Tool(tool) => tool.render(ctx, expanded),
            Body::Bash(bash) => bash.render(ctx, expanded),
            Body::Notice(text, color) => {
                message::notice(ctx.theme, text, color, ctx.width, ctx.pad)
            }
        };
        self.cache = Some(CachedRow {
            key,
            lines,
            thinking,
        });
    }
}

#[cfg(test)]
mod tests;
