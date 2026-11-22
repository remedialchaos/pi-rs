use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::model::{Api, ClassifierApi, ImageApi, Model, ModelCost, ProviderId, ThinkingLevel};

pub type JsonObject = Map<String, Value>;

macro_rules! tag_type {
    ($name:ident, $variant:ident, $tag:literal) => {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            #[default]
            #[serde(rename=$tag)]
            $variant,
        }
    };
}

tag_type!(TextKind, Text, "text");
tag_type!(ImageContentKind, Image, "image");
tag_type!(ThinkingKind, Thinking, "thinking");
tag_type!(ToolCallKind, ToolCall, "toolCall");
tag_type!(SystemRole, System, "system");
tag_type!(UserRole, User, "user");
tag_type!(AssistantRole, Assistant, "assistant");
tag_type!(ToolResultRole, ToolResult, "toolResult");

type OptionalString = Option<String>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: TextKind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: OptionalString,
}

impl From<String> for TextContent {
    fn from(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}
impl From<&str> for TextContent {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub kind: ImageContentKind,
    pub data: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub kind: ThinkingKind,
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub id: String,
    pub name: String,
    pub arguments: JsonObject,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_error: OptionalString,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrBlocks<B> {
    String(String),
    Blocks(Vec<B>),
}
impl<B> Default for StringOrBlocks<B> {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
}

fn null_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub role: SystemRole,
    #[serde(default, deserialize_with = "null_default")]
    pub content: StringOrBlocks<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<IndexMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<ToolReference>>,
    pub timestamp: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub role: UserRole,
    #[serde(default, deserialize_with = "null_default")]
    pub content: StringOrBlocks<InputContent>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub content: Vec<InputContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedToolCalls>,
    pub is_error: bool,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedToolStatus {
    Ok,
    Error,
    Unfinished,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCallRecord {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<usize>,
    pub status: NestedToolStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: OptionalString,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCallRecord>,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}
impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "toolUse",
            Self::Error => "error",
            Self::Aborted => "aborted",
            Self::Deferred => "deferred",
        }
    }
    pub fn failed(self) -> bool {
        matches!(self, Self::Error | Self::Aborted)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_from_provider: Option<bool>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    pub total_tokens: f64,
    pub cost: Cost,
}

impl Usage {
    pub fn calculate_cost(&mut self, model_cost: &ModelCost) -> &Cost {
        let input_tokens = self.input + self.cache_read + self.cache_write;
        let mut rates = &model_cost.rates;
        let mut matched = -1.0;
        for tier in &model_cost.tiers {
            if input_tokens > tier.input_tokens_above && tier.input_tokens_above > matched {
                rates = &tier.rates;
                matched = tier.input_tokens_above;
            }
        }
        let long = self.cache_write_1h.unwrap_or(0.0);
        let input = (rates.input / 1e6) * self.input;
        let output = (rates.output / 1e6) * self.output;
        let cache_read = (rates.cache_read / 1e6) * self.cache_read;
        let cache_write =
            (rates.cache_write * (self.cache_write - long) + rates.input * 2.0 * long) / 1e6;
        self.cost = Cost {
            input,
            output,
            cache_read,
            cache_write,
            total: input + output + cache_read + cache_write,
            total_from_provider: Some(false),
        };
        &self.cost
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: Api,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub role: AssistantRole,
    #[serde(default, deserialize_with = "null_default")]
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<JsonObject>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}
impl AssistantMessage {
    pub fn pending(model: &Model) -> Self {
        Self {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: unix_millis(),
            duration_ms: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[expect(
    clippy::large_enum_variant,
    reason = "Replay consumes owned messages from a contiguous transcript; do not box every reply."
)]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: OptionalString,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TranscriptContext {
    pub(crate) messages: Vec<Message>,
}
impl TranscriptContext {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrictPreference {
    Prefer,
    Require,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    JsonSchema { strict: StrictPreference },
    Grammar { variants: IndexMap<String, String> },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConstrainedSampling {
    Enabled(ConstrainedSamplingConfig),
    Disabled(bool),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSampling>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<InputContent>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: ImageApi,
    pub provider: ProviderId,
    pub model: String,
    pub output: Vec<InputContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: OptionalString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: OptionalString,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierQuestion {
    Choice {
        instructions: String,
        criteria: IndexMap<String, String>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Bool {
        instructions: String,
        criteria: BoolCriteria,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoolCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassifierContext {
    pub state: JsonObject,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageContent>,
    pub questions: IndexMap<String, ClassifierQuestion>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierAnswer {
    Choice {
        choice: String,
        probabilities: IndexMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        confidence: f64,
    },
    Bool {
        probability: f64,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    pub api: ClassifierApi,
    pub provider: ProviderId,
    pub model: String,
    pub answers: IndexMap<String, ClassifierAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: OptionalString,
    pub timestamp: u64,
}

pub type PartialMessage = Arc<RwLock<AssistantMessage>>;
pub type FinalMessage = Arc<AssistantMessage>;
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantMessageEvent {
    Start {
        partial: PartialMessage,
    },
    TextStart {
        content_index: usize,
        partial: PartialMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: PartialMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: PartialMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: PartialMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: PartialMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: PartialMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        partial: PartialMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: PartialMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: PartialMessage,
    },
    Done {
        reason: StopReason,
        message: FinalMessage,
    },
    Error {
        reason: StopReason,
        error: FinalMessage,
    },
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
