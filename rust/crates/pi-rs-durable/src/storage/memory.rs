use super::scan::{MAX_SAFE_INTEGER, document_scan_start, paginate, scan_start};
use super::*;
use pi_rs_chord::delta::apply;
use pi_rs_chord::json::JsonValue;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

pub(super) fn backend(message: impl Into<String>) -> StorageError {
    StorageError::Backend(message.into())
}
pub(super) fn lock<T>(mutex: &Mutex<T>) -> StorageResult<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| backend("storage lock is poisoned"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Table {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}
impl Table {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }
}

pub(super) struct DocumentState {
    pub record: DocumentRecord,
    pub revisions: Vec<(Seq, DocumentContent)>,
}

pub(super) struct State {
    pub next_id: u64,
    pub next_seq: Seq,
    pub ids: HashMap<u64, Table>,
    pub conversations: BTreeMap<ConversationId, ConversationRecord>,
    pub entries: BTreeMap<EntryId, StoredEntry>,
    entry_ids: HashMap<ConversationId, std::collections::BTreeSet<EntryId>>,
    pub tasks: BTreeMap<TaskId, TaskRecord>,
    pub submissions: BTreeMap<SubmissionId, SubmissionRecord>,
    requests: HashMap<ConversationId, HashMap<String, SubmissionId>>,
    pub documents: BTreeMap<DocumentId, DocumentState>,
    addresses: HashMap<DocumentAddress, DocumentId>,
    pub(super) indexed_versions: HashMap<DocumentId, u64>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            next_id: 2,
            next_seq: 1,
            ids: HashMap::new(),
            conversations: BTreeMap::new(),
            entries: BTreeMap::new(),
            entry_ids: HashMap::new(),
            tasks: BTreeMap::new(),
            submissions: BTreeMap::new(),
            requests: HashMap::new(),
            documents: BTreeMap::new(),
            addresses: HashMap::new(),
            indexed_versions: HashMap::new(),
        }
    }
}

#[derive(Default)]
pub(super) struct DocumentAction {
    pub create: Option<DocumentCreate>,
    pub content: Option<DocumentContent>,
    pub retire: bool,
}
pub(super) struct Prepared {
    pub seq: Seq,
    pub writes: Vec<StorageWrite>,
    pub actions: BTreeMap<DocumentId, DocumentAction>,
}

impl State {
    pub(super) fn mint_id(&mut self) -> StorageResult<u64> {
        if self.next_id > MAX_SAFE_INTEGER {
            return Err(backend("ID space is exhausted"));
        }
        let id = self.next_id;
        self.next_id += 1;
        Ok(id)
    }

    pub(super) fn prepare(&self, writes: &[StorageWrite], seq: Seq) -> StorageResult<Prepared> {
        if seq < self.next_seq || seq > MAX_SAFE_INTEGER {
            return Err(backend(format!(
                "Commit sequence {seq} does not strictly increase or exceeds supported range"
            )));
        }
        let mut touched = None;
        let mut claimed = HashMap::new();
        let mut detached = Vec::with_capacity(writes.len());
        let mut actions = BTreeMap::<_, DocumentAction>::new();
        for original in writes {
            let write = if let StorageWrite::DocumentCopy { record, source } = original {
                if touched
                    .get_or_insert_with(|| {
                        writes
                            .iter()
                            .filter_map(document_id)
                            .collect::<HashSet<_>>()
                    })
                    .contains(&source.id)
                {
                    return Err(backend(format!(
                        "Document copy {} was rejected: source {} is changed in the copy batch",
                        record.id, source.id
                    )));
                }
                let stored = self
                    .document(source.id, source.at)
                    .map_err(|e| backend(format!("Document copy {} was rejected: {e}", record.id)))?
                    .ok_or_else(|| {
                        backend(format!(
                            "Document copy {} was rejected: source {} cannot be read",
                            record.id, source.id
                        ))
                    })?;
                if !matches!(stored.record.scope, DocumentScope::Conversation { .. })
                    || !matches!(record.scope, DocumentScope::Conversation { .. })
                    || stored.record.kind != record.kind
                    || stored.record.key != record.key
                    || stored.record.history != record.history
                    || stored.record.fork != record.fork
                {
                    return Err(backend(format!(
                        "Document copy {} was rejected: source {} does not match the copied record",
                        record.id, source.id
                    )));
                }
                StorageWrite::DocumentCreate {
                    record: record.clone(),
                    content: DocumentContent::Base {
                        version: stored.version,
                        value: stored.value,
                    },
                }
            } else {
                original.clone()
            };
            if let Some((id, table)) = record_claim(&write) {
                let replaceable = matches!(table, Table::Task | Table::Submission);
                if id > MAX_SAFE_INTEGER {
                    return Err(backend(format!("Invalid record ID {id}")));
                }
                if let Some(prior) = self.ids.get(&id)
                    && (!replaceable || *prior != table)
                {
                    return Err(backend(format!(
                        "ID {id} already belongs to {}",
                        prior.name()
                    )));
                }
                if let Some(prior) = claimed.insert(id, table) {
                    if !replaceable {
                        return Err(backend(format!("ID {id} is written more than once")));
                    }
                    if prior != table {
                        return Err(backend(format!("ID {id} is written as two record types")));
                    }
                }
            }
            if let Some(id) = document_id(&write) {
                let action = actions.entry(id).or_default();
                match &write {
                    StorageWrite::DocumentCreate { record, content } => {
                        if !matches!(content, DocumentContent::Base { .. }) {
                            return Err(backend(format!("Document {id} creation requires a base")));
                        }
                        if action.content.replace(content.clone()).is_some() {
                            return Err(backend(format!(
                                "Document {id} has more than one content command"
                            )));
                        }
                        action.create = Some(record.clone());
                    }
                    StorageWrite::DocumentChange { content, .. } => {
                        if action.content.replace(content.clone()).is_some() {
                            return Err(backend(format!(
                                "Document {id} has more than one content command"
                            )));
                        }
                    }
                    StorageWrite::DocumentRetire { .. }
                        if std::mem::replace(&mut action.retire, true) =>
                    {
                        return Err(backend(format!("Document {id} is retired more than once")));
                    }
                    _ => {}
                }
            }
            detached.push(write);
        }
        let mut live_counts = HashMap::new();
        for (&id, action) in &actions {
            let existing = self.documents.get(&id);
            if existing.is_some_and(|doc| doc.record.retired_at.is_some()) {
                return Err(backend(format!("Document {id} is retired")));
            }
            if let Some(DocumentContent::Delta { version, .. }) = action.content.as_ref() {
                let previous = existing
                    .and_then(|doc| doc.revisions.last().map(|(_, content)| content.version()))
                    .or_else(|| self.indexed_versions.get(&id).copied())
                    .ok_or_else(|| backend(format!("Document {id} delta has no base")))?;
                if previous != *version {
                    return Err(backend(format!(
                        "Document {id} version transition requires a base"
                    )));
                }
            }
            let address = if let Some(create) = &action.create {
                create.address()
            } else {
                existing
                    .ok_or_else(|| backend(format!("Unknown document: {id}")))?
                    .record
                    .address()
            };
            let current_id = self.addresses.get(&address).copied();
            let live = live_counts
                .entry(address)
                .or_insert(i32::from(current_id.is_some()));
            if action.retire && current_id == Some(id) {
                *live -= 1;
            }
            if action.create.is_some() && !action.retire {
                *live += 1;
            }
        }
        if live_counts.values().any(|live| *live > 1) {
            return Err(backend(
                "Document address already has a current incarnation",
            ));
        }
        Ok(Prepared {
            seq,
            writes: detached,
            actions,
        })
    }

    pub(super) fn apply(&mut self, prepared: Prepared) {
        let seq = prepared.seq;
        for write in prepared.writes {
            if let Some((id, table)) = record_claim(&write) {
                self.ids.insert(id, table);
                self.next_id = self.next_id.max(id + 1);
            }
            match write {
                StorageWrite::Conversation { value } => {
                    self.conversations.insert(value.id, value);
                }
                StorageWrite::Entry { value } => self.insert_entry(StoredEntry {
                    entry: value,
                    commit_seq: seq,
                }),
                StorageWrite::Task { value } => {
                    self.tasks.insert(value.id, value);
                }
                StorageWrite::Submission { value } => self.insert_submission(value),
                _ => {}
            }
        }
        // Clear every retiring address first, then install creations regardless of write or ID order.
        for (&id, action) in &prepared.actions {
            if action.retire
                && let Some(doc) = self.documents.get(&id)
            {
                self.addresses.remove(&doc.record.address());
            }
        }
        for (id, action) in prepared.actions {
            if let Some(create) = action.create {
                let record = create.stamp(seq, action.retire.then_some(seq));
                let revisions = if action.retire && record.current_only() {
                    Vec::new()
                } else {
                    action
                        .content
                        .into_iter()
                        .map(|content| (seq, content))
                        .collect()
                };
                self.insert_document(DocumentState { record, revisions });
            } else if let Some(doc) = self.documents.get_mut(&id) {
                if let Some(content) = action.content {
                    if doc.record.current_only() && matches!(content, DocumentContent::Base { .. })
                    {
                        doc.revisions.clear();
                    }
                    doc.revisions.push((seq, content));
                }
                if action.retire {
                    doc.record.retired_at = Some(seq);
                    if doc.record.current_only() {
                        doc.revisions.clear();
                    }
                }
            }
        }
        self.next_seq = seq + 1;
    }
    pub(super) fn insert_entry(&mut self, entry: StoredEntry) {
        self.entry_ids
            .entry(entry.entry.conversation_id)
            .or_default()
            .insert(entry.entry.id);
        self.entries.insert(entry.entry.id, entry);
    }
    pub(super) fn insert_submission(&mut self, value: SubmissionRecord) {
        if let Some(previous) = self.submissions.get(&value.id)
            && let Some(request) = previous.request_id.as_ref()
            && let Some(index) = self.requests.get_mut(&previous.conversation_id)
            && index.get(request) == Some(&value.id)
        {
            index.remove(request);
        }
        if let Some(request) = &value.request_id {
            self.requests
                .entry(value.conversation_id)
                .or_default()
                .insert(request.clone(), value.id);
        }
        self.submissions.insert(value.id, value);
    }
    pub(super) fn insert_document(&mut self, doc: DocumentState) {
        if doc.record.retired_at.is_none() {
            self.addresses.insert(doc.record.address(), doc.record.id);
        }
        self.documents.insert(doc.record.id, doc);
    }
    pub(super) fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageResult<Option<StoredDocument>> {
        let Some(doc) = self.documents.get(&id) else {
            return Ok(None);
        };
        materialize(&doc.record, &doc.revisions, at)
    }
    fn segments(
        &self,
        id: ConversationId,
        mut upper: EntryId,
        lower: EntryId,
    ) -> StorageResult<Vec<(ConversationId, EntryId)>> {
        let mut chain = Vec::new();
        let mut current = id;
        loop {
            let conversation = self
                .conversations
                .get(&current)
                .ok_or_else(|| StorageError::Request(format!("Unknown conversation: {current}")))?;
            if upper < lower {
                break;
            }
            if chain.iter().any(|(id, _)| *id == current) {
                return Err(backend("Cycle in conversation ancestry"));
            }
            chain.push((current, upper));
            if let Some(parent) = conversation.parent {
                upper = upper.min(parent.at);
                current = parent.conversation_id;
            } else {
                break;
            }
        }
        Ok(chain)
    }
    pub(super) fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>> {
        let chain = self.segments(conversation_id, id, id)?;
        Ok(self
            .entries
            .get(&id)
            .filter(|entry| {
                chain
                    .iter()
                    .any(|(conv, upper)| *conv == entry.entry.conversation_id && id <= *upper)
            })
            .cloned())
    }
    pub(super) fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        for (conv, upper) in self.segments(conversation_id, at.unwrap_or(u64::MAX), 0)? {
            if let Some(ids) = self.entry_ids.get(&conv) {
                for id in ids.range(..=upper).rev() {
                    if let Some(stored) = self.entries.get(id)
                        && stored.entry.head.is_some()
                    {
                        return Ok(Some(stored.entry.clone()));
                    }
                }
            }
        }
        Ok(None)
    }
    pub(super) fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Descending)?;
        let min = query.min_entry_id.unwrap_or(0);
        let max = query.max_entry_id.unwrap_or(u64::MAX);
        let mut segments = self.segments(query.conversation_id, max, min)?;
        if start.order == ScanOrder::Ascending {
            segments.reverse();
        }
        let values = segments
            .into_iter()
            .flat_map(|(conv, upper)| {
                self.entry_ids.get(&conv).into_iter().flat_map(move |ids| {
                    let mut values = ids.range(min..=upper);
                    std::iter::from_fn(move || match start.order {
                        ScanOrder::Ascending => values.next(),
                        ScanOrder::Descending => values.next_back(),
                    })
                })
            })
            .filter(|&&id| start.contains(id))
            .filter_map(|id| self.entries.get(id))
            .map(|entry| entry.entry.clone());
        Ok(paginate(values, |entry| entry.id, limit, start.order))
    }
    pub(super) fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        Ok(paginate(
            ordered(self.conversations.values(), start.order)
                .filter(|r| {
                    start.contains(r.id)
                        && query.owner_conversation_id.is_none_or(|id| {
                            r.owner.is_some_and(|owner| owner.conversation_id == id)
                        })
                        && query
                            .owner_task_id
                            .is_none_or(|id| r.owner.is_some_and(|owner| owner.task_id == id))
                })
                .cloned(),
            |r| r.id,
            limit,
            start.order,
        ))
    }
    pub(super) fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        Ok(paginate(
            ordered(self.tasks.values(), start.order)
                .filter(|r| {
                    start.contains(r.id)
                        && query
                            .conversation_id
                            .is_none_or(|id| r.conversation_id == id)
                        && query.kind.as_ref().is_none_or(|kind| r.kind == *kind)
                        && query.status.is_none_or(|status| r.state.status() == status)
                        && query
                            .abort_requested
                            .is_none_or(|mark| r.abort_requested == mark)
                        && query
                            .background
                            .is_none_or(|background| r.background == background)
                })
                .cloned(),
            |r| r.id,
            limit,
            start.order,
        ))
    }
    pub(super) fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        Ok(paginate(
            ordered(self.submissions.values(), start.order)
                .filter(|r| {
                    start.contains(r.id)
                        && query
                            .conversation_id
                            .is_none_or(|id| r.conversation_id == id)
                        && query.status.is_none_or(|status| r.status == status)
                })
                .cloned(),
            |r| r.id,
            limit,
            start.order,
        ))
    }
    pub(super) fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> Option<DocumentRecord> {
        if at == DocumentPoint::Current {
            return self
                .addresses
                .get(address)
                .and_then(|id| self.documents.get(id))
                .map(|doc| doc.record.clone());
        }
        self.documents
            .values()
            .find(|doc| {
                doc.record.kind == address.kind
                    && doc.record.scope == address.scope
                    && doc.record.key == address.key
                    && doc.record.alive_at(at)
            })
            .map(|doc| doc.record.clone())
    }
    pub(super) fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>> {
        let start = document_scan_start(cursor)?;
        Ok(paginate(
            self.documents
                .values()
                .map(|doc| &doc.record)
                .filter(|r| {
                    start.contains(r.id)
                        && r.scope == query.scope
                        && query.kind.as_ref().is_none_or(|kind| r.kind == *kind)
                        && r.alive_at(query.at)
                })
                .cloned(),
            |r| r.id,
            limit,
            start.order,
        ))
    }
    pub(super) fn submission_by_request(
        &self,
        id: ConversationId,
        request: &str,
    ) -> Option<SubmissionRecord> {
        self.requests
            .get(&id)
            .and_then(|requests| requests.get(request))
            .and_then(|id| self.submissions.get(id))
            .cloned()
    }
}

pub(super) fn materialize(
    record: &DocumentRecord,
    revisions: &[(Seq, DocumentContent)],
    at: DocumentPoint,
) -> StorageResult<Option<StoredDocument>> {
    if at != DocumentPoint::Current && record.current_only() {
        return Err(StorageError::Request(format!(
            "Document {} does not retain historical content",
            record.id
        )));
    }
    if !record.alive_at(at) {
        return Ok(None);
    }
    let end = match at {
        DocumentPoint::Current => revisions.len(),
        DocumentPoint::Seq(seq) => revisions.partition_point(|(id, _)| *id <= seq),
    };
    let relevant = &revisions[..end];
    let index = relevant
        .iter()
        .rposition(|(_, content)| matches!(content, DocumentContent::Base { .. }))
        .ok_or_else(|| backend(format!("Document {} is missing a required base", record.id)))?;
    let DocumentContent::Base { version, value } = &relevant[index].1 else {
        return Err(backend("Invalid document base"));
    };
    let changes = &relevant[index + 1..];
    let value = if changes.is_empty() {
        value.clone()
    } else {
        let mut value = JsonValue::from_serde(Value::Object(value.clone()));
        for (_, content) in changes {
            match content {
                DocumentContent::Delta {
                    version: target,
                    ops,
                } if target == version => {
                    value = apply(value, ops).map_err(|err| backend(err.to_string()))?;
                }
                _ => {
                    return Err(backend(format!(
                        "Document {} crosses a stored version boundary without a base",
                        record.id
                    )));
                }
            }
        }
        let Value::Object(value) = value.to_serde().map_err(|err| backend(err.to_string()))? else {
            return Err(backend("Document value is not an object"));
        };
        value
    };
    Ok(Some(StoredDocument {
        record: record.clone(),
        version: *version,
        value,
        deltas_since_base: changes.len(),
    }))
}

fn ordered<T>(
    mut iter: impl DoubleEndedIterator<Item = T>,
    order: ScanOrder,
) -> impl Iterator<Item = T> {
    std::iter::from_fn(move || match order {
        ScanOrder::Ascending => iter.next(),
        ScanOrder::Descending => iter.next_back(),
    })
}

pub(super) fn document_id(write: &StorageWrite) -> Option<DocumentId> {
    match write {
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            Some(record.id)
        }
        StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => Some(*id),
        _ => None,
    }
}
pub(super) fn record_claim(write: &StorageWrite) -> Option<(u64, Table)> {
    Some(match write {
        StorageWrite::Conversation { value } => (value.id, Table::Conversation),
        StorageWrite::Entry { value } => (value.id, Table::Entry),
        StorageWrite::Task { value } => (value.id, Table::Task),
        StorageWrite::Submission { value } => (value.id, Table::Submission),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            (record.id, Table::Document)
        }
        _ => return None,
    })
}

// Memory-backed adapters guard their read snapshot differently, while sharing the read semantics.
macro_rules! state_reads {
    () => {
        fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>> {
            self.read(|state| Ok(state.conversations.get(&id).cloned()))
        }
        fn scan_conversations(
            &self,
            q: &ConversationQuery,
            limit: usize,
            cursor: Option<&Cursor>,
        ) -> StorageResult<Page<ConversationRecord>> {
            self.read(|state| state.scan_conversations(q, limit, cursor))
        }
        fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
            self.read(|state| Ok(state.entries.get(&id).cloned()))
        }
        fn visible_entry(
            &self,
            c: ConversationId,
            id: EntryId,
        ) -> StorageResult<Option<StoredEntry>> {
            self.read(|state| state.visible_entry(c, id))
        }
        fn find_latest_head_marker(
            &self,
            c: ConversationId,
            at: Option<EntryId>,
        ) -> StorageResult<Option<EntryRecord>> {
            self.read(|state| state.find_latest_head_marker(c, at))
        }
        fn scan_entries(
            &self,
            q: &EntryQuery,
            limit: usize,
            cursor: Option<&Cursor>,
        ) -> StorageResult<Page<EntryRecord>> {
            self.read(|state| state.scan_entries(q, limit, cursor))
        }
        fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>> {
            self.read(|state| Ok(state.tasks.get(&id).cloned()))
        }
        fn scan_tasks(
            &self,
            q: &TaskQuery,
            limit: usize,
            cursor: Option<&Cursor>,
        ) -> StorageResult<Page<TaskRecord>> {
            self.read(|state| state.scan_tasks(q, limit, cursor))
        }
        fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>> {
            self.read(|state| Ok(state.submissions.get(&id).cloned()))
        }
        fn scan_submissions(
            &self,
            q: &SubmissionQuery,
            limit: usize,
            cursor: Option<&Cursor>,
        ) -> StorageResult<Page<SubmissionRecord>> {
            self.read(|state| state.scan_submissions(q, limit, cursor))
        }
        fn submission_by_request(
            &self,
            c: ConversationId,
            r: &str,
        ) -> StorageResult<Option<SubmissionRecord>> {
            self.read(|state| Ok(state.submission_by_request(c, r)))
        }
        fn find_document(
            &self,
            a: &DocumentAddress,
            at: DocumentPoint,
        ) -> StorageResult<Option<DocumentRecord>> {
            self.read(|state| Ok(state.find_document(a, at)))
        }
        fn document(
            &self,
            id: DocumentId,
            at: DocumentPoint,
        ) -> StorageResult<Option<StoredDocument>> {
            self.read(|state| state.document(id, at))
        }
        fn scan_documents(
            &self,
            q: &DocumentQuery,
            limit: usize,
            cursor: Option<&Cursor>,
        ) -> StorageResult<Page<DocumentRecord>> {
            self.read(|state| state.scan_documents(q, limit, cursor))
        }
    };
}
pub(super) use state_reads;

pub(super) fn commit_result(stored: usize, refused: Option<StorageError>) -> CommitResult {
    match refused {
        Some(refused) => CommitResult::Refused { stored, refused },
        None => CommitResult::Stored { stored },
    }
}

pub struct MemoryStorage {
    state: Mutex<Option<State>>,
    commits: Mutex<()>,
}
impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}
impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(Some(State::default())),
            commits: Mutex::new(()),
        }
    }
    fn read<T>(&self, read: impl FnOnce(&State) -> StorageResult<T>) -> StorageResult<T> {
        read(
            lock(&self.state)?
                .as_ref()
                .ok_or_else(|| backend("MemoryStorage is closed"))?,
        )
    }
}
impl Storage for MemoryStorage {
    fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut dyn FnMut(usize, Seq),
    ) -> StorageResult<CommitResult> {
        let _write = lock(&self.commits)?;
        let mut seqs = Vec::with_capacity(batches.len());
        let mut refused = None;
        {
            let mut state = lock(&self.state)?;
            let state = state
                .as_mut()
                .ok_or_else(|| backend("MemoryStorage is closed"))?;
            for batch in batches {
                match state.prepare(batch, state.next_seq) {
                    Ok(prepared) => {
                        seqs.push(prepared.seq);
                        state.apply(prepared);
                    }
                    Err(err) => {
                        refused = Some(err);
                        break;
                    }
                }
            }
        }
        for (index, &seq) in seqs.iter().enumerate() {
            receive(index, seq);
        }
        Ok(commit_result(seqs.len(), refused))
    }
    fn mint_id(&self) -> StorageResult<u64> {
        lock(&self.state)?
            .as_mut()
            .ok_or_else(|| backend("MemoryStorage is closed"))?
            .mint_id()
    }
    state_reads!();
    fn close(&self) -> StorageResult<()> {
        *lock(&self.state)? = None;
        Ok(())
    }
}
