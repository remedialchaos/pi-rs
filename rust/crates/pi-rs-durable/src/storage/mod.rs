pub mod jsonl;
pub mod memory;
pub mod scan;
pub mod sqlite;
mod sqlite_schema;
pub mod store;

use crate::ids::*;
pub use crate::storage_types::*;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{0}")]
    Request(String),
    #[error("{0}")]
    Backend(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    JsonlCorruption(#[from] jsonl::JsonlCorruptionError),
    #[error(transparent)]
    JsonlReadOnly(#[from] jsonl::JsonlStorageReadOnlyError),
    #[error(transparent)]
    JsonlPoisoned(#[from] jsonl::JsonlStoragePoisonedError),
}

pub type StorageResult<T> = Result<T, StorageError>;

/// Persistence boundary. Receipts follow durable writes and precede the next commit.
pub trait Storage: Send + Sync {
    fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut dyn FnMut(usize, Seq),
    ) -> StorageResult<CommitResult>;
    fn publishing(&self, publish: &mut dyn FnMut()) {
        publish();
    }
    fn mint_id(&self) -> StorageResult<u64>;
    fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>>;
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>>;
    fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>>;
    fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>>;
    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>>;
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>>;
    fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>>;
    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>>;
    fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>>;
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>>;
    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> StorageResult<Option<SubmissionRecord>>;
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> StorageResult<Option<DocumentRecord>>;
    fn document(&self, id: DocumentId, at: DocumentPoint) -> StorageResult<Option<StoredDocument>>;
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>>;
    fn close(&self) -> StorageResult<()>;
}
