//! Durable owner tables and document scopes on Chord's StoreEngine.

mod commit;
mod documents;
mod tables;

use std::{
    collections::HashSet,
    future::Future,
    path::Path,
    pin::pin,
    sync::{Arc, Mutex, RwLock, RwLockReadGuard},
    task::{Context, Poll, Wake, Waker},
    thread,
};

use pi_rs_chord::{
    Error, Result,
    sqlite::{Options, SqliteDatabase},
    store::{Claimant, Step, StoreEngine},
};
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use tokio::runtime::{Builder, Runtime};

use crate::{
    ids::*,
    storage::{
        Storage, StorageError, StorageResult,
        scan::{MAX_SAFE_INTEGER, next_cursor, scan_start},
    },
    storage_types::*,
};

use self::tables::{Conditions, Table, Tables, encode_indexed};

const DEFAULT_OWNER: &str = "durable";

struct Owner {
    name: String,
    tables: Tables,
}

/// Storage scoped to one Durable owner of a Chord store. Closing an attached adapter
/// releases its owner claims and leaves the application-owned engine open.
pub struct StoreStorage {
    store: StoreEngine,
    owner: Arc<Owner>,
    claimant: Claimant,
    closed: RwLock<bool>,
    committing: Mutex<()>,
    runtime: Option<Runtime>,
}

impl StoreStorage {
    pub fn in_memory() -> StorageResult<Self> {
        Self::open(":memory:")
    }

    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_named(path, DEFAULT_OWNER)
    }

    pub fn open_named(path: impl AsRef<Path>, owner: &str) -> StorageResult<Self> {
        let runtime = Builder::new_current_thread().build()?;
        let result = {
            let _guard = runtime.enter();
            let db = wait(SqliteDatabase::open(path, Options::default()));
            db.and_then(|db| wait(StoreEngine::open(db)))
        }
        .map_err(storage_error)
        .and_then(|store| Self::from_store(store, owner));
        match result {
            Ok(mut result) => {
                result.runtime = Some(runtime);
                Ok(result)
            }
            Err(error) => {
                runtime.shutdown_background();
                Err(error)
            }
        }
    }

    pub fn from_store(store: StoreEngine, owner: &str) -> StorageResult<Self> {
        pi_rs_chord::contracts::check_name(owner, "owner").map_err(storage_error)?;
        let owner = Arc::new(Owner {
            name: owner.to_owned(),
            tables: Tables::new(owner).map_err(storage_error)?,
        });
        let claimant = Claimant::new(format!("StoreStorage({})", owner.name));
        let init = (|| {
            for table in owner.tables.all() {
                store.claims().table(table.name(), &claimant)?;
            }
            for namespace in documents::SCOPE_NAMESPACES {
                store
                    .claims()
                    .documents(&owner.name, namespace, &claimant)?;
            }
            let owner = Arc::clone(&owner);
            wait(store.database().transaction(move |c| {
                let mut step = Step::new(c)?;
                let mut changed = false;
                for table in owner.tables.all() {
                    changed |= table.prepare(&owner.name, &mut step)?;
                }
                if changed {
                    step.finish()?;
                }
                if let Some(highest) = owner.tables.ids.highest_id(c)? {
                    commit::raise(c, highest)?;
                }
                Ok(())
            }))
        })();
        if let Err(error) = init {
            store.claims().release(&claimant);
            return Err(storage_error(error));
        }
        Ok(Self {
            store,
            owner,
            claimant,
            closed: RwLock::new(false),
            committing: Mutex::new(()),
            runtime: None,
        })
    }

    fn opened(&self) -> StorageResult<RwLockReadGuard<'_, bool>> {
        let guard = self
            .closed
            .read()
            .map_err(|_| StorageError::Backend("StoreStorage close lock poisoned".into()))?;
        if *guard {
            return Err(StorageError::Backend("StoreStorage is closed".into()));
        }
        Ok(guard)
    }

    fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection, &Owner) -> Result<T> + Send + 'static,
    ) -> StorageResult<T> {
        let _open = self.opened()?;
        let owner = Arc::clone(&self.owner);
        wait(self.store.database().call(move |c| operation(c, &owner))).map_err(storage_error)
    }

    fn snapshot<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection, &Owner) -> Result<T> + Send + 'static,
    ) -> StorageResult<T> {
        self.read(move |c, owner| {
            let transaction = c.unchecked_transaction()?;
            let result = operation(&transaction, owner)?;
            transaction.commit()?;
            Ok(result)
        })
    }
}

impl Drop for StoreStorage {
    fn drop(&mut self) {
        self.store.claims().release(&self.claimant);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl Storage for StoreStorage {
    fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut dyn FnMut(usize, Seq),
    ) -> StorageResult<CommitResult> {
        let _committing = self
            .committing
            .lock()
            .map_err(|_| StorageError::Backend("StoreStorage commit lock poisoned".into()))?;
        let opened = self.opened()?;
        if batches.is_empty() {
            return Ok(CommitResult::Stored { stored: 0 });
        }
        if self.store.diverged() {
            return Err(StorageError::Backend("Chord store already diverged".into()));
        }
        let owner = Arc::clone(&self.owner);
        let staged = commit::stage(&owner.tables, &owner.name, batches);
        let (seqs, result) = wait(
            self.store
                .database()
                .transaction(move |c| commit::series(c, &owner.tables, &owner.name, staged)),
        )
        .map_err(storage_error)?;
        drop(opened);
        for (index, seq) in seqs.into_iter().enumerate() {
            receive(index, seq);
        }
        Ok(result)
    }

    fn publishing(&self, publish: &mut dyn FnMut()) {
        let batch = self.store.batch();
        if batch.running() {
            publish();
        } else if let Err(error) =
            batch.commit(&pi_rs_chord::context::Context::background(), || {
                publish();
                Ok(())
            })
        {
            batch.report_delivery(error);
        }
    }

    fn mint_id(&self) -> StorageResult<Id> {
        let _open = self.opened()?;
        wait(self.store.database().transaction(commit::mint)).map_err(storage_error)
    }

    fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>> {
        self.read(move |c, owner| owner.tables.conversations.get(c, number(id)))
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut conditions = Conditions::default();
        conditions.after(start.order, start.after);
        if let Some(id) = query.owner_conversation_id {
            conditions.add("owner_conversation_id=?", number(id));
        }
        if let Some(id) = query.owner_task_id {
            conditions.add("owner_task_id=?", number(id));
        }
        self.read(move |c, owner| {
            scan_table(
                c,
                &owner.tables.conversations,
                &conditions,
                start.order,
                limit,
                |row: &ConversationRecord| row.id,
            )
        })
    }

    fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
        self.read(move |c, owner| owner.tables.entries.entry(c, number(id)))
    }

    fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>> {
        self.snapshot(move |c, owner| {
            let tables = &owner.tables;
            let mut conversation = required_conversation(c, tables, conversation_id)?;
            let Some(stored) = tables.entries.entry(c, number(id))? else {
                return Ok(None);
            };
            let mut upper = MAX_SAFE_ID;
            let mut seen = HashSet::new();
            while conversation.id != stored.entry.conversation_id {
                let Some(parent) = conversation.parent else {
                    return Ok(None);
                };
                upper = upper.min(parent.at);
                if id > upper {
                    return Ok(None);
                }
                conversation = follow(
                    c,
                    tables,
                    &mut seen,
                    conversation.id,
                    parent.conversation_id,
                )?;
            }
            Ok(Some(stored))
        })
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        self.snapshot(move |c, owner| {
            let tables = &owner.tables;
            let mut conversation = required_conversation(c, tables, conversation_id)?;
            let mut upper = at_or_before_entry_id;
            let mut seen = HashSet::new();
            loop {
                let conditions = entry_conditions(conversation.id, None, upper);
                if let Some(head) = tables.heads.first_id(c, &conditions)? {
                    return tables.entries.get(c, head);
                }
                let Some(parent) = conversation.parent else {
                    return Ok(None);
                };
                upper = Some(upper.map_or(parent.at, |upper| upper.min(parent.at)));
                conversation = follow(
                    c,
                    tables,
                    &mut seen,
                    conversation.id,
                    parent.conversation_id,
                )?;
            }
        })
    }

    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Descending)?;
        let query = query.clone();
        self.snapshot(move |c, owner| {
            let tables = &owner.tables;
            let mut conversation = required_conversation(c, tables, query.conversation_id)?;
            let mut segments = Vec::new();
            let mut upper = query.max_entry_id;
            let mut seen = HashSet::new();
            let count = limit.saturating_add(1);
            let mut values = Vec::new();
            loop {
                if start.order == ScanOrder::Descending {
                    let mut conditions =
                        entry_conditions(conversation.id, query.min_entry_id, upper);
                    conditions.after(start.order, start.after);
                    tables.entries.select(
                        c,
                        &conditions,
                        start.order,
                        count - values.len(),
                        &mut values,
                    )?;
                    if values.len() == count {
                        break;
                    }
                } else {
                    segments.push((conversation.id, upper));
                }
                let Some(parent) = conversation.parent else {
                    break;
                };
                let cap = upper.map_or(parent.at, |upper| upper.min(parent.at));
                if query.min_entry_id.is_some_and(|lower| cap < lower) {
                    break;
                }
                upper = Some(cap);
                conversation = follow(
                    c,
                    tables,
                    &mut seen,
                    conversation.id,
                    parent.conversation_id,
                )?;
            }
            for (id, upper) in segments.into_iter().rev() {
                let mut conditions = entry_conditions(id, query.min_entry_id, upper);
                conditions.after(start.order, start.after);
                tables.entries.select(
                    c,
                    &conditions,
                    start.order,
                    count - values.len(),
                    &mut values,
                )?;
                if values.len() == count {
                    break;
                }
            }
            Ok(page(values, limit, start.order, |row: &EntryRecord| row.id))
        })
    }

    fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>> {
        self.read(move |c, owner| owner.tables.tasks.get(c, number(id)))
    }

    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut conditions = Conditions::default();
        conditions.after(start.order, start.after);
        if let Some(id) = query.conversation_id {
            conditions.add("conversation_id=?", number(id));
        }
        if let Some(kind) = &query.kind {
            conditions.add("kind=?", encode_indexed(kind).map_err(storage_error)?);
        }
        if let Some(status) = query.status {
            conditions.add("status=?", status.as_str().to_owned());
        }
        if let Some(abort) = query.abort_requested {
            conditions.add("abort_requested=?", i64::from(abort));
        }
        if let Some(background) = query.background {
            conditions.add("background=?", i64::from(background));
        }
        self.read(move |c, owner| {
            scan_table(
                c,
                &owner.tables.tasks,
                &conditions,
                start.order,
                limit,
                |row: &TaskRecord| row.id,
            )
        })
    }

    fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>> {
        self.read(move |c, owner| owner.tables.submissions.get(c, number(id)))
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>> {
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut conditions = Conditions::default();
        conditions.after(start.order, start.after);
        if let Some(id) = query.conversation_id {
            conditions.add("conversation_id=?", number(id));
        }
        if let Some(status) = query.status {
            conditions.add("status=?", status.as_str().to_owned());
        }
        self.read(move |c, owner| {
            scan_table(
                c,
                &owner.tables.submissions,
                &conditions,
                start.order,
                limit,
                |row: &SubmissionRecord| row.id,
            )
        })
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> StorageResult<Option<SubmissionRecord>> {
        let mut conditions = Conditions::default();
        conditions.add("conversation_id=?", number(conversation_id));
        conditions.add(
            "request_id=?",
            encode_indexed(request_id).map_err(storage_error)?,
        );
        self.read(move |c, owner| {
            let mut rows = Vec::new();
            owner
                .tables
                .submissions
                .select(c, &conditions, ScanOrder::Ascending, 1, &mut rows)?;
            Ok(rows.pop())
        })
    }

    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> StorageResult<Option<DocumentRecord>> {
        let address = address.clone();
        self.read(move |c, owner| documents::find(c, &owner.name, &address, at))
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> StorageResult<Option<StoredDocument>> {
        self.snapshot(move |c, owner| documents::load(c, &owner.name, number(id), at))
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>> {
        let after = match cursor.and_then(|cursor| cursor.get("after")) {
            None => -1,
            Some(after) => after
                .as_i64()
                .filter(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER)
                .ok_or_else(|| StorageError::Request("Invalid storage cursor".into()))?,
        };
        let query = query.clone();
        self.read(move |c, owner| {
            let values = documents::scan(c, &owner.name, &query, after, limit.saturating_add(1))?;
            Ok(page(values, limit, ScanOrder::Ascending, |row| row.id))
        })
    }

    fn close(&self) -> StorageResult<()> {
        let mut closed = self
            .closed
            .write()
            .map_err(|_| StorageError::Backend("StoreStorage close lock poisoned".into()))?;
        if !*closed {
            *closed = true;
            self.store.claims().release(&self.claimant);
            if self.runtime.is_some() {
                wait(self.store.database().close()).map_err(storage_error)?;
            }
        }
        Ok(())
    }
}

fn required_conversation(
    c: &Connection,
    tables: &Tables,
    id: ConversationId,
) -> Result<ConversationRecord> {
    tables
        .conversations
        .get(c, number(id))?
        .ok_or_else(|| Error::Invalid(format!("Unknown conversation: {id}")))
}

fn follow(
    c: &Connection,
    tables: &Tables,
    seen: &mut HashSet<ConversationId>,
    previous: ConversationId,
    next: ConversationId,
) -> Result<ConversationRecord> {
    if !seen.insert(previous) {
        return Err(Error::Invalid(format!(
            "Conversation fork cycle at {previous}"
        )));
    }
    required_conversation(c, tables, next)
}

fn entry_conditions(
    conversation_id: ConversationId,
    lower: Option<EntryId>,
    upper: Option<EntryId>,
) -> Conditions {
    let mut conditions = Conditions::default();
    conditions.add("conversation_id=?", number(conversation_id));
    if let Some(lower) = lower {
        conditions.add("id>=?", number(lower));
    }
    if let Some(upper) = upper {
        conditions.add("id<=?", number(upper));
    }
    conditions
}

fn scan_table<T: DeserializeOwned>(
    c: &Connection,
    table: &Table,
    conditions: &Conditions,
    order: ScanOrder,
    limit: usize,
    id: impl FnOnce(&T) -> Id,
) -> Result<Page<T>> {
    let mut values = Vec::new();
    table.select(c, conditions, order, limit.saturating_add(1), &mut values)?;
    Ok(page(values, limit, order, id))
}

fn page<T>(
    mut items: Vec<T>,
    limit: usize,
    order: ScanOrder,
    id: impl FnOnce(&T) -> Id,
) -> Page<T> {
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|last| next_cursor(id(last), order))
    } else {
        None
    };
    Page { items, next }
}

fn checked_id(id: Id) -> Result<i64> {
    if !(1..=MAX_SAFE_ID).contains(&id) {
        return Err(Error::Invalid(format!("Invalid storage ID: {id}")));
    }
    Ok(number(id))
}

fn checked_version(version: u64) -> Result<i64> {
    if !(1..=MAX_SAFE_ID).contains(&version) {
        return Err(Error::Invalid(format!(
            "Invalid document version: {version}"
        )));
    }
    Ok(number(version))
}

fn unsigned(value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::Invalid(format!("Negative Chord ID or sequence: {value}")))
}

fn number(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn storage_error(error: Error) -> StorageError {
    match error {
        Error::Sqlite(error) => StorageError::Sqlite(error),
        Error::Io(error) => StorageError::Io(error),
        Error::Json(error) => StorageError::Json(error),
        Error::Invalid(message) => StorageError::Request(message),
        error => StorageError::Backend(error.to_string()),
    }
}

// Chord runs SQLite on its own thread. Wait on its result without starting a nested
// Tokio executor; the synchronous Storage caller blocks, not the SQLite actor.
fn wait<F: Future>(future: F) -> F::Output {
    struct Waiting(thread::Thread);
    impl Wake for Waiting {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Waiting(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(tokio::task::unconstrained(future));
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
