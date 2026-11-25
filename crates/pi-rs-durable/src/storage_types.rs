use crate::ids::*;
use crate::signals::SignalKey;
use crate::storage::StorageError;
use pi_rs_chord::delta::Op;
use pi_rs_telemetry::TelemetryCarrier;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type JsonObject = Map<String, Value>;
pub type Cursor = JsonObject;

// JSON null is a valid payload, distinct from an omitted optional field.
fn optional_json<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConversationOwnership {
    Ownerless,
    Task { task_id: TaskId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationParent {
    pub conversation_id: ConversationId,
    pub at: EntryId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationOwner {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRecord {
    pub id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationParent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum ContextEdit {
    Omit {
        target: EntryId,
    },
    Replace {
        target: EntryId,
        messages: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    pub id: EntryId,
    pub conversation_id: ConversationId,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Value>>,
    #[serde(
        default,
        deserialize_with = "optional_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_task_id: Option<TaskId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "EntryHeadWire", into = "EntryHeadWire")]
pub enum EntryHead {
    Id(EntryId),
    SelfEntry,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
enum EntryHeadWire {
    Id(EntryId),
    SelfEntry(SelfHead),
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum SelfHead {
    #[serde(rename = "self")]
    SelfEntry,
}
impl From<EntryHeadWire> for EntryHead {
    fn from(value: EntryHeadWire) -> Self {
        match value {
            EntryHeadWire::Id(id) => Self::Id(id),
            EntryHeadWire::SelfEntry(_) => Self::SelfEntry,
        }
    }
}
impl From<EntryHead> for EntryHeadWire {
    fn from(value: EntryHead) -> Self {
        match value {
            EntryHead::Id(id) => Self::Id(id),
            EntryHead::SelfEntry => Self::SelfEntry(SelfHead::SelfEntry),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDraft {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Value>>,
    #[serde(
        default,
        deserialize_with = "optional_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryHead>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEntry {
    pub entry: EntryRecord,
    pub commit_seq: Seq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionKind {
    Input,
    Write,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionStatus {
    Queued,
    Placed,
    Done,
    Unanswered,
}
impl SubmissionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Placed => "placed",
            Self::Done => "done",
            Self::Unanswered => "unanswered",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionNotify {
    pub task_id: TaskId,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionCreate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TelemetryCarrier>,
    pub conversation_id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<SubmissionNotify>,
    #[serde(rename = "type")]
    pub kind: SubmissionKind,
    pub status: SubmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TelemetryCarrier>,
    pub id: SubmissionId,
    pub conversation_id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<SubmissionNotify>,
    #[serde(rename = "type")]
    pub kind: SubmissionKind,
    pub status: SubmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<Value>,
}

impl SubmissionCreate {
    pub fn with_id(self, id: SubmissionId) -> SubmissionRecord {
        SubmissionRecord {
            id,
            trace: self.trace,
            conversation_id: self.conversation_id,
            request_id: self.request_id,
            notify: self.notify,
            kind: self.kind,
            status: self.status,
            entry: self.entry,
            answer: self.answer,
            reason: self.reason,
            detail: self.detail,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SubmissionSettlement {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: String,
        #[serde(
            default,
            deserialize_with = "optional_json",
            skip_serializing_if = "Option::is_none"
        )]
        detail: Option<Value>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TaskOwnership {
    Conversation,
    Task { task_id: TaskId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum WakeReason {
    Time,
    Expired,
    Signal { signal: String, key: String },
    Tasks { ended: Vec<TaskId> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskAbortReason {
    Restart,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutcomeError {
    pub message: String,
    #[serde(
        default,
        deserialize_with = "optional_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    bound(deserialize = "R: Deserialize<'de>")
)]
pub enum TaskOutcome<R = Value> {
    Completed {
        result: R,
    },
    Failed {
        error: TaskOutcomeError,
        #[serde(
            default,
            deserialize_with = "optional_payload",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<R>,
    },
    Aborted {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(
            default,
            deserialize_with = "optional_payload",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<R>,
    },
    Orphaned {
        reason: String,
    },
    Faulted {
        error: TaskOutcomeError,
    },
}
fn optional_payload<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Sleeping,
    Completing,
    Terminal,
}
impl TaskStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Sleeping => "sleeping",
            Self::Completing => "completing",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskState<S = Value, R = Value> {
    Pending {
        checkpoint: S,
        #[serde(skip_serializing_if = "Option::is_none")]
        wake: Option<WakeReason>,
    },
    Running {
        checkpoint: S,
        #[serde(skip_serializing_if = "Option::is_none")]
        wake: Option<WakeReason>,
    },
    Waiting {
        checkpoint: S,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Sleeping {
        checkpoint: S,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signals: Option<Vec<SignalKey>>,
    },
    Completing {
        outcome: TaskOutcome<R>,
    },
    Terminal {
        outcome: TaskOutcome<R>,
    },
}
impl<S, R> TaskState<S, R> {
    pub const fn status(&self) -> TaskStatus {
        match self {
            Self::Pending { .. } => TaskStatus::Pending,
            Self::Running { .. } => TaskStatus::Running,
            Self::Waiting { .. } => TaskStatus::Waiting,
            Self::Sleeping { .. } => TaskStatus::Sleeping,
            Self::Completing { .. } => TaskStatus::Completing,
            Self::Terminal { .. } => TaskStatus::Terminal,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord<I = Value, S = Value, R = Value> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TelemetryCarrier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<TelemetryCarrier>>,
    pub id: TaskId,
    pub conversation_id: ConversationId,
    pub kind: String,
    pub version: u64,
    pub input: I,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    pub background: bool,
    pub abort_requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<TaskAbortReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abandon_on_restart: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u64>,
    pub state: TaskState<S, R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memos: Option<JsonObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DocumentScope {
    Session,
    Conversation { conversation_id: ConversationId },
    Task { task_id: TaskId },
}
impl DocumentScope {
    pub const fn columns(self) -> (&'static str, u64) {
        match self {
            Self::Session => ("session", 0),
            Self::Conversation { conversation_id } => ("conversation", conversation_id),
            Self::Task { task_id } => ("task", task_id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentHistory {
    Latest,
    Rewindable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentFork {
    Current,
    Initial,
    AsOf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    pub id: DocumentId,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: DocumentScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
    pub created_at: Seq,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Seq>,
}
impl DocumentRecord {
    pub fn address(&self) -> DocumentAddress {
        DocumentAddress {
            kind: self.kind.clone(),
            scope: self.scope,
            key: self.key.clone(),
        }
    }
    pub fn alive_at(&self, at: DocumentPoint) -> bool {
        match at {
            DocumentPoint::Current => self.retired_at.is_none(),
            DocumentPoint::Seq(seq) => {
                self.created_at <= seq && self.retired_at.is_none_or(|retired| seq < retired)
            }
        }
    }
    pub fn current_only(&self) -> bool {
        !matches!(self.scope, DocumentScope::Conversation { .. })
            || self.history != Some(DocumentHistory::Rewindable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCreate {
    pub id: DocumentId,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: DocumentScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
}
impl DocumentCreate {
    pub fn address(&self) -> DocumentAddress {
        DocumentAddress {
            kind: self.kind.clone(),
            scope: self.scope,
            key: self.key.clone(),
        }
    }
    pub fn stamp(self, created_at: Seq, retired_at: Option<Seq>) -> DocumentRecord {
        DocumentRecord {
            id: self.id,
            kind: self.kind,
            key: self.key,
            scope: self.scope,
            history: self.history,
            fork: self.fork,
            created_at,
            retired_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanOrder {
    Ascending,
    Descending,
}
impl ScanOrder {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_conversation_id: Option<ConversationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryQuery {
    pub conversation_id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_entry_id: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_entry_id: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_requested: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SubmissionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PointWire", into = "PointWire")]
pub enum DocumentPoint {
    Current,
    Seq(Seq),
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
enum PointWire {
    Current(CurrentWire),
    Seq(Seq),
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum CurrentWire {
    #[serde(rename = "current")]
    Current,
}
impl From<PointWire> for DocumentPoint {
    fn from(value: PointWire) -> Self {
        match value {
            PointWire::Current(_) => Self::Current,
            PointWire::Seq(seq) => Self::Seq(seq),
        }
    }
}
impl From<DocumentPoint> for PointWire {
    fn from(value: DocumentPoint) -> Self {
        match value {
            DocumentPoint::Current => Self::Current(CurrentWire::Current),
            DocumentPoint::Seq(seq) => Self::Seq(seq),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentAddress {
    pub kind: String,
    pub scope: DocumentScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentQuery {
    pub scope: DocumentScope,
    pub at: DocumentPoint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocumentContent {
    Base {
        version: u64,
        value: JsonObject,
    },
    Delta {
        version: u64,
        #[serde(with = "ops_wire")]
        ops: Vec<Op>,
    },
}
impl DocumentContent {
    pub const fn version(&self) -> u64 {
        match self {
            Self::Base { version, .. } | Self::Delta { version, .. } => *version,
        }
    }
}

pub(crate) mod ops_wire {
    use super::*;
    use pi_rs_chord::json::JsonValue;
    use serde::ser::SerializeSeq;
    pub(crate) fn serialize<S: serde::Serializer>(
        ops: &[Op],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(ops.len()))?;
        for op in ops {
            seq.serialize_element(&op.to_json().to_serde().map_err(serde::ser::Error::custom)?)?;
        }
        seq.end()
    }
    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Op>, D::Error> {
        Vec::<Value>::deserialize(deserializer)?
            .into_iter()
            .map(|op| Op::from_json(&JsonValue::from_serde(op)).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCopySource {
    pub id: DocumentId,
    pub at: DocumentPoint,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDocument {
    pub record: DocumentRecord,
    pub version: u64,
    pub value: JsonObject,
    pub deltas_since_base: usize,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Move task records through commit batches without allocating a box per task write"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StorageWrite {
    Conversation {
        value: ConversationRecord,
    },
    Entry {
        value: EntryRecord,
    },
    Task {
        value: TaskRecord,
    },
    Submission {
        value: SubmissionRecord,
    },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentCreate,
        content: DocumentContent,
    },
    #[serde(rename = "document.copy")]
    DocumentCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    #[serde(rename = "document.change")]
    DocumentChange {
        id: DocumentId,
        content: DocumentContent,
    },
    #[serde(rename = "document.retire")]
    DocumentRetire {
        id: DocumentId,
    },
}

#[expect(
    clippy::large_enum_variant,
    reason = "Publications own records and share only the whole commit, without a second allocation per task"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CommitChange {
    Conversation {
        value: ConversationRecord,
    },
    Entry {
        value: EntryRecord,
    },
    Task {
        value: TaskRecord,
    },
    Submission {
        value: SubmissionRecord,
    },
    Document(DocumentCommitChange),
    #[serde(rename = "document.copy")]
    DocumentCopy {
        record: DocumentRecord,
        conversation_id: ConversationId,
        source: DocumentCopySource,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DocumentCommitChange {
    Change {
        record: DocumentRecord,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation_id: Option<ConversationId>,
        version: u64,
        value: JsonObject,
        #[serde(with = "ops_wire")]
        ops: Vec<Op>,
    },
    Retire {
        record: DocumentRecord,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation_id: Option<ConversationId>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitPublication {
    pub seq: Seq,
    pub changes: Vec<CommitChange>,
}

#[derive(Debug)]
pub enum CommitResult {
    Stored {
        stored: usize,
    },
    Refused {
        stored: usize,
        refused: StorageError,
    },
}
impl CommitResult {
    pub const fn stored(&self) -> usize {
        match self {
            Self::Stored { stored } | Self::Refused { stored, .. } => *stored,
        }
    }
}
