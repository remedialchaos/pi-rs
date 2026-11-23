use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    store::{DocumentBase, DocumentContent as EngineContent, Step, StoreWrite},
};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{
    ids::*,
    storage::{CommitResult, StorageError},
    storage_types::{
        DocumentContent, DocumentCopySource, DocumentCreate, DocumentPoint, DocumentScope,
        StorageWrite,
    },
};

use super::{
    checked_id, documents, number,
    tables::{Table, Tables, encode_indexed},
    unsigned,
};

const MAX: i64 = MAX_SAFE_ID as i64;

pub(super) fn raise(c: &Connection, highest: i64) -> Result<()> {
    c.prepare_cached("UPDATE chord_meta SET next_id=max(next_id,?) WHERE singleton=1")?
        .execute([highest.saturating_add(1).min(MAX)])?;
    Ok(())
}

pub(super) fn mint(c: &Connection) -> Result<Id> {
    let next: i64 = c
        .prepare_cached("SELECT next_id FROM chord_meta WHERE singleton=1")?
        .query_row([], |r| r.get(0))?;
    if next >= MAX {
        return Err(Error::Invalid("ID space is exhausted".into()));
    }
    raise(c, next)?;
    unsigned(next)
}

pub(super) struct StagedSeries {
    batches: Vec<Batch>,
    highest: i64,
    refused: Option<StorageError>,
}

pub(super) fn stage(tables: &Tables, owner: &str, batches: &[Vec<StorageWrite>]) -> StagedSeries {
    let mut staged = Vec::with_capacity(batches.len());
    let mut highest = 1;
    let mut refused = None;
    for writes in batches {
        match Batch::stage(tables, owner, writes, &mut highest) {
            Ok(batch) => staged.push(batch),
            Err(error) => {
                refused = Some(super::storage_error(error));
                break;
            }
        }
    }
    StagedSeries {
        batches: staged,
        highest,
        refused,
    }
}

pub(super) fn series(
    c: &Connection,
    tables: &Tables,
    owner: &str,
    staged: StagedSeries,
) -> Result<(Vec<Seq>, CommitResult)> {
    let StagedSeries {
        batches,
        highest,
        mut refused,
    } = staged;
    // Explicit IDs are reserved during staging, even if a later check refuses the record.
    raise(c, highest)?;
    let mut receipts = Vec::with_capacity(batches.len());
    for batch in batches {
        let step = Step::new(c)?;
        match batch.check(c, tables, owner, step.seq) {
            Ok(writes) => receipts.push(apply(step, writes)?),
            Err(Error::Invalid(message)) => {
                refused = Some(StorageError::Request(message));
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let stored = receipts.len();
    let result = match refused {
        Some(refused) => CommitResult::Refused { stored, refused },
        None => CommitResult::Stored { stored },
    };
    Ok((receipts, result))
}

fn apply(mut step: Step<'_>, prepared: Vec<StoreWrite>) -> Result<Seq> {
    for write in prepared {
        step.apply(write)?;
    }
    unsigned(step.finish()?.seq)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }

    fn mutable(self) -> bool {
        matches!(self, Self::Task | Self::Submission)
    }
}

#[derive(Default)]
struct Actions {
    create: Option<(DocumentCreate, Initial)>,
    content: Option<EngineContent>,
    retire: bool,
}

enum Initial {
    Base(DocumentBase),
    Copy(DocumentCopySource),
}

#[derive(Default)]
struct Batch {
    claims: Vec<(i64, Kind)>,
    rows: Vec<(i64, Kind, StoreWrite)>,
    documents: IndexMap<i64, Actions>,
}

impl Batch {
    fn stage(
        tables: &Tables,
        owner: &str,
        writes: &[StorageWrite],
        highest: &mut i64,
    ) -> Result<Self> {
        let mut batch = Self::default();
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => {
                    let id = batch.claim(value.id, Kind::Conversation, highest)?;
                    let row = json!({"id": id,
                        "owner_conversation_id": value.owner.map_or(0, |owner| owner.conversation_id),
                        "owner_task_id": value.owner.map_or(0, |owner| owner.task_id),
                        "record": serde_json::to_value(value)?,
                    });
                    batch.row(owner, id, Kind::Conversation, &tables.conversations, row)?;
                }
                StorageWrite::Entry { value } => {
                    let id = batch.claim(value.id, Kind::Entry, highest)?;
                    let conversation = value.conversation_id;
                    let head = value.head.is_some();
                    let row = json!({"id": id, "conversation_id": conversation, "commit_seq": 0, "record": serde_json::to_value(value)? });
                    batch.row(owner, id, Kind::Entry, &tables.entries, row)?;
                    if head {
                        batch.row(
                            owner,
                            id,
                            Kind::Entry,
                            &tables.heads,
                            json!({"id": id, "conversation_id": conversation}),
                        )?;
                    }
                }
                StorageWrite::Task { value } => {
                    let id = batch.claim(value.id, Kind::Task, highest)?;
                    let row = json!({"id": id, "conversation_id": value.conversation_id, "kind": encode_indexed(&value.kind)?,
                        "status": value.state.status().as_str(), "abort_requested": i64::from(value.abort_requested),
                        "background": i64::from(value.background), "record": serde_json::to_value(value)?,
                    });
                    batch.row(owner, id, Kind::Task, &tables.tasks, row)?;
                }
                StorageWrite::Submission { value } => {
                    let id = batch.claim(value.id, Kind::Submission, highest)?;
                    let request = value
                        .request_id
                        .as_deref()
                        .map(encode_indexed)
                        .transpose()?
                        .unwrap_or_default();
                    let row = json!({"id": id, "conversation_id": value.conversation_id, "request_id": request,
                        "status": value.status.as_str(), "record": serde_json::to_value(value)?,
                    });
                    batch.row(owner, id, Kind::Submission, &tables.submissions, row)?;
                }
                StorageWrite::DocumentCreate { record, content } => {
                    let DocumentContent::Base { version, value } = content else {
                        return Err(Error::Invalid("Document creation requires a base".into()));
                    };
                    batch.create(
                        record.clone(),
                        Initial::Base(documents::base(*version, value.clone())?),
                        highest,
                    )?;
                }
                StorageWrite::DocumentCopy { record, source } => {
                    batch.create(record.clone(), Initial::Copy(*source), highest)?
                }
                StorageWrite::DocumentChange { id, content } => {
                    let action = batch.documents.entry(checked_id(*id)?).or_default();
                    if action.create.is_some() || action.content.is_some() {
                        return Err(repeated_content(*id));
                    }
                    action.content = Some(documents::change(content)?);
                }
                StorageWrite::DocumentRetire { id } => {
                    let action = batch.documents.entry(checked_id(*id)?).or_default();
                    if action.retire {
                        return Err(Error::Invalid(format!(
                            "Document {id} is retired more than once"
                        )));
                    }
                    action.retire = true;
                }
            }
        }
        Ok(batch)
    }

    fn create(
        &mut self,
        record: DocumentCreate,
        initial: Initial,
        highest: &mut i64,
    ) -> Result<()> {
        let id = checked_id(record.id)?;
        let action = self.documents.entry(id).or_default();
        if action.create.is_some() || action.content.is_some() {
            return Err(repeated_content(record.id));
        }
        action.create = Some((record, initial));
        self.claims.push((id, Kind::Document));
        *highest = (*highest).max(id);
        Ok(())
    }

    fn claim(&mut self, id: Id, kind: Kind, highest: &mut i64) -> Result<i64> {
        let id = checked_id(id)?;
        self.claims.push((id, kind));
        *highest = (*highest).max(id);
        Ok(id)
    }

    fn row(&mut self, owner: &str, id: i64, kind: Kind, table: &Table, row: Value) -> Result<()> {
        self.rows.push((id, kind, table.put(owner, row)?));
        Ok(())
    }

    fn check(
        self,
        c: &Connection,
        tables: &Tables,
        owner: &str,
        seq: i64,
    ) -> Result<Vec<StoreWrite>> {
        let mut claimed: HashMap<i64, (Option<String>, Kind)> =
            HashMap::with_capacity(self.claims.len());
        for (id, kind) in self.claims {
            let (existing, earlier) = if let Some((stored, earlier)) = claimed.get(&id) {
                (stored.clone(), Some(*earlier))
            } else {
                (tables.ids.record_type(c, id)?, None)
            };
            if let Some(existing) = &existing
                && (!kind.mutable() || existing != kind.name())
            {
                return Err(Error::Invalid(format!(
                    "ID {id} already belongs to {existing}"
                )));
            }
            if let Some(earlier) = earlier {
                if !kind.mutable() {
                    return Err(Error::Invalid(format!("ID {id} is written more than once")));
                }
                if earlier != kind {
                    return Err(Error::Invalid(format!(
                        "ID {id} is written as two record types"
                    )));
                }
            }
            claimed.insert(id, (existing, kind));
        }

        let mut prepared = Vec::with_capacity(self.rows.len() + self.documents.len());
        for (id, kind, mut write) in self.rows {
            if let Some((existing, _)) = claimed.get_mut(&id)
                && existing.is_none()
            {
                prepared.push(tables.ids.claim(owner, id, kind.name())?);
                *existing = Some(kind.name().into());
            }
            if let StoreWrite::Put { table, row, .. } = &mut write
                && table == tables.entries.name()
            {
                row.insert("commit_seq".into(), seq.into());
            }
            prepared.push(write);
        }

        let mut live = HashSet::new();
        let mut bases = HashMap::new();
        for (&id, action) in &self.documents {
            let Some((record, initial)) = &action.create else {
                continue;
            };
            if documents::occupied(c, id)? {
                return Err(Error::Invalid(format!(
                    "ID {id} already belongs to document"
                )));
            }
            if !action.retire {
                let address = record.address();
                let conflicts = documents::find(c, owner, &address, DocumentPoint::Current)?
                    .is_some_and(|current| {
                        self.documents
                            .get(&number(current.id))
                            .is_none_or(|other| !other.retire)
                    });
                if conflicts || !live.insert(address) {
                    return Err(Error::Invalid(
                        "Document address already has a current incarnation".into(),
                    ));
                }
            }
            if let Initial::Copy(source) = initial {
                if self.documents.contains_key(&checked_id(source.id)?) {
                    return Err(Error::Invalid(format!(
                        "Document copy {id} source is changed in the copy batch"
                    )));
                }
                bases.insert(id, copy_base(c, owner, record, *source)?);
            }
        }

        let mut creates = Vec::new();
        for (id, action) in self.documents {
            if let Some((record, initial)) = action.create {
                let base = match initial {
                    Initial::Base(base) => base,
                    Initial::Copy(_) => bases.remove(&id).ok_or_else(|| {
                        Error::Invalid(format!("Document copy {id} was not resolved"))
                    })?,
                };
                creates.push(tables.ids.claim(owner, id, Kind::Document.name())?);
                creates.push(documents::create(owner, record, base, seq, action.retire)?);
                continue;
            }
            let Some(stored) = documents::by_id(c, owner, id)? else {
                return Err(Error::Invalid(format!("Unknown document: {id}")));
            };
            if stored.record.retired_at.is_some() {
                return Err(Error::Invalid(format!("Document {id} is retired")));
            }
            let mut revision = stored.revision;
            if let Some(content) = action.content {
                if let EngineContent::Delta { version, .. } = &content
                    && *version != stored.version
                {
                    return Err(Error::Invalid(format!(
                        "Document {id} version transition requires a base"
                    )));
                }
                prepared.push(StoreWrite::DocumentChange {
                    id,
                    revision,
                    content,
                });
                revision += 1;
            }
            if action.retire {
                prepared.push(StoreWrite::DocumentRetire { id, revision });
            }
        }
        prepared.extend(creates);
        Ok(prepared)
    }
}

fn repeated_content(id: DocumentId) -> Error {
    Error::Invalid(format!("Document {id} has more than one content command"))
}

fn copy_base(
    c: &Connection,
    owner: &str,
    record: &DocumentCreate,
    source: DocumentCopySource,
) -> Result<DocumentBase> {
    let rejected = |detail| {
        Error::Invalid(format!(
            "Document copy {} was rejected: {detail}",
            record.id
        ))
    };
    let source_doc = match documents::load(c, owner, checked_id(source.id)?, source.at) {
        Ok(Some(source_doc)) => source_doc,
        Ok(None) => {
            return Err(rejected(format!(
                "Fork source document {} cannot be read",
                source.id
            )));
        }
        Err(Error::Invalid(message)) => return Err(rejected(message)),
        Err(error) => return Err(error),
    };
    let stored = source_doc.record;
    if !matches!(stored.scope, DocumentScope::Conversation { .. })
        || !matches!(record.scope, DocumentScope::Conversation { .. })
        || stored.kind != record.kind
        || stored.key != record.key
        || stored.history != record.history
        || stored.fork != record.fork
    {
        return Err(rejected(format!(
            "Fork source document {} does not match the copied record",
            source.id
        )));
    }
    documents::base(source_doc.version, source_doc.value)
}
