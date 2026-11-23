use crate::{
    EntryId, JsonObject, SubmissionId, TaskId, entries::CompactionReason, tools::ToolDiagnostic,
};
use pi_rs_ai::types::{AssistantMessage, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotStatus {
    Pending,
    Running,
    Sleeping,
    Waiting,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureReason {
    ModelError,
    NoModel,
    Aborted,
    Reset,
    Faulted,
    Orphaned,
    Interrupted,
    RestartLimit,
    NothingToAnswer,
    TurnLimit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotProgress {
    pub call_id: String,
    pub name: String,
    pub status: SlotStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    #[serde(flatten)]
    pub progress: SlotProgress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unoffered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSlot {
    #[serde(flatten)]
    pub progress: SlotProgress,
    pub parent_call_id: String,
    pub parent_task_id: TaskId,
    pub task_id: TaskId,
    pub arguments: JsonObject,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<NestedToolSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSummary {
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStatus {
    pub at: u64,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    pub task_id: TaskId,
    pub trigger: CompactionReason,
    pub blocking: bool,
    pub attempt: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailure {
    pub task_id: TaskId,
    pub trigger: CompactionReason,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFailure {
    pub task_id: TaskId,
    pub inputs: Vec<SubmissionId>,
    pub reason: RunFailureReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<EntryId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterRunTasks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<TaskId>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YieldTasks {
    pub answer: EntryId,
    pub tasks: Vec<TaskId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRun {
    pub task_id: TaskId,
    pub inputs: Vec<SubmissionId>,
    pub turns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_run: Option<AfterRunTasks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#yield: Option<YieldTasks>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredStatus {
    pub poll_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationStatus {
    pub attempt: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AssistantMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<LiveRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_tools: Option<Vec<NestedToolSlot>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_failure: Option<RunFailure>,
}
