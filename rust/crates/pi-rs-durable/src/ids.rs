//! Numeric identities in the durable wire formats. Every backend allocates from one ID space.

pub type Id = u64;
pub type ConversationId = Id;
pub type EntryId = Id;
pub type TaskId = Id;
pub type SubmissionId = Id;
pub type DocumentId = Id;
pub type Seq = u64;

pub const ROOT_CONVERSATION_ID: ConversationId = 1;
/// Highest integer which can round-trip through the upstream JavaScript storage API.
pub const MAX_SAFE_ID: Id = (1 << 53) - 1;

/// Apply an ID alias to a number already validated by the allocator or decoder.
pub const fn id_from_number(value: u64) -> Id {
    value
}

/// Apply the sequence alias at a trusted storage boundary.
pub const fn seq_from_number(value: u64) -> Seq {
    value
}
