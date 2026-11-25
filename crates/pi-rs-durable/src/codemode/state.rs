//! Wire documents and the atomic operations required of the session adapter.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use pi_rs_chord::context::Context;
use pi_rs_codemode::{Outcome, StoreWrites};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{CodemodeError, CodemodeToolResult};
use crate::{
    documents::{
        DocDefinition, DocToken, DocumentError, DocumentSemantics, InitialDocument, define_doc,
    },
    storage_types::{DocumentFork, JsonObject},
};

pub const CODEMODE_RUN_DOC: &str = "pi.codemode.run";
pub const CODEMODE_JOURNAL_DOC: &str = "pi.codemode.journal";
pub const CODEMODE_STORE_DOC: &str = "pi.codemode.store";
pub const CODEMODE_DOCUMENT_VERSION: u64 = 1;
pub const CODEMODE_STORE_CHECKPOINT_DELTAS: usize = 64;

fn is_false(value: &bool) -> bool {
    !*value
}

// A present JSON null is different from an omitted JavaScript value.
pub(super) fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeDeclaredTool {
    pub name: String,
    pub js_name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub structured: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeDeclaredNamespace {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeGlobalSpec {
    pub name: String,
    pub spread: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStart {
    pub version: u32,
    pub started_at: u64,
    pub seed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    pub tools: Vec<CodemodeDeclaredTool>,
    pub namespaces: Vec<CodemodeDeclaredNamespace>,
    pub globals: Vec<CodemodeGlobalSpec>,
    pub max_output_tokens: u64,
    pub store: IndexMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeRunState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<CodemodeStart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CodemodeToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreState {
    pub values: IndexMap<String, Value>,
}

impl CodemodeStoreState {
    pub fn apply(&mut self, writes: StoreWrites) {
        for key in writes.delete {
            self.values.shift_remove(&key);
        }
        self.values.extend(writes.set);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeOutcome {
    pub ok: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Value>,
}

impl CodemodeOutcome {
    pub(super) fn success(value: Option<Value>) -> Self {
        Self {
            ok: true,
            value,
            message: None,
            properties: None,
        }
    }

    pub(super) fn failure(message: String, properties: Option<Value>) -> Self {
        Self {
            ok: false,
            value: None,
            message: Some(message),
            properties,
        }
    }
}

impl From<CodemodeOutcome> for Outcome {
    fn from(outcome: CodemodeOutcome) -> Self {
        if outcome.ok {
            Self::Ok(outcome.value)
        } else {
            Self::Error {
                message: outcome.message.unwrap_or_default(),
                properties: outcome.properties,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeJournalGlobal {
    pub name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub args: Option<Value>,
    pub outcome: CodemodeOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournalEntry {
    pub id: u32,
    pub at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global: Option<CodemodeJournalGlobal>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournalState {
    pub entries: Vec<CodemodeJournalEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_issued: Option<u64>,
}

#[derive(Debug, Default)]
pub struct CodemodeJournalUpdate {
    /// Drain sequence 0 is the initial script execution; other sequences are one-based journal positions.
    pub acknowledged: Option<pi_rs_codemode::During>,
    pub delivered: Vec<CodemodeJournalEntry>,
}

impl CodemodeJournalState {
    /// Record an acknowledgment only once and append new results in delivery order.
    pub fn apply(&mut self, update: CodemodeJournalUpdate) -> Result<(), CodemodeError> {
        if let Some(ack) = update.acknowledged {
            let slot = if ack.seq == 0 {
                &mut self.first_issued
            } else {
                let index = usize::try_from(ack.seq - 1)
                    .map_err(|error| CodemodeError::Documents(error.to_string()))?;
                &mut self
                    .entries
                    .get_mut(index)
                    .ok_or_else(|| {
                        CodemodeError::Documents(format!(
                            "Acknowledgment for unknown journal result {}",
                            ack.seq
                        ))
                    })?
                    .issued_after
            };
            if slot.is_some_and(|recorded| recorded != ack.issued) {
                return Err(CodemodeError::Documents(format!(
                    "Conflicting issued count for journal result {}",
                    ack.seq
                )));
            }
            *slot = Some(ack.issued);
        }
        self.entries.extend(update.delivered);
        Ok(())
    }
}

pub enum CodemodeSnapshot {
    Finished(CodemodeToolResult),
    Started {
        start: CodemodeStart,
        journal: CodemodeJournalState,
        rerun: bool,
    },
}

/// Scoped to one codemode task and its conversation. Each operation is an atomic durable transaction.
/// The run and journal documents retire with the task; the store is rewindable and forks as of the fork point.
#[async_trait]
pub trait CodemodeDocuments: Send + Sync {
    /// Return any already decided result, or initialize the run once by filling `fresh.store`
    /// from the conversation store and persisting it together with the initial journal state.
    async fn begin(
        &self,
        fresh: CodemodeStart,
        context: &Context,
    ) -> Result<CodemodeSnapshot, CodemodeError>;

    /// Record the first VM fingerprint before any host call is allowed to run.
    async fn record_fingerprint(
        &self,
        fingerprint: String,
        context: &Context,
    ) -> Result<(), CodemodeError>;

    /// Apply `update` before delivering any new results or admitting covered calls.
    async fn update_journal(
        &self,
        update: CodemodeJournalUpdate,
        context: &Context,
    ) -> Result<(), CodemodeError>;

    /// Read the saved path; if absent, atomically adopt `created` (if any) before writing files.
    async fn output_dir(
        &self,
        created: Option<String>,
        context: &Context,
    ) -> Result<Option<String>, CodemodeError>;

    /// First outcome wins. Store it and successful writes in the same transaction, then return the saved outcome.
    async fn finish(
        &self,
        result: CodemodeToolResult,
        writes: Option<StoreWrites>,
        context: &Context,
    ) -> Result<CodemodeToolResult, CodemodeError>;
}

fn document(
    kind: &str,
    semantics: DocumentSemantics,
    initial: fn() -> JsonObject,
    interval: Option<usize>,
) -> Result<DocToken, DocumentError> {
    define_doc(DocDefinition {
        kind: kind.into(),
        version: CODEMODE_DOCUMENT_VERSION,
        semantics,
        initial: InitialDocument::Singleton(Arc::new(move || Ok(initial()))),
        migrate: None,
        checkpoint_when: Arc::new(move |_, _, info| {
            interval.is_some_and(|threshold| info.deltas_since_base >= threshold)
        }),
    })
}

pub fn codemode_run_doc() -> Result<DocToken, DocumentError> {
    document(
        CODEMODE_RUN_DOC,
        DocumentSemantics::Task,
        JsonObject::new,
        None,
    )
}

pub fn codemode_journal_doc() -> Result<DocToken, DocumentError> {
    document(
        CODEMODE_JOURNAL_DOC,
        DocumentSemantics::Task,
        || {
            [("entries".into(), Value::Array(Vec::new()))]
                .into_iter()
                .collect()
        },
        None,
    )
}

pub fn codemode_store_doc() -> Result<DocToken, DocumentError> {
    document(
        CODEMODE_STORE_DOC,
        DocumentSemantics::RewindableConversation {
            fork: DocumentFork::AsOf,
        },
        || {
            [("values".into(), Value::Object(JsonObject::new()))]
                .into_iter()
                .collect()
        },
        Some(CODEMODE_STORE_CHECKPOINT_DELTAS),
    )
}
