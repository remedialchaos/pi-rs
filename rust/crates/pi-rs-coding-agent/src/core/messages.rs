//! Coding-agent message types and transformers. Ported from `core/messages.ts`.
//!
//! Custom records share the transcript with LLM messages but are converted to
//! user messages before being sent to a provider. The same conversion is used by
//! compaction, queued prompts, and session export.

use pi_rs_agent::AgentMessage;
use pi_rs_ai::{
    AssistantContent, ImageContent, ImageContentKind, InputContent, Message, StringOrBlocks,
    TextContent, ToolResultMessage, UserMessage, UserRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wrapper emitted before a compacted history summary.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Wrapper emitted before a summary of a branch that was left.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Result of a `!` bash command shown to the user and, unless excluded, the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: u64,
    /// Set by the `!!` prefix; keeps the message out of LLM context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// Content injected into the conversation by an extension via `sendMessage()`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomContent {
    Text(String),
    Blocks(Vec<InputContent>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: CustomContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: Option<String>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: u64,
}

/// A session entry carrying one agent message, as written by sessions/export.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageEntry {
    pub role: String,
    pub message: AgentMessage,
}

impl BashExecutionMessage {
    /// Render a bash execution as the user message text sent to the model.
    pub fn to_context_text(&self) -> String {
        let mut text = format!("Ran `{}`\n", self.command);
        if self.output.is_empty() {
            text.push_str("(no output)");
        } else {
            text.push_str("```\n");
            text.push_str(&self.output);
            text.push_str("\n```");
        }
        if self.cancelled {
            text.push_str("\n\n(command cancelled)");
        } else if let Some(code) = self.exit_code
            && code != 0
        {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        if self.truncated
            && let Some(path) = &self.full_output_path
        {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
        text
    }

    /// Convert to the extensible agent message used by sessions.
    pub fn to_agent_message(&self) -> Result<AgentMessage, String> {
        let mut fields = Map::new();
        fields.insert("command".into(), Value::String(self.command.clone()));
        fields.insert("output".into(), Value::String(self.output.clone()));
        fields.insert(
            "exitCode".into(),
            self.exit_code.map_or(Value::Null, Value::from),
        );
        fields.insert("cancelled".into(), Value::Bool(self.cancelled));
        fields.insert("truncated".into(), Value::Bool(self.truncated));
        if let Some(path) = &self.full_output_path {
            fields.insert("fullOutputPath".into(), Value::String(path.clone()));
        }
        fields.insert("timestamp".into(), Value::from(self.timestamp));
        if let Some(excluded) = self.exclude_from_context {
            fields.insert("excludeFromContext".into(), Value::Bool(excluded));
        }
        AgentMessage::custom("bashExecution", fields).map_err(|error| error.to_string())
    }
}

/// Free function form of [`BashExecutionMessage::to_context_text`].
pub fn bash_execution_to_text(message: &BashExecutionMessage) -> String {
    message.to_context_text()
}

impl CustomMessage {
    pub fn to_agent_message(&self) -> Result<AgentMessage, String> {
        let mut fields = Map::new();
        fields.insert("customType".into(), Value::String(self.custom_type.clone()));
        match &self.content {
            CustomContent::Text(text) => {
                fields.insert("content".into(), Value::String(text.clone()));
            }
            CustomContent::Blocks(blocks) => {
                fields.insert(
                    "content".into(),
                    serde_json::to_value(blocks).map_err(|error| error.to_string())?,
                );
            }
        }
        fields.insert("display".into(), Value::Bool(self.display));
        if let Some(details) = &self.details {
            fields.insert("details".into(), details.clone());
        }
        fields.insert("timestamp".into(), Value::from(self.timestamp));
        AgentMessage::custom("custom", fields).map_err(|error| error.to_string())
    }
}

impl BranchSummaryMessage {
    pub fn to_agent_message(&self) -> Result<AgentMessage, String> {
        let mut fields = Map::new();
        fields.insert("summary".into(), Value::String(self.summary.clone()));
        fields.insert(
            "fromId".into(),
            self.from_id
                .as_ref()
                .map_or(Value::Null, |id| Value::String(id.clone())),
        );
        fields.insert("timestamp".into(), Value::from(self.timestamp));
        AgentMessage::custom("branchSummary", fields).map_err(|error| error.to_string())
    }
}

impl CompactionSummaryMessage {
    pub fn to_agent_message(&self) -> Result<AgentMessage, String> {
        let mut fields = Map::new();
        fields.insert("summary".into(), Value::String(self.summary.clone()));
        fields.insert("tokensBefore".into(), Value::from(self.tokens_before));
        fields.insert("timestamp".into(), Value::from(self.timestamp));
        AgentMessage::custom("compactionSummary", fields).map_err(|error| error.to_string())
    }
}

/// Concatenate the text blocks in a user/system content value.
pub fn content_to_text(content: &StringOrBlocks<TextContent>) -> String {
    match content {
        StringOrBlocks::String(text) => text.clone(),
        StringOrBlocks::Blocks(blocks) => blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Concatenate the text blocks in model input content.
pub fn input_content_to_text(content: &[InputContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            InputContent::Text(text) => Some(text.text.as_str()),
            InputContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Concatenate the text of a user message's string-or-blocks content.
pub fn user_content_to_text(content: &StringOrBlocks<InputContent>) -> String {
    match content {
        StringOrBlocks::String(text) => text.clone(),
        StringOrBlocks::Blocks(blocks) => input_content_to_text(blocks),
    }
}

/// Collect inline image blocks from model input content.
pub fn image_blocks(content: &[InputContent]) -> Vec<ImageContent> {
    content
        .iter()
        .filter_map(|block| match block {
            InputContent::Image(image) => Some(image.clone()),
            InputContent::Text(_) => None,
        })
        .collect()
}

/// Build a `!` prefix bash command from a text message, if present.
pub fn bash_command_from_text(text: &str) -> Option<&str> {
    text.strip_prefix('!').filter(|rest| !rest.is_empty())
}

/// Format a tool result as a human-readable line for transcripts.
pub fn tool_result_to_text(result: &ToolResultMessage) -> String {
    let content = input_content_to_text(&result.content);
    let body = if result.is_error && content.is_empty() {
        "error"
    } else {
        content.as_str()
    };
    format!("{}: {}", result.tool_name, body)
}

/// Extract a plain-text representation of any agent message.
pub fn message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::System(system) => content_to_text(&system.content),
        AgentMessage::User(user) => match &user.content {
            StringOrBlocks::String(text) => text.clone(),
            StringOrBlocks::Blocks(blocks) => input_content_to_text(blocks),
        },
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                AssistantContent::Thinking(thinking) => Some(thinking.thinking.as_str()),
                AssistantContent::ToolCall(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::ToolResult(result) => tool_result_to_text(result),
        AgentMessage::Custom(custom) => custom
            .fields
            .values()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Extract inline image blocks from any agent message.
pub fn message_images(message: &AgentMessage) -> Vec<ImageContent> {
    match message {
        AgentMessage::User(user) => match &user.content {
            StringOrBlocks::String(_) => Vec::new(),
            StringOrBlocks::Blocks(blocks) => image_blocks(blocks),
        },
        AgentMessage::ToolResult(result) => image_blocks(&result.content),
        _ => Vec::new(),
    }
}

fn custom_content_from_value(value: Option<&Value>) -> CustomContent {
    match value {
        Some(Value::String(text)) => CustomContent::Text(text.clone()),
        Some(Value::Array(items)) => serde_json::from_value(Value::Array(items.clone()))
            .map_or(CustomContent::Text(String::new()), CustomContent::Blocks),
        _ => CustomContent::Text(String::new()),
    }
}

fn custom_message_to_llm(fields: &Map<String, Value>, timestamp: u64) -> Option<Message> {
    let content = custom_content_from_value(fields.get("content"));
    let content = match content {
        CustomContent::Text(text) => StringOrBlocks::String(text),
        CustomContent::Blocks(blocks) => StringOrBlocks::Blocks(blocks),
    };
    Some(Message::User(UserMessage {
        role: UserRole::User,
        content,
        timestamp,
    }))
}

fn custom_agent_to_llm(
    message: &AgentMessage,
    custom: &pi_rs_agent::CustomAgentMessage,
) -> Option<Message> {
    let timestamp = custom
        .fields
        .get("timestamp")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match message.role() {
        "custom" => custom_message_to_llm(&custom.fields, timestamp),
        "bashExecution" => {
            if custom
                .fields
                .get("excludeFromContext")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            let command = custom
                .fields
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            let output = custom
                .fields
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = BashExecutionMessage {
                command: command.to_string(),
                output: output.to_string(),
                exit_code: custom
                    .fields
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok()),
                cancelled: custom
                    .fields
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                truncated: custom
                    .fields
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                full_output_path: custom
                    .fields
                    .get("fullOutputPath")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                timestamp,
                exclude_from_context: None,
            }
            .to_context_text();
            Some(Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(text),
                timestamp,
            }))
        }
        "branchSummary" => {
            let summary = custom
                .fields
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}");
            Some(Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(text),
                timestamp,
            }))
        }
        "compactionSummary" => {
            let summary = custom
                .fields
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}");
            Some(Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(text),
                timestamp,
            }))
        }
        _ => None,
    }
}

/// Convert agent messages (including coding-agent custom types) to LLM messages.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(custom) => custom_agent_to_llm(message, custom),
            other => other.to_llm(),
        })
        .collect()
}

/// Wrap an agent message in a session/export entry.
pub fn message_to_entry(message: AgentMessage) -> MessageEntry {
    let role = message.role().to_string();
    MessageEntry { role, message }
}

/// Create an inline image content block from base64 data and a MIME type.
pub fn make_image_block(data: impl Into<String>, mime_type: impl Into<String>) -> InputContent {
    InputContent::Image(ImageContent {
        kind: ImageContentKind::Image,
        data: data.into(),
        mime_type: mime_type.into(),
    })
}

/// Create a text content block.
pub fn make_text_block(text: impl Into<String>) -> TextContent {
    TextContent {
        text: text.into(),
        ..TextContent::default()
    }
}

/// Create a text block as model input content.
pub fn make_input_text_block(text: impl Into<String>) -> InputContent {
    InputContent::Text(make_text_block(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_ai::{AssistantMessage, AssistantRole, StopReason, Usage};

    fn bash(command: &str) -> BashExecutionMessage {
        BashExecutionMessage {
            command: command.into(),
            output: "hello\n".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 42,
            exclude_from_context: None,
        }
    }

    #[test]
    fn bash_text_uses_fenced_output() {
        let text = bash("ls").to_context_text();
        assert_eq!(text, "Ran `ls`\n```\nhello\n\n```");
    }

    #[test]
    fn bash_text_reports_no_output_and_exit_code() {
        let mut message = bash("false");
        message.output.clear();
        message.exit_code = Some(7);
        let text = message.to_context_text();
        assert!(text.contains("(no output)"));
        assert!(text.contains("Command exited with code 7"));
    }

    #[test]
    fn bash_text_reports_cancellation_and_truncation() {
        let mut message = bash("long");
        message.cancelled = true;
        message.truncated = true;
        message.full_output_path = Some("/tmp/full.log".into());
        let text = message.to_context_text();
        assert!(text.contains("(command cancelled)"));
        assert!(text.contains("[Output truncated. Full output: /tmp/full.log]"));
    }

    #[test]
    fn content_helpers_extract_text_and_images() {
        let blocks = vec![
            make_input_text_block("hello"),
            make_image_block("AAAA", "image/png"),
            make_input_text_block("world"),
        ];
        assert_eq!(input_content_to_text(&blocks), "hello\nworld");
        assert_eq!(image_blocks(&blocks).len(), 1);
        assert_eq!(image_blocks(&blocks)[0].mime_type, "image/png");
    }

    #[test]
    fn custom_messages_convert_to_user_text() -> Result<(), String> {
        let message = CustomMessage {
            custom_type: "note".into(),
            content: CustomContent::Text("remember".into()),
            display: true,
            details: None,
            timestamp: 1,
        };
        let agent = message
            .to_agent_message()
            .map_err(|error| error.to_string())?;
        let llm = convert_to_llm(&[agent]);
        assert_eq!(llm.len(), 1);
        match &llm[0] {
            Message::User(user) => assert_eq!(user_content_to_text(&user.content), "remember"),
            _ => return Err("expected user message".to_string()),
        }
        Ok(())
    }

    #[test]
    fn excluded_bash_messages_are_dropped_from_context() -> Result<(), String> {
        let mut message = bash("secret");
        message.exclude_from_context = Some(true);
        let agent = message
            .to_agent_message()
            .map_err(|error| error.to_string())?;
        assert!(convert_to_llm(&[agent]).is_empty());
        Ok(())
    }

    #[test]
    fn summary_messages_use_exact_wrappers() -> Result<(), String> {
        let branch = BranchSummaryMessage {
            summary: "old branch".into(),
            from_id: Some("abc".into()),
            timestamp: 5,
        };
        let agent = branch
            .to_agent_message()
            .map_err(|error| error.to_string())?;
        let llm = convert_to_llm(&[agent]);
        match &llm[0] {
            Message::User(user) => assert_eq!(
                user_content_to_text(&user.content),
                format!("{BRANCH_SUMMARY_PREFIX}old branch{BRANCH_SUMMARY_SUFFIX}")
            ),
            _ => return Err("expected user message".to_string()),
        }
        Ok(())
    }

    #[test]
    fn standard_agent_messages_pass_through_conversion() {
        let assistant = AgentMessage::from(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(make_text_block("hi"))],
            api: pi_rs_ai::Api::OpenAiCompletions,
            provider: "openai".into(),
            model: "gpt".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        });
        assert_eq!(message_text(&assistant), "hi");
        assert!(matches!(
            convert_to_llm(&[assistant])[0],
            Message::Assistant(_)
        ));
    }

    #[test]
    fn message_entries_keep_their_role() {
        let message = AgentMessage::user("hello");
        let entry = message_to_entry(message);
        assert_eq!(entry.role, "user");
        assert_eq!(message_text(&entry.message), "hello");
    }

    #[test]
    fn tool_result_text_includes_error_marker() {
        let mut result = ToolResultMessage {
            role: pi_rs_ai::ToolResultRole::ToolResult,
            tool_call_id: "1".into(),
            tool_name: "bash".into(),
            content: vec![make_input_text_block("boom")],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: true,
            timestamp: 0,
            duration_ms: None,
        };
        assert_eq!(tool_result_to_text(&result), "bash: boom");
        result.content.clear();
        assert_eq!(tool_result_to_text(&result), "bash: error");
    }

    #[test]
    fn bash_command_parser_requires_nonempty_command() {
        assert_eq!(bash_command_from_text("!ls"), Some("ls"));
        assert_eq!(bash_command_from_text("!"), None);
        assert_eq!(bash_command_from_text("plain"), None);
    }
}
