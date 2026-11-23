use std::collections::HashSet;

use crate::{documents::address_id, ids::*, storage_types::*};

use super::{Error, Inner, Result};

pub(crate) struct ForkDocuments {
    pub copies: Vec<(DocumentCreate, DocumentCopySource)>,
    pub source_ids: HashSet<DocumentId>,
}

/// Select as-of documents from the conversation that owns the cutoff entry, and current
/// documents from the parent itself. The source need not have a registered definition.
pub(crate) async fn prepare_fork_document_copies(
    session: &Inner,
    parent: ConversationId,
    at: EntryId,
    child: ConversationId,
) -> Result<ForkDocuments> {
    let Some(found) = session.visible_entry(parent, at).await? else {
        return Err(Error::invalid(format!(
            "Entry {at} is not visible from conversation {parent}"
        )));
    };
    let mut result = ForkDocuments {
        copies: Vec::new(),
        source_ids: HashSet::new(),
    };
    let mut addresses = HashSet::new();
    for (source_conversation, at, policy) in [
        (
            found.entry.conversation_id,
            DocumentPoint::Seq(found.commit_seq),
            DocumentFork::AsOf,
        ),
        (parent, DocumentPoint::Current, DocumentFork::Current),
    ] {
        let query = DocumentQuery {
            scope: DocumentScope::Conversation {
                conversation_id: source_conversation,
            },
            at,
            kind: None,
        };
        let mut cursor = None;
        loop {
            let page = session.scan_documents(&query, 256, cursor.as_ref()).await?;
            for source in page.items {
                if !matches!(source.scope, DocumentScope::Conversation { .. })
                    || source.fork != Some(policy)
                {
                    continue;
                }
                session.touch(&super::Key::Incarnation(source.id)).await?;
                let address = DocumentAddress {
                    scope: DocumentScope::Conversation {
                        conversation_id: child,
                    },
                    kind: source.kind,
                    key: source.key,
                };
                if !addresses.insert(address_id(&address)) {
                    let mut member = address.kind;
                    if let Some(key) = address.key {
                        member.push('/');
                        member.push_str(&key);
                    }
                    return Err(Error::invalid(format!(
                        "Fork selects multiple source documents for {member}"
                    )));
                }
                let record = DocumentCreate {
                    id: session.mint_id()?,
                    scope: address.scope,
                    kind: address.kind,
                    key: address.key,
                    history: source.history,
                    fork: source.fork,
                };
                result.source_ids.insert(source.id);
                result
                    .copies
                    .push((record, DocumentCopySource { id: source.id, at }));
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(result)
}
