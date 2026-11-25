use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use indexmap::IndexSet;
use pi_rs_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, InputContent, JsonObject,
    Message, Model, PartialMessage, StreamOptions, StringOrBlocks, SystemMessage, Tool, ToolCall,
    ToolResultMessage, TranscriptContext, Usage, UserMessage, UserRole,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use pi_rs_ai::ThinkingLevel;
pub use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error(
        "No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn()."
    )]
    MissingStreamFunction,
    #[error("Cannot continue: no messages in context")]
    EmptyContext,
    #[error("No messages to continue from")]
    NoConversation,
    #[error("Cannot continue from message role: assistant")]
    AssistantTail,
    #[error(
        "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
    )]
    PromptInProgress,
    #[error("Agent is already processing. Wait for completion before continuing.")]
    ContinueInProgress,
    #[error("Agent is already processing. Wait for completion before resetting.")]
    ResetInProgress,
    #[error("{0}")]
    Callback(String),
    #[error(transparent)]
    AssistantStream(Arc<pi_rs_ai::Error>),
    #[error("Agent event stream ended without a result")]
    MissingAgentResult,
    #[error("Starting an agent event stream requires a Tokio runtime")]
    MissingRuntime,
}

impl From<pi_rs_ai::Error> for AgentError {
    fn from(error: pi_rs_ai::Error) -> Self {
        Self::AssistantStream(Arc::new(error))
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Custom records share the transcript and event lifecycle but are not sent by the default converter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomAgentMessage {
    #[serde(deserialize_with = "custom_role")]
    role: String,
    #[serde(flatten)]
    pub fields: JsonObject,
}

fn custom_role<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let role = String::deserialize(d)?;
    if is_standard_role(&role) {
        return Err(serde::de::Error::custom("reserved agent message role"));
    }
    Ok(role)
}

fn is_standard_role(role: &str) -> bool {
    matches!(role, "system" | "user" | "assistant" | "toolResult")
}

/// Reference-counted messages let transcripts, UI events, and tool hooks share large results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    System(Arc<SystemMessage>),
    User(Arc<UserMessage>),
    Assistant(Arc<AssistantMessage>),
    ToolResult(Arc<ToolResultMessage>),
    Custom(Arc<CustomAgentMessage>),
}

impl AgentMessage {
    pub fn user(content: impl Into<String>) -> Self {
        UserMessage {
            role: UserRole::User,
            content: StringOrBlocks::String(content.into()),
            timestamp: unix_millis(),
        }
        .into()
    }

    pub fn custom(role: impl Into<String>, mut fields: JsonObject) -> Result<Self> {
        let role = role.into();
        if is_standard_role(&role) {
            return Err(AgentError::Callback(format!(
                "Reserved message role: {role}"
            )));
        }
        fields.remove("role");
        Ok(Self::Custom(Arc::new(CustomAgentMessage { role, fields })))
    }

    pub fn role(&self) -> &str {
        match self {
            Self::System(_) => "system",
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "toolResult",
            Self::Custom(message) => &message.role,
        }
    }

    pub fn to_llm(&self) -> Option<Message> {
        match self {
            Self::System(message) => Some(Message::System((**message).clone())),
            Self::User(message) => Some(Message::User((**message).clone())),
            Self::Assistant(message) => Some(Message::Assistant((**message).clone())),
            Self::ToolResult(message) => Some(Message::ToolResult((**message).clone())),
            Self::Custom(_) => None,
        }
    }
}

macro_rules! message_from {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        $(impl From<$ty> for AgentMessage {
            fn from(message: $ty) -> Self {
                Self::$variant(Arc::new(message))
            }
        })+
    };
}
message_from!(
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage)
);

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        match message {
            Message::System(message) => message.into(),
            Message::User(message) => message.into(),
            Message::Assistant(message) => message.into(),
            Message::ToolResult(message) => message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    OneAtATime,
    All,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolReplay {
    Never,
    Safe,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    #[serde(default)]
    pub content: Vec<InputContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![InputContent::Text(text.into().into())],
            ..Self::default()
        }
    }
}

pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;
pub type AgentToolCall = ToolCall;

pub trait AgentTool: Send + Sync {
    fn declaration(&self) -> &Tool;
    fn label(&self) -> &str;

    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>>;

    fn prepare_arguments(&self, _args: &mut JsonObject) -> Result<()> {
        Ok(())
    }
    fn output_schema(&self) -> Option<&Value> {
        None
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }
    fn replay(&self) -> Option<ToolReplay> {
        None
    }
}

#[derive(Clone, Default)]
pub struct AgentContext {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

#[derive(Clone)]
pub struct AgentState {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<StreamingMessage>,
    pub pending_tool_calls: IndexSet<String>,
    pub error_message: Option<String>,
}

/// Committed message starts and live provider deltas share a single streaming state slot.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum StreamingMessage {
    Fixed(AgentMessage),
    Assistant(PartialMessage),
}

impl StreamingMessage {
    pub fn snapshot(&self) -> AgentMessage {
        match self {
            Self::Fixed(message) => message.clone(),
            Self::Assistant(message) => message
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentToolCallOutcome {
    pub tool_call: AgentToolCall,
    pub result: Arc<AgentToolResult>,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<InputContent>>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a AgentToolCall,
    pub args: &'a mut Value,
    pub context: &'a AgentContext,
}

pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a AgentToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
    pub context: &'a AgentContext,
}

pub struct AgentTurnContext<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [Arc<ToolResultMessage>],
    pub context: &'a AgentContext,
    pub new_messages: &'a [AgentMessage],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum AgentTurnDecision {
    Continue,
    End,
}

#[derive(Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub messages: Vec<AgentMessage>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub type PrepareNextTurnContext<'a> = AgentTurnContext<'a>;

pub struct PrepareRequestContext<'a> {
    pub context: &'a AgentContext,
    pub model: &'a Model,
    pub thinking_level: ThinkingLevel,
}

#[derive(Default)]
pub struct AgentRequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub type StreamFn = Arc<
    dyn Fn(
            Model,
            TranscriptContext,
            StreamOptions,
        ) -> BoxFuture<'static, Result<AssistantMessageEventStream>>
        + Send
        + Sync,
>;
pub type ConvertToLlm =
    Arc<dyn for<'a> Fn(&'a [AgentMessage]) -> BoxFuture<'a, Vec<Message>> + Send + Sync>;
pub type TransformContext = Arc<
    dyn for<'a> Fn(&'a [AgentMessage], &'a CancellationToken) -> BoxFuture<'a, Vec<AgentMessage>>
        + Send
        + Sync,
>;
pub type GetApiKey = Arc<dyn for<'a> Fn(&'a str) -> BoxFuture<'a, Option<String>> + Send + Sync>;
pub type GetQueuedMessages = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;
pub type BeforeToolCall = Arc<
    dyn for<'a> Fn(
            BeforeToolCallContext<'a>,
            &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Option<BeforeToolCallResult>>>
        + Send
        + Sync,
>;
pub type AfterToolCall = Arc<
    dyn for<'a> Fn(
            AfterToolCallContext<'a>,
            &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Option<AfterToolCallResult>>>
        + Send
        + Sync,
>;
pub type FinishTurn = Arc<
    dyn for<'a> Fn(
            AgentTurnContext<'a>,
            &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Option<AgentTurnDecision>>>
        + Send
        + Sync,
>;
pub type PrepareRequest = Arc<
    dyn for<'a> Fn(
            PrepareRequestContext<'a>,
            &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Option<AgentRequestUpdate>>>
        + Send
        + Sync,
>;
pub type PrepareNextTurn = Arc<
    dyn for<'a> Fn(
            PrepareNextTurnContext<'a>,
            &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Option<AgentLoopTurnUpdate>>>
        + Send
        + Sync,
>;
pub type PrepareNextTurnWithSignal = Arc<
    dyn for<'a> Fn(&'a CancellationToken) -> BoxFuture<'a, Result<Option<AgentLoopTurnUpdate>>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct ToolCallHooks {
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
}

#[derive(Clone, Default)]
pub struct AgentHooks {
    pub convert_to_llm: Option<ConvertToLlm>,
    pub transform_context: Option<TransformContext>,
    pub get_api_key: Option<GetApiKey>,
    pub finish_turn: Option<FinishTurn>,
    pub prepare_request: Option<PrepareRequest>,
    pub prepare_next_turn: Option<PrepareNextTurn>,
    pub tool_calls: ToolCallHooks,
}

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub stream_options: StreamOptions,
    pub hooks: AgentHooks,
    pub get_steering_messages: Option<GetQueuedMessages>,
    pub get_follow_up_messages: Option<GetQueuedMessages>,
    pub tool_execution: ToolExecutionMode,
}

impl AgentLoopConfig {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            stream_options: StreamOptions::default(),
            hooks: AgentHooks::default(),
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: ToolExecutionMode::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Arc<[AgentMessage]>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<Arc<ToolResultMessage>>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: PartialMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: JsonObject,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: JsonObject,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: Arc<AgentToolResult>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

pub type AgentEventSink<'a> = dyn Fn(AgentEvent) -> BoxFuture<'a, Result<()>> + Send + Sync + 'a;
pub type AgentListener = dyn for<'a> Fn(&'a AgentEvent, &'a CancellationToken) -> BoxFuture<'a, Result<()>>
    + Send
    + Sync;

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn system_messages(messages: &[AgentMessage]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|message| match message {
        AgentMessage::System(system) => Some(system.as_ref()),
        _ => None,
    })
}
