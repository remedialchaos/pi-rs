use pi_rs_agent::QueueMode;
use pi_rs_ai::{ImageContent, Model, ThinkingLevel};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::core::skills::SourceInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub command: RpcCommand,
}

macro_rules! commands {
    ($($variant:ident $({$($(#[$attr:meta])* $field:ident: $value:ty),* $(,)?})? => $wire:literal),* $(,)?) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all_fields = "camelCase")]
        pub enum RpcCommand {
            $(#[serde(rename = $wire)] $variant $({$($(#[$attr])* $field: $value),*})?),*
        }

        impl RpcCommand {
            pub fn name(&self) -> &'static str {
                match self { $(Self::$variant $({$($field: _,)*})? => $wire),* }
            }

            pub(crate) fn is_known(name: &str) -> bool {
                matches!(name, $($wire)|*)
            }
        }
    };
}

commands! {
    Prompt {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<RpcStreamingBehavior>,
    } => "prompt",
    Steer {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
    } => "steer",
    FollowUp {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
    } => "follow_up",
    Abort => "abort",
    ClearQueue => "clear_queue",
    NewSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    } => "new_session",
    GetState => "get_state",
    SetModel { provider: String, model_id: String } => "set_model",
    CycleModel => "cycle_model",
    GetAvailableModels => "get_available_models",
    SetThinkingLevel { level: ThinkingLevel } => "set_thinking_level",
    CycleThinkingLevel => "cycle_thinking_level",
    GetAvailableThinkingLevels => "get_available_thinking_levels",
    SetSteeringMode { mode: QueueMode } => "set_steering_mode",
    SetFollowUpMode { mode: QueueMode } => "set_follow_up_mode",
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    } => "compact",
    SetAutoCompaction { enabled: bool } => "set_auto_compaction",
    SetAutoRetry { enabled: bool } => "set_auto_retry",
    AbortRetry => "abort_retry",
    Bash {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
    } => "bash",
    AbortBash => "abort_bash",
    GetSessionStats => "get_session_stats",
    ExportHtml {
        #[serde(skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
    } => "export_html",
    SwitchSession { session_path: String } => "switch_session",
    Fork { entry_id: String } => "fork",
    Clone => "clone",
    GetForkMessages => "get_fork_messages",
    GetEntries {
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    } => "get_entries",
    GetTree => "get_tree",
    GetLastAssistantText => "get_last_assistant_text",
    SetSessionName { name: String } => "set_session_name",
    GetMessages => "get_messages",
    GetCommands => "get_commands",
}

pub type RpcImage = ImageContent;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcStreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "response")]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(
        default,
        deserialize_with = "present_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn present_data<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl RpcResponse {
    pub(crate) fn ok(id: Option<String>, command: &str, data: Option<Value>) -> Self {
        Self {
            id,
            command: command.into(),
            success: true,
            data,
            error: None,
        }
    }

    pub(crate) fn error(id: Option<String>, command: &str, error: String) -> Self {
        Self {
            id,
            command: command.into(),
            success: false,
            data: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSlashCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: RpcSlashCommandSource,
    pub source_info: SourceInfo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcSlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "extension_ui_request")]
pub struct RpcExtensionUiRequest {
    pub id: String,
    #[serde(flatten)]
    pub method: RpcUiMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RpcUiMethod {
    Select {
        title: String,
        options: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Confirm {
        title: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Input {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Editor {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefill: Option<String>,
    },
    Notify {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        notify_type: Option<String>,
    },
    SetStatus {
        status_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_text: Option<String>,
    },
    SetWidget {
        widget_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_lines: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_placement: Option<RpcWidgetPlacement>,
    },
    SetTitle {
        title: String,
    },
    #[serde(rename = "set_editor_text")]
    SetEditorText {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcWidgetPlacement {
    AboveEditor,
    BelowEditor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "extension_ui_response")]
pub struct RpcExtensionUiResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcDisposition {
    Handled,
    Queued,
    Started,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcQueue {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RpcCancelled {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcForkResult {
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcForkMessage {
    pub entry_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcModelCycle {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_scoped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBashResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcCompactionResult {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<u64>,
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_rs_ai::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: RpcTokenCounts,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<RpcContextUsage>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTokenCounts {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEntries {
    pub entries: Vec<crate::core::session_manager::SessionEntry>,
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTree {
    pub tree: Vec<RpcTreeNode>,
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTreeNode {
    pub entry: crate::core::session_manager::SessionEntry,
    pub children: Vec<RpcTreeNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<String>,
}
