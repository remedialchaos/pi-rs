//! Logical addresses, definition callbacks, and typed access to persisted documents.

use std::sync::Arc;

use pi_rs_chord::delta::Op;
use serde_json::Value;

use crate::errors::BoxError;
use crate::ids::{DocumentId, Id, MAX_SAFE_ID};
use crate::storage_types::{
    DocumentAddress, DocumentCreate, DocumentFork, DocumentHistory, DocumentRecord, DocumentScope,
    JsonObject, StoredDocument,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatestFork {
    Current,
    Initial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentSemantics {
    Session,
    Task,
    LatestConversation { fork: LatestFork },
    RewindableConversation { fork: DocumentFork },
}

impl DocumentSemantics {
    fn matches_scope(self, scope: &DocumentScope) -> bool {
        match self {
            Self::Session => matches!(scope, DocumentScope::Session),
            Self::Task => matches!(scope, DocumentScope::Task { .. }),
            Self::LatestConversation { .. } | Self::RewindableConversation { .. } => {
                matches!(scope, DocumentScope::Conversation { .. })
            }
        }
    }

    pub const fn history_and_fork(self) -> (Option<DocumentHistory>, Option<DocumentFork>) {
        match self {
            Self::Session | Self::Task => (None, None),
            Self::LatestConversation { fork } => (
                Some(DocumentHistory::Latest),
                Some(match fork {
                    LatestFork::Current => DocumentFork::Current,
                    LatestFork::Initial => DocumentFork::Initial,
                }),
            ),
            Self::RewindableConversation { fork } => {
                (Some(DocumentHistory::Rewindable), Some(fork))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointInfo {
    pub deltas_since_base: usize,
}

type FamilyInitial = Arc<dyn Fn(&Value) -> Result<JsonObject, BoxError> + Send + Sync>;
pub enum InitialDocument {
    Singleton(Arc<dyn Fn() -> Result<JsonObject, BoxError> + Send + Sync>),
    Family(FamilyInitial),
}

pub type DocumentMigration =
    Arc<dyn Fn(JsonObject, u64) -> Result<JsonObject, BoxError> + Send + Sync>;
pub type CheckpointPolicy = Arc<dyn Fn(&JsonObject, &[Op], CheckpointInfo) -> bool + Send + Sync>;

pub struct DocDefinition {
    pub kind: String,
    pub version: u64,
    pub semantics: DocumentSemantics,
    pub initial: InitialDocument,
    pub migrate: Option<DocumentMigration>,
    pub checkpoint_when: CheckpointPolicy,
}

impl DocDefinition {
    pub const fn is_family(&self) -> bool {
        matches!(self.initial, InitialDocument::Family(_))
    }

    pub fn initial(&self, seed: Option<&Value>) -> Result<JsonObject, DocumentError> {
        let value = match &self.initial {
            InitialDocument::Singleton(initial) => initial(),
            InitialDocument::Family(initial) => {
                initial(seed.ok_or_else(|| DocumentError::MissingSeed(self.kind.clone()))?)
            }
        };
        value.map_err(DocumentError::Callback)
    }
}

pub type DocToken = Arc<DocDefinition>;
pub type DocFamilyToken = Arc<DocDefinition>;

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("Document {0} version must be a positive integer")]
    Version(String),
    #[error("Document {0} was defined with the wrong initializer")]
    Initializer(String),
    #[error("Document family {0} requires an initial seed")]
    MissingSeed(String),
    #[error("Document {kind} requires a {scope} ID")]
    Owner { kind: String, scope: &'static str },
    #[error("Document {0} requires an address matching its definition scope and family")]
    Address(String),
    #[error("Document {id} ({kind}) does not match the supplied definition semantics")]
    Semantics { id: DocumentId, kind: String },
    #[error("Document {id} ({kind}) has newer version {stored} than {defined}")]
    Newer {
        id: DocumentId,
        kind: String,
        stored: u64,
        defined: u64,
    },
    #[error("Document {id} ({kind}) requires migration from version {stored}")]
    NeedsMigration {
        id: DocumentId,
        kind: String,
        stored: u64,
    },
    #[error(transparent)]
    Callback(#[from] BoxError),
}

pub fn define_doc(definition: DocDefinition) -> Result<DocToken, DocumentError> {
    define(definition, false)
}

pub fn define_doc_family(definition: DocDefinition) -> Result<DocFamilyToken, DocumentError> {
    define(definition, true)
}

fn define(definition: DocDefinition, family: bool) -> Result<Arc<DocDefinition>, DocumentError> {
    if !(1..=MAX_SAFE_ID).contains(&definition.version) {
        return Err(DocumentError::Version(definition.kind));
    }
    if definition.is_family() != family {
        return Err(DocumentError::Initializer(definition.kind));
    }
    Ok(Arc::new(definition))
}

pub fn resolve_address(
    definition: &DocDefinition,
    owner: Option<Id>,
    key: Option<String>,
) -> Result<DocumentAddress, DocumentError> {
    let owner = owner.filter(|id| *id <= MAX_SAFE_ID);
    let required_owner = |scope| {
        owner.ok_or_else(|| DocumentError::Owner {
            kind: definition.kind.clone(),
            scope,
        })
    };
    let scope = match definition.semantics {
        DocumentSemantics::Session => DocumentScope::Session,
        DocumentSemantics::Task => DocumentScope::Task {
            task_id: required_owner("task")?,
        },
        DocumentSemantics::LatestConversation { .. }
        | DocumentSemantics::RewindableConversation { .. } => DocumentScope::Conversation {
            conversation_id: required_owner("conversation")?,
        },
    };
    if key.is_some() != definition.is_family() {
        return Err(DocumentError::Address(definition.kind.clone()));
    }
    Ok(DocumentAddress {
        kind: definition.kind.clone(),
        scope,
        key,
    })
}

/// Unambiguous identity even when the definition kind or family member contains separators.
pub fn address_id(address: &DocumentAddress) -> String {
    let (scope, owner) = match address.scope {
        DocumentScope::Session => ("session", None),
        DocumentScope::Conversation { conversation_id } => ("conversation", Some(conversation_id)),
        DocumentScope::Task { task_id } => ("task", Some(task_id)),
    };
    serde_json::json!([address.kind, scope, owner, address.key]).to_string()
}

pub fn document_create(
    definition: &DocDefinition,
    address: &DocumentAddress,
    id: DocumentId,
) -> Result<DocumentCreate, DocumentError> {
    if address.kind != definition.kind
        || !definition.semantics.matches_scope(&address.scope)
        || address.key.is_some() != definition.is_family()
    {
        return Err(DocumentError::Address(definition.kind.clone()));
    }
    let (history, fork) = definition.semantics.history_and_fork();
    Ok(DocumentCreate {
        id,
        kind: address.kind.clone(),
        scope: address.scope,
        key: address.key.clone(),
        history,
        fork,
    })
}

pub trait DocumentIdentity {
    fn document_id(&self) -> DocumentId;
    fn document_kind(&self) -> &str;
    fn document_scope(&self) -> &DocumentScope;
    fn history_and_fork(&self) -> (Option<DocumentHistory>, Option<DocumentFork>);
}

macro_rules! impl_identity {
    ($($record:ty),+) => {$(
        impl DocumentIdentity for $record {
            fn document_id(&self) -> DocumentId { self.id }
            fn document_kind(&self) -> &str { &self.kind }
            fn document_scope(&self) -> &DocumentScope { &self.scope }
            fn history_and_fork(&self) -> (Option<DocumentHistory>, Option<DocumentFork>) { (self.history, self.fork) }
        }
    )+};
}
impl_identity!(DocumentCreate, DocumentRecord);

pub fn check_record_scope(
    definition: &DocDefinition,
    record: &impl DocumentIdentity,
) -> Result<(), DocumentError> {
    if !definition.semantics.matches_scope(record.document_scope())
        || definition.semantics.history_and_fork() != record.history_and_fork()
    {
        return Err(DocumentError::Semantics {
            id: record.document_id(),
            kind: record.document_kind().to_owned(),
        });
    }
    Ok(())
}

pub fn check_record_version(
    definition: &DocDefinition,
    record: &impl DocumentIdentity,
    version: u64,
) -> Result<(), DocumentError> {
    if version > definition.version {
        return Err(DocumentError::Newer {
            id: record.document_id(),
            kind: record.document_kind().to_owned(),
            stored: version,
            defined: definition.version,
        });
    }
    if version < definition.version && definition.migrate.is_none() {
        return Err(DocumentError::NeedsMigration {
            id: record.document_id(),
            kind: record.document_kind().to_owned(),
            stored: version,
        });
    }
    Ok(())
}

pub fn materialize_document(
    definition: &DocDefinition,
    stored: StoredDocument,
) -> Result<JsonObject, DocumentError> {
    materialize_document_value(definition, &stored.record, stored.version, stored.value)
}

pub fn materialize_document_value(
    definition: &DocDefinition,
    record: &impl DocumentIdentity,
    version: u64,
    value: JsonObject,
) -> Result<JsonObject, DocumentError> {
    check_record_scope(definition, record)?;
    check_record_version(definition, record, version)?;
    match &definition.migrate {
        Some(migrate) if version < definition.version => Ok(migrate(value, version)?),
        _ => Ok(value),
    }
}
