use std::sync::Arc;

use crate::{ids::*, storage_types::*};

use super::cached_storage::Lru;

/// A visible entry whose head identifies the start of the active context.
pub type HeadMarker = Arc<EntryRecord>;

#[derive(Default)]
pub(crate) struct ConversationTails {
    latest: Lru<Option<EntryId>, 4096>,
    markers: Lru<Option<HeadMarker>, 256>,
}

impl ConversationTails {
    pub fn peek(&self, conversation: ConversationId) -> Option<Option<EntryId>> {
        self.latest.peek(conversation).copied()
    }

    pub fn latest(&mut self, conversation: ConversationId) -> Option<Option<EntryId>> {
        self.latest.get(conversation).copied()
    }

    pub fn keep_latest(&mut self, conversation: ConversationId, entry: Option<EntryId>) {
        self.latest.put(conversation, entry);
    }

    pub fn marker(
        &mut self,
        conversation: ConversationId,
        cutoff: Option<EntryId>,
    ) -> Option<Option<HeadMarker>> {
        let marker = self.markers.get(conversation)?;
        if marker
            .as_ref()
            .is_some_and(|entry| cutoff.is_some_and(|at| at < entry.id))
        {
            return None;
        }
        Some(marker.clone())
    }

    pub fn keep_marker(
        &mut self,
        conversation: ConversationId,
        cutoff: Option<EntryId>,
        marker: Option<HeadMarker>,
    ) {
        if cutoff.is_none_or(|at| {
            self.peek(conversation)
                .is_some_and(|id| id.is_none_or(|id| id <= at))
        }) {
            self.markers.put(conversation, marker);
        }
    }

    pub fn apply(&mut self, writes: &[StorageWrite]) {
        for write in writes {
            match write {
                StorageWrite::Conversation { value } if value.parent.is_none() => {
                    self.latest.put(value.id, None);
                    self.markers.put(value.id, None);
                }
                StorageWrite::Entry { value } => {
                    self.latest.put(value.conversation_id, Some(value.id));
                    if value.head.is_some() {
                        self.markers
                            .put(value.conversation_id, Some(Arc::new(value.clone())));
                    }
                }
                _ => {}
            }
        }
    }

    pub fn len(&self) -> usize {
        self.latest.len()
    }
}
