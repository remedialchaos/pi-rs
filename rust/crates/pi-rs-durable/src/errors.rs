//! Errors whose messages form part of the durable session contract.

use crate::ids::ConversationId;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub fn error_message(error: &(impl std::fmt::Display + ?Sized)) -> String {
    error.to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0} cannot read tables after the first table write")]
    ReadAfterWrite(&'static str),
    #[error("Read-only Harness: {0} refused")]
    ReadOnly(String),
    #[error("Session failed after a storage error; close and reopen it")]
    Failed(#[source] BoxError),
    #[error("Conversation {0} is busy")]
    ConversationBusy(ConversationId),
    #[error("Conversation {0} is idle")]
    ConversationIdle(ConversationId),
}
