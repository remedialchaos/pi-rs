use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use pi_rs_chord::{
    delta::{Change, Op, PendingOps, PendingTake, Revision, Tracker},
    json::JsonValue,
};
use serde_json::Value;

use crate::{
    documents::{
        CheckpointInfo, DocDefinition, DocToken, DocumentError, address_id, check_record_scope,
        resolve_address,
    },
    ids::*,
    storage_types::*,
};

use super::{Error, Key, Result, keys::table_keys};

pub(crate) fn validate_address(
    definition: &DocDefinition,
    address: &DocumentAddress,
) -> Result<()> {
    let owner = match address.scope {
        DocumentScope::Session => None,
        DocumentScope::Conversation { conversation_id } => Some(conversation_id),
        DocumentScope::Task { task_id } => Some(task_id),
    };
    if resolve_address(definition, owner, address.key.clone())? != *address {
        return Err(DocumentError::Address(definition.kind.clone()).into());
    }
    Ok(())
}

pub(crate) struct LoadedDocument {
    pub record: DocumentRecord,
    pub version: u64,
    pub stored_version: u64,
    pub deltas: usize,
    pub value: Arc<JsonObject>,
    pub revision: Revision,
}

impl LoadedDocument {
    pub fn new(
        record: DocumentRecord,
        version: u64,
        stored_version: u64,
        deltas: usize,
        value: JsonObject,
    ) -> Result<Self> {
        let revision = Revision::from_json(JsonValue::from_serde(Value::Object(value.clone())))?;
        Ok(Self {
            record,
            version,
            stored_version,
            deltas,
            value: Arc::new(value),
            revision,
        })
    }
}

pub(crate) enum Origin {
    New(DocumentCreate),
    Copy(DocumentCreate, DocumentCopySource),
    Existing(DocumentRecord),
}

impl Origin {
    fn id(&self) -> DocumentId {
        match self {
            Self::New(record) | Self::Copy(record, _) => record.id,
            Self::Existing(record) => record.id,
        }
    }
    fn fork(&self) -> Option<DocumentFork> {
        match self {
            Self::New(record) | Self::Copy(record, _) => record.fork,
            Self::Existing(record) => record.fork,
        }
    }
    fn record(&self, seq: Option<Seq>, retire: bool) -> Result<DocumentRecord> {
        let retired_at = if retire { seq } else { None };
        Ok(match self {
            Self::New(record) | Self::Copy(record, _) => record.clone().stamp(
                seq.ok_or_else(|| Error::invalid("Document creation has no durability receipt"))?,
                retired_at,
            ),
            Self::Existing(record) => {
                let mut record = record.clone();
                if retire {
                    record.retired_at = retired_at;
                }
                record
            }
        })
    }
}

pub(crate) struct DocumentDraft {
    pub address: DocumentAddress,
    pub origin: Origin,
    pub retire: bool,
    draft: Option<Editing>,
}

struct Editing {
    // A Change has a weak authority; retain it for the lifetime of the draft.
    _tracker: Tracker,
    change: Change,
    definition: DocToken,
    loaded: Option<Arc<LoadedDocument>>,
}

impl DocumentDraft {
    pub fn new(address: DocumentAddress, origin: Origin) -> Self {
        Self {
            address,
            origin,
            retire: false,
            draft: None,
        }
    }

    pub fn create(
        address: DocumentAddress,
        record: DocumentCreate,
        definition: DocToken,
        value: JsonObject,
    ) -> Result<Self> {
        let tracker = Tracker::new(JsonValue::from_serde(Value::Object(value)))?;
        Ok(Self::edit(
            address,
            Origin::New(record),
            definition,
            tracker,
            None,
        ))
    }

    pub fn loaded(
        address: DocumentAddress,
        definition: DocToken,
        loaded: Arc<LoadedDocument>,
    ) -> Result<Self> {
        let tracker = Tracker::from_revision(loaded.revision.clone())?;
        Ok(Self::edit(
            address,
            Origin::Existing(loaded.record.clone()),
            definition,
            tracker,
            Some(loaded),
        ))
    }

    fn edit(
        address: DocumentAddress,
        origin: Origin,
        definition: DocToken,
        tracker: Tracker,
        loaded: Option<Arc<LoadedDocument>>,
    ) -> Self {
        let change = tracker.begin_change();
        Self {
            address,
            origin,
            retire: false,
            draft: Some(Editing {
                _tracker: tracker,
                change,
                definition,
                loaded,
            }),
        }
    }

    pub fn check_scope(&self, definition: &DocDefinition) -> Result<()> {
        match &self.origin {
            Origin::New(record) | Origin::Copy(record, _) => {
                check_record_scope(definition, record)?
            }
            Origin::Existing(record) => check_record_scope(definition, record)?,
        }
        Ok(())
    }

    pub fn change(&mut self) -> Result<&mut Change> {
        self.draft
            .as_mut()
            .map(|draft| &mut draft.change)
            .ok_or_else(|| Error::invalid("Document was not acquired"))
    }

    pub fn prepare(self) -> Result<DocumentPlan> {
        let mut plan = DocumentPlan::new(self.address, self.origin, self.retire);
        if let Some(mut draft) = self.draft {
            let prepared = draft.change.prepare()?;
            let version = draft.definition.version;
            let loaded = draft.loaded;
            if loaded
                .as_ref()
                .is_none_or(|loaded| loaded.stored_version != version)
                || !prepared.ops.is_empty()
            {
                let root = prepared.value.to_json().to_serde()?;
                let Value::Object(value) = root else {
                    return Err(Error::invalid("Document state must be a JSON object"));
                };
                let revision = prepared.value.clone();
                let base = loaded
                    .as_ref()
                    .is_none_or(|loaded| loaded.stored_version != version);
                plan.candidate = Some(Candidate {
                    value: Arc::new(value),
                    revision,
                    version,
                });
                if loaded.is_some() {
                    plan.ops = prepared.ops.clone();
                }
                plan.base = base;
                plan.observed = true;
            }
            plan.loaded = loaded;
            plan.definition = Some(draft.definition);
        }
        Ok(plan)
    }
}

struct Candidate {
    value: Arc<JsonObject>,
    revision: Revision,
    version: u64,
}

pub(crate) struct DocumentPlan {
    pub address: DocumentAddress,
    pub retire: bool,
    pub conversation_id: Option<ConversationId>,
    origin: Origin,
    definition: Option<DocToken>,
    loaded: Option<Arc<LoadedDocument>>,
    candidate: Option<Candidate>,
    ops: Vec<Op>,
    folded: Option<Vec<Op>>,
    base: bool,
    observed: bool,
}

impl DocumentPlan {
    fn new(address: DocumentAddress, origin: Origin, retire: bool) -> Self {
        Self {
            address,
            retire,
            origin,
            definition: None,
            loaded: None,
            candidate: None,
            ops: Vec::new(),
            folded: None,
            base: false,
            observed: false,
            conversation_id: None,
        }
    }

    pub fn retire(record: DocumentRecord) -> Self {
        Self::new(record.address(), Origin::Existing(record), true)
    }
    pub fn id(&self) -> DocumentId {
        self.origin.id()
    }
    pub fn fork(&self) -> Option<DocumentFork> {
        self.origin.fork()
    }
    pub fn writes(&self) -> bool {
        self.retire || self.candidate.is_some() || matches!(self.origin, Origin::Copy(..))
    }

    pub fn checkpoint(&mut self) {
        if !self.base
            && let (Some(candidate), Some(definition), Some(loaded)) =
                (&self.candidate, &self.definition, &self.loaded)
        {
            self.base = (definition.checkpoint_when)(
                &candidate.value,
                &self.ops,
                CheckpointInfo {
                    deltas_since_base: loaded.deltas,
                },
            );
        }
    }

    fn holdable(&self) -> bool {
        if !self.writes() {
            return true;
        }
        self.loaded.as_ref().is_some_and(|loaded| {
            self.candidate
                .as_ref()
                .is_some_and(|next| next.version == loaded.stored_version)
                && !self.retire
                && loaded.record.history == Some(DocumentHistory::Latest)
                && loaded.record.fork == Some(DocumentFork::Initial)
        })
    }

    fn merge_hold(&mut self, mut held: Held) {
        let loaded = held.loaded;
        if self.candidate.is_none() {
            self.candidate = Some(Candidate {
                value: loaded.value.clone(),
                version: loaded.version,
                revision: loaded.revision.clone(),
            });
        }
        held.ops.push(&self.ops);
        self.folded = Some(match held.ops.take() {
            PendingTake::Ops(ops) => ops,
            PendingTake::Replace => {
                self.base = true;
                vec![Op::Replace(self.candidate.as_ref().map_or_else(
                    || loaded.revision.to_json(),
                    |next| next.revision.to_json(),
                ))]
            }
        });
        self.base |= held.base;
        self.loaded = Some(loaded);
        if let DocumentScope::Conversation { conversation_id } = self.address.scope {
            self.conversation_id = Some(conversation_id);
        }
    }

    fn content(&self) -> Option<DocumentContent> {
        let next = self.candidate.as_ref()?;
        Some(if self.base {
            DocumentContent::Base {
                version: next.version,
                value: (*next.value).clone(),
            }
        } else {
            DocumentContent::Delta {
                version: next.version,
                ops: self.folded.as_ref().unwrap_or(&self.ops).clone(),
            }
        })
    }

    fn append_writes(&self, writes: &mut Vec<StorageWrite>) {
        match &self.origin {
            Origin::Copy(record, source) => writes.push(StorageWrite::DocumentCopy {
                record: record.clone(),
                source: *source,
            }),
            Origin::New(record) => {
                if let Some(content) = self.content() {
                    writes.push(StorageWrite::DocumentCreate {
                        record: record.clone(),
                        content,
                    });
                }
            }
            Origin::Existing(record) => {
                if let Some(content) = self.content() {
                    writes.push(StorageWrite::DocumentChange {
                        id: record.id,
                        content,
                    });
                }
            }
        }
        if self.retire {
            writes.push(StorageWrite::DocumentRetire { id: self.id() });
        }
    }
}

#[derive(Default)]
pub(crate) struct PreparedBatch {
    pub writes: Vec<StorageWrite>,
    pub documents: Vec<DocumentPlan>,
    pub created_tasks: Vec<TaskId>,
    pub silent: bool,
}

impl PreparedBatch {
    pub fn has_writes(&self) -> bool {
        !self.writes.is_empty() || self.documents.iter().any(DocumentPlan::writes)
    }

    pub fn holdable(&self) -> bool {
        self.writes.is_empty() && self.documents.iter().all(DocumentPlan::holdable)
    }

    pub fn assemble(&mut self, cache: &mut DocumentCache) {
        for plan in &mut self.documents {
            if matches!(plan.origin, Origin::Existing(_))
                && let Some(held) = cache.held.remove(&plan.id())
            {
                plan.merge_hold(held);
            }
            plan.append_writes(&mut self.writes);
        }
    }

    pub fn keys(&self) -> Vec<Key> {
        let mut keys = Vec::new();
        table_keys(&self.writes, &mut keys);
        for plan in self.documents.iter().filter(|plan| plan.writes()) {
            keys.push(Key::Document(address_id(&plan.address)));
            keys.push(Key::Incarnation(plan.id()));
            if plan.retire || !matches!(plan.origin, Origin::Existing(_)) {
                keys.push(Key::Scope(plan.address.scope));
            }
        }
        keys
    }
}

struct CacheEntry {
    document: Arc<LoadedDocument>,
    last_used: u64,
}

struct Held {
    loaded: Arc<LoadedDocument>,
    ops: PendingOps,
    base: bool,
    deadline: Option<u64>,
}

#[derive(Default)]
pub(crate) struct DocumentCache {
    current: HashMap<String, CacheEntry>,
    held: HashMap<DocumentId, Held>,
    task_documents: HashMap<TaskId, HashMap<DocumentId, DocumentRecord>>,
}

pub(crate) struct Adoption {
    pub observed: Vec<CommitChange>,
    pub held_adjustments: Vec<(usize, Vec<Op>)>,
    pub stored_only: Vec<CommitChange>,
}

impl DocumentCache {
    pub fn get(&mut self, id: &str, now: u64) -> Option<Arc<LoadedDocument>> {
        let entry = self.current.get_mut(id)?;
        entry.last_used = now;
        Some(entry.document.clone())
    }

    pub fn touch(&mut self, key: &str, now: u64) {
        if let Some(entry) = self.current.get_mut(key) {
            entry.last_used = now;
        }
    }

    pub fn put(&mut self, key: String, document: Arc<LoadedDocument>, now: u64) {
        self.current.insert(
            key,
            CacheEntry {
                document,
                last_used: now,
            },
        );
    }

    pub fn clear(&mut self) {
        self.current.clear();
    }
    pub fn len(&self) -> usize {
        self.current.len()
    }
    pub fn held_len(&self) -> usize {
        self.held.len()
    }
    pub fn task_count(&self) -> usize {
        self.task_documents.len()
    }
    pub fn has_held(&self, id: DocumentId) -> bool {
        self.held.contains_key(&id)
    }
    pub fn knows_task(&self, id: TaskId) -> Option<Vec<DocumentRecord>> {
        self.task_documents
            .get(&id)
            .map(|docs| docs.values().cloned().collect())
    }

    pub fn take_held(&mut self, id: Option<DocumentId>) -> PreparedBatch {
        let mut batch = PreparedBatch::default();
        let mut add = |held: Held| {
            let record = held.loaded.record.clone();
            let mut plan = DocumentPlan::new(record.address(), Origin::Existing(record), false);
            plan.merge_hold(held);
            plan.append_writes(&mut batch.writes);
            batch.documents.push(plan);
        };
        if let Some(id) = id {
            if let Some(held) = self.held.remove(&id) {
                add(held);
            }
        } else {
            // Publishing order is deterministic even when several holds share a deadline.
            let mut holds: Vec<_> = self.held.drain().collect();
            holds.sort_unstable_by_key(|(id, _)| *id);
            for (_, held) in holds {
                add(held);
            }
        }
        batch
    }

    pub fn sweep(
        &mut self,
        now: u64,
        retention: Option<u64>,
        pinned: &HashSet<String>,
        mut unpublished: impl FnMut(&str) -> bool,
    ) -> Option<u64> {
        let retention = retention?;
        let mut next = None;
        self.current.retain(|key, item| {
            if self.held.contains_key(&item.document.record.id)
                || pinned.contains(key)
                || unpublished(key)
            {
                return true;
            }
            let deadline = item.last_used.saturating_add(retention);
            if deadline <= now {
                return false;
            }
            next = Some(next.map_or(deadline, |old: u64| old.min(deadline)));
            true
        });
        next
    }

    pub fn next_hold(&self) -> Option<u64> {
        self.held.values().filter_map(|held| held.deadline).min()
    }

    pub fn adopt(
        &mut self,
        batch: &mut PreparedBatch,
        seq: Option<Seq>,
        deadline: Option<u64>,
        now: u64,
        describe: bool,
    ) -> Result<Adoption> {
        let mut adoption = Adoption {
            observed: Vec::new(),
            held_adjustments: Vec::new(),
            stored_only: Vec::new(),
        };
        for id in &batch.created_tasks {
            self.task_documents.entry(*id).or_default();
        }
        for plan in batch.documents.drain(..).filter(DocumentPlan::writes) {
            let record = plan.origin.record(seq, plan.retire)?;
            let key = address_id(&plan.address);
            if plan.retire {
                if self
                    .current
                    .get(&key)
                    .is_some_and(|entry| entry.document.record.id == record.id)
                {
                    self.current.remove(&key);
                }
                if let DocumentScope::Task { task_id } = record.scope
                    && let Some(known) = self.task_documents.get_mut(&task_id)
                {
                    known.remove(&record.id);
                }
                if describe {
                    adoption
                        .observed
                        .push(CommitChange::Document(DocumentCommitChange::Retire {
                            record,
                            conversation_id: plan.conversation_id,
                        }));
                }
                continue;
            }
            if let Origin::Copy(_, source) = plan.origin {
                if describe && let Some(conversation_id) = plan.conversation_id {
                    adoption.observed.push(CommitChange::DocumentCopy {
                        record,
                        conversation_id,
                        source,
                    });
                }
                continue;
            }
            let Some(next) = plan.candidate else {
                continue;
            };
            let loaded = plan.loaded.as_deref();
            let deltas = if seq.is_some() {
                if plan.base {
                    0
                } else {
                    loaded.map_or(0, |document| document.deltas + 1)
                }
            } else {
                loaded.map_or(0, |document| document.deltas)
            };
            let document = Arc::new(LoadedDocument {
                record: record.clone(),
                version: next.version,
                stored_version: if seq.is_some() {
                    next.version
                } else {
                    loaded.map_or(next.version, |document| document.stored_version)
                },
                deltas,
                value: next.value,
                revision: next.revision,
            });
            if seq.is_none() {
                let held = self.held.entry(record.id).or_insert_with(|| Held {
                    loaded: document.clone(),
                    ops: PendingOps::default(),
                    base: false,
                    deadline,
                });
                held.loaded = document.clone();
                held.ops.push(&plan.ops);
                held.base |= plan.base;
                if let Some(deadline) = deadline {
                    held.deadline = Some(held.deadline.map_or(deadline, |old| old.min(deadline)));
                }
            }
            if let DocumentScope::Task { task_id } = record.scope
                && let Some(known) = self.task_documents.get_mut(&task_id)
            {
                known.entry(record.id).or_insert_with(|| record.clone());
            }
            self.put(key, document.clone(), now);
            if !describe {
                continue;
            }
            let mut ops = plan.ops;
            if !plan.observed {
                if let Some(folded) = plan.folded {
                    ops = folded;
                }
            } else if let Some(folded) = plan.folded {
                adoption
                    .held_adjustments
                    .push((adoption.observed.len(), folded));
            }
            let change = CommitChange::Document(DocumentCommitChange::Change {
                record,
                conversation_id: plan.conversation_id,
                version: next.version,
                value: (*document.value).clone(),
                ops,
            });
            if plan.observed {
                adoption.observed.push(change);
            } else {
                adoption.stored_only.push(change);
            }
        }
        for write in &batch.writes {
            if let StorageWrite::Task { value } = write
                && matches!(value.state, TaskState::Terminal { .. })
            {
                self.task_documents.remove(&value.id);
            }
        }
        Ok(adoption)
    }
}
