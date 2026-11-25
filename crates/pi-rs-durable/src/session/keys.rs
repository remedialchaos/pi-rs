use crate::{ids::*, storage_types::DocumentScope, storage_types::StorageWrite};

/// A record or index that a mutation may change and a read may depend on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    All,
    Conversation(ConversationId),
    Entry(EntryId),
    Append(ConversationId),
    Task(TaskId),
    Submission(SubmissionId),
    Request(ConversationId, String),
    Document(String),
    Incarnation(DocumentId),
    Scope(DocumentScope),
    OwnedTask(TaskId),
    OwnedConversation(ConversationId),
    Roots,
    Intent,
}

pub(crate) fn table_keys(writes: &[StorageWrite], keys: &mut Vec<Key>) {
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => keys.push(Key::Conversation(value.id)),
            StorageWrite::Entry { value } => {
                keys.push(Key::Entry(value.id));
                keys.push(Key::Append(value.conversation_id));
            }
            StorageWrite::Task { value } => keys.push(Key::Task(value.id)),
            StorageWrite::Submission { value } => {
                keys.push(Key::Submission(value.id));
                if let Some(request) = &value.request_id {
                    keys.push(Key::Request(value.conversation_id, request.clone()));
                }
            }
            _ => {}
        }
    }
}
