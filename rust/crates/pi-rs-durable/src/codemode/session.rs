use async_trait::async_trait;
use pi_rs_chord::{
    context::Context,
    delta::{Change, Segment},
    json::JsonValue,
};
use pi_rs_codemode::StoreWrites;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    documents::DocToken,
    ids::{ConversationId, TaskId},
    session::{Error, Result, Session, Transaction},
    storage_types::{DocumentAddress, DocumentScope},
};

use super::{
    CodemodeDocuments, CodemodeError, CodemodeJournalState, CodemodeJournalUpdate,
    CodemodeSnapshot, CodemodeStart, CodemodeStoreState, CodemodeToolResult, codemode_journal_doc,
    codemode_run_doc, codemode_store_doc,
};

struct Document {
    token: DocToken,
    address: DocumentAddress,
}

impl Document {
    fn new(token: DocToken, scope: DocumentScope) -> Self {
        let address = DocumentAddress {
            kind: token.kind.clone(),
            scope,
            key: None,
        };
        Self { token, address }
    }

    async fn open<'tx>(&self, tx: &'tx mut Transaction<'_>) -> Result<&'tx mut Change> {
        tx.doc(&self.token, &self.address, None).await
    }
}

/// Binds one codemode invocation to real Session documents and commits. Nested
/// tool scheduling is supplied separately, through `CodemodeTools`.
pub struct SessionCodemodeDocuments {
    session: Session,
    conversation: ConversationId,
    task: TaskId,
    run: Document,
    journal: Document,
    store: Document,
}

impl SessionCodemodeDocuments {
    pub fn new(session: Session, conversation: ConversationId, task: TaskId) -> Result<Self> {
        let scope = DocumentScope::Task { task_id: task };
        Ok(Self {
            session,
            conversation,
            task,
            run: Document::new(codemode_run_doc()?, scope),
            journal: Document::new(codemode_journal_doc()?, scope),
            store: Document::new(
                codemode_store_doc()?,
                DocumentScope::Conversation {
                    conversation_id: conversation,
                },
            ),
        })
    }

    async fn check_binding(&self, tx: &mut Transaction<'_>) -> Result<()> {
        let record = tx
            .task(self.task)
            .await?
            .ok_or_else(|| Error::invalid(format!("Task {} does not exist", self.task)))?;
        if record.conversation_id != self.conversation {
            return Err(Error::invalid(format!(
                "Task {} does not belong to conversation {}",
                self.task, self.conversation
            )));
        }
        Ok(())
    }
}

fn key(name: &str) -> Segment {
    Segment::Key(name.into())
}

fn optional<T: DeserializeOwned>(doc: &Change, path: &[Segment]) -> Result<Option<T>> {
    doc.value()?
        .at(path)
        .ok()
        .map(|value| Ok(serde_json::from_value(value.to_json().to_serde()?)?))
        .transpose()
}

fn whole<T: DeserializeOwned>(doc: &Change) -> Result<T> {
    Ok(serde_json::from_value(doc.value()?.to_json().to_serde()?)?)
}

fn set(doc: &mut Change, path: &[Segment], value: impl Serialize) -> Result<()> {
    doc.set(path, JsonValue::from_serde(serde_json::to_value(value)?))?;
    Ok(())
}

fn documents(error: Error) -> CodemodeError {
    match error {
        Error::Cancelled(error) => CodemodeError::Aborted(error),
        error => CodemodeError::Documents(error.to_string()),
    }
}

#[async_trait]
impl CodemodeDocuments for SessionCodemodeDocuments {
    async fn begin(
        &self,
        mut fresh: CodemodeStart,
        context: &Context,
    ) -> std::result::Result<CodemodeSnapshot, CodemodeError> {
        self.session
            .commit(
                async |tx| {
                    self.check_binding(tx).await?;
                    let run = self.run.open(tx).await?;
                    if let Some(result) = optional(run, &[key("result")])? {
                        return Ok(CodemodeSnapshot::Finished(result));
                    }
                    let saved = optional(run, &[key("start")])?;
                    let rerun = saved.is_some();
                    let start = if let Some(saved) = saved {
                        saved
                    } else {
                        let store: CodemodeStoreState = whole(self.store.open(tx).await?)?;
                        fresh.store = store.values;
                        set(self.run.open(tx).await?, &[key("start")], &fresh)?;
                        fresh
                    };
                    let journal: CodemodeJournalState = whole(self.journal.open(tx).await?)?;
                    Ok(CodemodeSnapshot::Started {
                        start,
                        journal,
                        rerun,
                    })
                },
                context,
            )
            .await
            .map_err(documents)
    }

    async fn record_fingerprint(
        &self,
        fingerprint: String,
        context: &Context,
    ) -> std::result::Result<(), CodemodeError> {
        self.session
            .commit(
                async |tx| {
                    self.check_binding(tx).await?;
                    let run = self.run.open(tx).await?;
                    let start = key("start");
                    run.value()?.at(std::slice::from_ref(&start))?;
                    let path = [start, key("fingerprint")];
                    match optional::<String>(run, &path)? {
                        Some(saved) if saved != fingerprint => Err(Error::invalid(
                            "Codemode start already has a different fingerprint",
                        )),
                        Some(_) => Ok(()),
                        None => set(run, &path, fingerprint),
                    }
                },
                context,
            )
            .await
            .map_err(documents)
    }

    async fn update_journal(
        &self,
        update: CodemodeJournalUpdate,
        context: &Context,
    ) -> std::result::Result<(), CodemodeError> {
        self.session
            .commit(
                async |tx| {
                    self.check_binding(tx).await?;
                    let journal = self.journal.open(tx).await?;
                    let entries = key("entries");
                    let count = journal
                        .value()?
                        .at(std::slice::from_ref(&entries))?
                        .elements()
                        .ok_or_else(|| Error::invalid("Codemode journal entries are not an array"))?
                        .len();
                    if let Some(ack) = update.acknowledged {
                        let first = [key("firstIssued")];
                        let delivered;
                        let path: &[Segment] = if ack.seq == 0 {
                            &first
                        } else {
                            let index = usize::try_from(ack.seq - 1)
                                .ok()
                                .filter(|&index| index < count)
                                .ok_or_else(|| {
                                    Error::invalid(format!(
                                        "Acknowledgment for unknown journal result {}",
                                        ack.seq
                                    ))
                                })?;
                            delivered =
                                [entries.clone(), Segment::Index(index), key("issuedAfter")];
                            &delivered
                        };
                        match optional::<u64>(journal, path)? {
                            Some(issued) if issued != ack.issued => {
                                return Err(Error::invalid(format!(
                                    "Conflicting issued count for journal result {}",
                                    ack.seq
                                )));
                            }
                            Some(_) => {}
                            None => set(journal, path, ack.issued)?,
                        }
                    }
                    if !update.delivered.is_empty() {
                        let new = update
                            .delivered
                            .into_iter()
                            .map(|entry| serde_json::to_value(entry).map(JsonValue::from_serde))
                            .collect::<std::result::Result<_, _>>()?;
                        journal.splice(&[entries], count, 0, new)?;
                    }
                    Ok(())
                },
                context,
            )
            .await
            .map_err(documents)
    }

    async fn output_dir(
        &self,
        created: Option<String>,
        context: &Context,
    ) -> std::result::Result<Option<String>, CodemodeError> {
        self.session
            .commit(
                async |tx| {
                    self.check_binding(tx).await?;
                    let run = self.run.open(tx).await?;
                    let path = [key("outputDir")];
                    if let Some(saved) = optional(run, &path)? {
                        return Ok(Some(saved));
                    }
                    if let Some(created) = &created {
                        set(run, &path, created)?;
                    }
                    Ok(created)
                },
                context,
            )
            .await
            .map_err(documents)
    }

    async fn finish(
        &self,
        result: CodemodeToolResult,
        writes: Option<StoreWrites>,
        context: &Context,
    ) -> std::result::Result<CodemodeToolResult, CodemodeError> {
        self.session
            .commit(
                async |tx| {
                    self.check_binding(tx).await?;
                    let run = self.run.open(tx).await?;
                    let path = [key("result")];
                    if let Some(saved) = optional(run, &path)? {
                        return Ok(saved);
                    }
                    set(run, &path, &result)?;
                    if !result.is_error
                        && let Some(writes) = writes
                        && (!writes.delete.is_empty() || !writes.set.is_empty())
                    {
                        let store = self.store.open(tx).await?;
                        let mut path = [key("values"), key("")];
                        for name in writes.delete {
                            path[1] = Segment::Key(name.into());
                            if store.value()?.at(&path).is_ok() {
                                store.delete(&path)?;
                            }
                        }
                        for (name, value) in writes.set {
                            path[1] = Segment::Key(name.into());
                            store.set(&path, JsonValue::from_serde(value))?;
                        }
                    }
                    Ok(result)
                },
                context,
            )
            .await
            .map_err(documents)
    }
}
