use super::memory::{
    DocumentState, Prepared, State, Table, backend, commit_result, document_id, lock, materialize,
    record_claim,
};
use super::scan::{MAX_SAFE_INTEGER, ScanStart, document_scan_start, paginate, scan_start};
use super::*;
use pi_rs_chord::sqlite::{apply_migrations_in, validate_sqlite_schema_prefix};
use rusqlite::types::Value as Cell;
use rusqlite::{
    Connection, OptionalExtension, Params, TransactionBehavior, params, params_from_iter,
};
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

pub use super::sqlite_schema::CURRENT_SQLITE_SCHEMA_VERSION;

#[derive(Clone, Debug, Default)]
pub struct SqliteStorageOptions {
    pub prefix: Option<String>,
}

pub struct SqliteStorage {
    prefix: String,
    connection: Mutex<Option<(Connection, u64)>>,
    commits: Mutex<()>,
}

impl SqliteStorage {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_with_options(path, SqliteStorageOptions::default())
    }
    pub fn open_with_options(
        path: impl AsRef<Path>,
        options: SqliteStorageOptions,
    ) -> StorageResult<Self> {
        if let Some(prefix) = &options.prefix {
            validate_sqlite_schema_prefix(prefix).map_err(|err| backend(err.to_string()))?;
        }
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(5_000))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "wal_autocheckpoint", 1_000)?;
        Self::from_connection(connection, options)
    }
    pub fn in_memory() -> StorageResult<Self> {
        Self::from_connection(
            Connection::open_in_memory()?,
            SqliteStorageOptions::default(),
        )
    }
    pub fn from_connection(
        mut connection: Connection,
        options: SqliteStorageOptions,
    ) -> StorageResult<Self> {
        if let Some(prefix) = &options.prefix {
            validate_sqlite_schema_prefix(prefix).map_err(|err| backend(err.to_string()))?;
        }
        let prefix = options.prefix.unwrap_or_default();
        connection.set_prepared_statement_cache_capacity(256);
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply_migrations_in(
            &transaction,
            &format!("{prefix}durable_schema"),
            &super::sqlite_schema::migrations(&prefix),
        )
        .map_err(|err| backend(err.to_string()))?;
        transaction.commit()?;
        let (next_id, _) = Db {
            connection: &connection,
            p: &prefix,
        }
        .metadata()?;
        Ok(Self {
            prefix,
            connection: Mutex::new(Some((connection, next_id))),
            commits: Mutex::new(()),
        })
    }
    fn read<T>(&self, f: impl FnOnce(Db<'_>) -> StorageResult<T>) -> StorageResult<T> {
        let connection = lock(&self.connection)?;
        let (connection, _) = connection
            .as_ref()
            .ok_or_else(|| backend("SqliteStorage is closed"))?;
        f(Db {
            connection,
            p: &self.prefix,
        })
    }
    fn snapshot<T>(&self, f: impl FnOnce(Db<'_>) -> StorageResult<T>) -> StorageResult<T> {
        let mut connection = lock(&self.connection)?;
        let (connection, _) = connection
            .as_mut()
            .ok_or_else(|| backend("SqliteStorage is closed"))?;
        let transaction = connection.transaction()?;
        let result = f(Db {
            connection: &transaction,
            p: &self.prefix,
        })?;
        transaction.commit()?;
        Ok(result)
    }
}

struct Db<'a> {
    connection: &'a Connection,
    p: &'a str,
}
impl Db<'_> {
    fn metadata(&self) -> StorageResult<(u64, Seq)> {
        let p = self.p;
        let (next_id, seq): (String, Seq) = self.connection.query_row(
            &format!("SELECT next_id,next_seq FROM {p}durable_metadata WHERE singleton=1"),
            [],
            |row| Ok((row.get(0)?, nonnegative(row, 1)?)),
        )?;
        Ok((
            next_id
                .parse()
                .map_err(|_| backend("Invalid Durable SQLite next ID"))?,
            seq,
        ))
    }
    fn one<T: DeserializeOwned>(&self, sql: &str, params: impl Params) -> StorageResult<Option<T>> {
        self.connection
            .prepare_cached(sql)?
            .query_row(params, |row| row.get::<_, String>(0))
            .optional()?
            .map(|row| serde_json::from_str(&row).map_err(Into::into))
            .transpose()
    }
    fn record<T: DeserializeOwned>(&self, table: &str, id: u64) -> StorageResult<Option<T>> {
        self.one(
            &format!("SELECT record FROM {}{table} WHERE id=?", self.p),
            [sql_int(id)?],
        )
    }
    fn run(&self, sql: &str, params: impl Params) -> StorageResult<()> {
        self.connection.prepare_cached(sql)?.execute(params)?;
        Ok(())
    }
    fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
        let p = self.p;
        self.connection
            .prepare_cached(&format!(
                "SELECT record,commit_seq FROM {p}entries WHERE id=?"
            ))?
            .query_row([sql_int(id)?], |row| {
                Ok((row.get::<_, String>(0)?, nonnegative(row, 1)?))
            })
            .optional()?
            .map(|(record, commit_seq)| {
                Ok(StoredEntry {
                    entry: serde_json::from_str(&record)?,
                    commit_seq,
                })
            })
            .transpose()
    }
    fn rows<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl Params,
        output: &mut Vec<T>,
    ) -> StorageResult<()> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let mut rows = statement.query(params)?;
        while let Some(row) = rows.next()? {
            output.push(serde_json::from_str(
                row.get_ref(0)?
                    .as_str()
                    .map_err(|err| backend(err.to_string()))?,
            )?);
        }
        Ok(())
    }
    fn scan<T: DeserializeOwned>(
        &self,
        table: &str,
        mut filters: Filters,
        start: ScanStart,
        limit: usize,
        id: impl Fn(&T) -> u64,
    ) -> StorageResult<Page<T>> {
        let p = self.p;
        let direction = direction(start.order);
        match start.order {
            ScanOrder::Ascending => filters.push("id > ?", start.after.unwrap_or(-1)),
            ScanOrder::Descending => {
                filters.push("id < ?", start.after.unwrap_or(MAX_SAFE_INTEGER as i64 + 1))
            }
        }
        filters
            .params
            .push(sql_limit(limit.saturating_add(1)).into());
        let clauses = filters.clauses.join(" AND ");
        let mut rows = Vec::new();
        self.rows(
            &format!(
                "SELECT record FROM {p}{table} WHERE {clauses} ORDER BY id {direction} LIMIT ?"
            ),
            params_from_iter(filters.params),
            &mut rows,
        )?;
        Ok(paginate(rows.into_iter(), id, limit, start.order))
    }
    fn walk(
        &self,
        conversation_id: ConversationId,
        mut upper: u64,
        min: u64,
        mut visit: impl FnMut(ConversationId, u64) -> StorageResult<bool>,
    ) -> StorageResult<()> {
        let mut visited = BTreeSet::new();
        let mut current = conversation_id;
        loop {
            let conversation = self
                .record::<ConversationRecord>("conversations", current)?
                .ok_or_else(|| StorageError::Request(format!("Unknown conversation: {current}")))?;
            if upper < min {
                return Ok(());
            }
            if !visited.insert(current) {
                return Err(backend("Cycle in conversation ancestry"));
            }
            if !visit(current, upper)? {
                return Ok(());
            }
            let Some(parent) = conversation.parent else {
                return Ok(());
            };
            upper = upper.min(parent.at);
            if upper < min {
                return Ok(());
            }
            current = parent.conversation_id;
        }
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> StorageResult<Option<DocumentRecord>> {
        let p = self.p;
        let (scope, owner) = address.scope.columns();
        let mut filters = Filters::default();
        filters.push("kind = ?", serde_json::to_string(&address.kind)?);
        filters.push("scope_kind = ?", scope.to_owned());
        filters.push("owner_id = ?", owner as i64);
        filters.push("family = ?", i64::from(address.key.is_some()));
        filters.push(
            "key_value = ?",
            serde_json::to_string(address.key.as_deref().unwrap_or(""))?,
        );
        filters.alive_at(at);
        let clauses = filters.clauses.join(" AND ");
        self.one(
            &format!(
                "SELECT record FROM {p}documents WHERE {clauses} ORDER BY created_at DESC LIMIT 1"
            ),
            params_from_iter(filters.params),
        )
    }
    fn revisions(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        all: bool,
    ) -> StorageResult<Vec<(Seq, DocumentContent)>> {
        let p = self.p;
        let upper = match at {
            DocumentPoint::Current => MAX_SAFE_INTEGER,
            DocumentPoint::Seq(seq) => seq,
        };
        let lower = if all {
            0
        } else {
            let base = self.connection.prepare_cached(&format!("SELECT seq FROM {p}document_revisions WHERE document_id=? AND kind='base' AND seq<=? ORDER BY seq DESC LIMIT 1"))?.query_row(params![sql_int(id)?,sql_int(upper)?],|row| row.get::<_,i64>(0)).optional()?;
            base.ok_or_else(|| backend(format!("Document {id} is missing a required base")))?
        };
        let mut stmt = self.connection.prepare_cached(&format!("SELECT seq,kind,version,content FROM {p}document_revisions WHERE document_id=? AND seq>=? AND seq<=? ORDER BY seq"))?;
        let mut rows = stmt.query(params![sql_int(id)?, lower, sql_int(upper)?])?;
        let mut revisions = Vec::new();
        while let Some(row) = rows.next()? {
            let seq = nonnegative(row, 0)?;
            let kind = row.get::<_, String>(1)?;
            let version = nonnegative(row, 2)?;
            let raw = row
                .get_ref(3)?
                .as_str()
                .map_err(|err| backend(err.to_string()))?;
            let content = match kind.as_str() {
                "base" => DocumentContent::Base {
                    version,
                    value: serde_json::from_str(raw)?,
                },
                "delta" => DocumentContent::Delta {
                    version,
                    ops: crate::storage_types::ops_wire::deserialize(
                        &mut serde_json::Deserializer::from_str(raw),
                    )?,
                },
                _ => return Err(backend(format!("Invalid document content kind {kind}"))),
            };
            revisions.push((seq, content));
        }
        Ok(revisions)
    }
    fn materialize(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageResult<Option<StoredDocument>> {
        let Some(record) = self.record::<DocumentRecord>("documents", id)? else {
            return Ok(None);
        };
        if (at != DocumentPoint::Current && record.current_only()) || !record.alive_at(at) {
            return materialize(&record, &[], at);
        }
        materialize(&record, &self.revisions(id, at, false)?, at)
    }
    fn validation_state(
        &self,
        batch: &[StorageWrite],
        next_id: u64,
        seq: Seq,
    ) -> StorageResult<State> {
        let p = self.p;
        let mut state = State::default();
        state.next_id = next_id;
        state.next_seq = seq;
        let mut docs = BTreeSet::new();
        let mut revisions = BTreeSet::new();
        let mut deltas = BTreeSet::new();
        let mut claims = BTreeSet::new();
        for write in batch {
            if let Some((id, _)) = record_claim(write)
                && id <= MAX_SAFE_INTEGER
            {
                claims.insert(id);
            }
            if let Some(id) = document_id(write)
                && id <= MAX_SAFE_INTEGER
            {
                docs.insert(id);
            }
            match write {
                StorageWrite::DocumentCopy { record, source } => {
                    if source.id <= MAX_SAFE_INTEGER {
                        docs.insert(source.id);
                        revisions.insert(source.id);
                    }
                    if let Some(existing) =
                        self.find_document(&record.address(), DocumentPoint::Current)?
                    {
                        docs.insert(existing.id);
                    }
                }
                StorageWrite::DocumentCreate { record, .. } => {
                    if let Some(existing) =
                        self.find_document(&record.address(), DocumentPoint::Current)?
                    {
                        docs.insert(existing.id);
                    }
                }
                StorageWrite::DocumentChange {
                    id,
                    content: DocumentContent::Delta { .. },
                } => {
                    deltas.insert(*id);
                }
                _ => {}
            }
        }
        if !claims.is_empty() {
            let mut statement = self.connection.prepare_cached(&format!("SELECT id,record_type FROM {p}record_ids WHERE id IN (SELECT value FROM json_each(?))"))?;
            let encoded = serde_json::to_string(&claims)?;
            let mut rows = statement.query([encoded])?;
            while let Some(row) = rows.next()? {
                let table = match row
                    .get_ref(1)?
                    .as_str()
                    .map_err(|err| backend(err.to_string()))?
                {
                    "conversation" => Table::Conversation,
                    "entry" => Table::Entry,
                    "task" => Table::Task,
                    "submission" => Table::Submission,
                    "document" => Table::Document,
                    _ => return Err(backend("Invalid SQLite record type")),
                };
                state.ids.insert(nonnegative(row, 0)?, table);
            }
        }
        for id in docs {
            if let Some(record) = self.record::<DocumentRecord>("documents", id)? {
                let contents = if revisions.contains(&id) {
                    self.revisions(id, DocumentPoint::Current, true)?
                } else {
                    if deltas.contains(&id) {
                        let version = self.connection.prepare_cached(&format!("SELECT version FROM {p}document_revisions WHERE document_id=? ORDER BY seq DESC LIMIT 1"))?.query_row([sql_int(id)?],|row| nonnegative(row,0)).optional()?;
                        if let Some(version) = version {
                            state.indexed_versions.insert(id, version);
                        }
                    }
                    Vec::new()
                };
                state.insert_document(DocumentState {
                    record,
                    revisions: contents,
                });
            }
        }
        Ok(state)
    }
    fn write(&self, state: &State, prepared: &Prepared) -> StorageResult<u64> {
        let p = self.p;
        let seq = prepared.seq;
        let mut next_id = state.next_id;
        for write in &prepared.writes {
            if let Some((id, table)) = record_claim(write) {
                self.run(
                    &format!("INSERT OR IGNORE INTO {p}record_ids (id,record_type) VALUES (?,?)"),
                    params![sql_int(id)?, table.name()],
                )?;
                next_id = next_id.max(id + 1);
            }
            match write {
                StorageWrite::Conversation { value } => self.run(&format!("INSERT INTO {p}conversations (id,owner_conversation_id,owner_task_id,record) VALUES (?,?,?,?)"),params![sql_int(value.id)?,value.owner.map(|o| sql_int(o.conversation_id)).transpose()?,value.owner.map(|o| sql_int(o.task_id)).transpose()?,serde_json::to_string(value)?])?,
                StorageWrite::Entry { value } => self.run(&format!("INSERT INTO {p}entries (id,conversation_id,head,commit_seq,record) VALUES (?,?,?,?,?)"),params![sql_int(value.id)?,sql_int(value.conversation_id)?,value.head.map(sql_int).transpose()?,sql_int(seq)?,serde_json::to_string(value)?])?,
                StorageWrite::Task { value } => self.run(&format!("INSERT INTO {p}tasks (id,conversation_id,kind,status,abort_requested,background,record) VALUES (?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET conversation_id=excluded.conversation_id,kind=excluded.kind,status=excluded.status,abort_requested=excluded.abort_requested,background=excluded.background,record=excluded.record"),params![sql_int(value.id)?,sql_int(value.conversation_id)?,serde_json::to_string(&value.kind)?,value.state.status().as_str(),value.abort_requested,value.background,serde_json::to_string(value)?])?,
                StorageWrite::Submission { value } => self.run(&format!("INSERT INTO {p}submissions (id,conversation_id,request_id,status,record) VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET conversation_id=excluded.conversation_id,request_id=excluded.request_id,status=excluded.status,record=excluded.record"),params![sql_int(value.id)?,sql_int(value.conversation_id)?,value.request_id.as_ref().map(serde_json::to_string).transpose()?,value.status.as_str(),serde_json::to_string(value)?])?,
                _ => {}
            }
        }
        for (&id, action) in &prepared.actions {
            let current_only;
            if let Some(create) = &action.create {
                let record = create.clone().stamp(seq, action.retire.then_some(seq));
                current_only = record.current_only();
                let (scope, owner) = record.scope.columns();
                self.run(&format!("INSERT INTO {p}documents (id,kind,family,key_value,scope_kind,owner_id,created_at,retired_at,record) VALUES (?,?,?,?,?,?,?,?,?)"),params![sql_int(id)?,serde_json::to_string(&record.kind)?,record.key.is_some(),serde_json::to_string(record.key.as_deref().unwrap_or(""))?,scope,sql_int(owner)?,sql_int(seq)?,record.retired_at.map(sql_int).transpose()?,serde_json::to_string(&record)?])?;
            } else {
                let record = &state
                    .documents
                    .get(&id)
                    .ok_or_else(|| backend(format!("Unknown document: {id}")))?
                    .record;
                current_only = record.current_only();
                if action.retire {
                    let mut record = record.clone();
                    record.retired_at = Some(seq);
                    self.run(
                        &format!("UPDATE {p}documents SET retired_at=?,record=? WHERE id=?"),
                        params![sql_int(seq)?, serde_json::to_string(&record)?, sql_int(id)?],
                    )?;
                }
            }
            if current_only
                && (action.retire || matches!(action.content, Some(DocumentContent::Base { .. })))
            {
                self.run(
                    &format!("DELETE FROM {p}document_revisions WHERE document_id=?"),
                    [sql_int(id)?],
                )?;
            }
            if let Some(content) = &action.content
                && !(action.retire && current_only)
            {
                let (kind, version, raw) = match content {
                    DocumentContent::Base { version, value } => {
                        ("base", version, serde_json::to_string(value)?)
                    }
                    DocumentContent::Delta { version, ops } => {
                        let mut data = Vec::new();
                        crate::storage_types::ops_wire::serialize(
                            ops,
                            &mut serde_json::Serializer::new(&mut data),
                        )?;
                        (
                            "delta",
                            version,
                            String::from_utf8(data).map_err(|err| backend(err.to_string()))?,
                        )
                    }
                };
                self.run(&format!("INSERT INTO {p}document_revisions (document_id,seq,kind,version,content) VALUES (?,?,?,?,?)"),params![sql_int(id)?,sql_int(seq)?,kind,sql_int(*version)?,raw])?;
            }
        }
        Ok(next_id)
    }
}

#[derive(Default)]
struct Filters {
    clauses: Vec<&'static str>,
    params: Vec<Cell>,
}
impl Filters {
    fn push(&mut self, clause: &'static str, value: impl Into<Cell>) {
        self.clauses.push(clause);
        self.params.push(value.into());
    }
    fn id(&mut self, clause: &'static str, value: Option<u64>) {
        if let Some(id) = value {
            self.push(clause, id.min(i64::MAX as u64) as i64);
        }
    }
    fn alive_at(&mut self, at: DocumentPoint) {
        match at {
            DocumentPoint::Current => self.clauses.push("retired_at IS NULL"),
            DocumentPoint::Seq(seq) => {
                let seq = seq.min(i64::MAX as u64) as i64;
                self.push("created_at <= ?", seq);
                self.push("(retired_at IS NULL OR retired_at > ?)", seq);
            }
        }
    }
}
fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}
fn direction(order: ScanOrder) -> &'static str {
    match order {
        ScanOrder::Ascending => "ASC",
        ScanOrder::Descending => "DESC",
    }
}

impl Storage for SqliteStorage {
    fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut dyn FnMut(usize, Seq),
    ) -> StorageResult<CommitResult> {
        let _write = lock(&self.commits)?;
        let mut seqs = Vec::with_capacity(batches.len());
        let mut refused = None;
        {
            let mut connection = lock(&self.connection)?;
            let (connection, minted) = connection
                .as_mut()
                .ok_or_else(|| backend("SqliteStorage is closed"))?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let db = Db {
                connection: &transaction,
                p: &self.prefix,
            };
            let (stored_id, mut seq) = db.metadata()?;
            let mut next_id = stored_id.max(*minted);
            for batch in batches {
                let state = db.validation_state(batch, next_id, seq)?;
                let prepared = match state.prepare(batch, seq) {
                    Ok(prepared) => prepared,
                    Err(err) => {
                        refused = Some(err);
                        break;
                    }
                };
                next_id = db.write(&state, &prepared)?;
                seqs.push(seq);
                seq += 1;
            }
            if !seqs.is_empty() {
                db.run(
                    &format!(
                        "UPDATE {}durable_metadata SET next_id=?,next_seq=? WHERE singleton=1",
                        self.prefix
                    ),
                    params![next_id.to_string(), sql_int(seq)?],
                )?;
            }
            transaction.commit()?;
            *minted = next_id;
        }
        for (index, &seq) in seqs.iter().enumerate() {
            receive(index, seq);
        }
        Ok(commit_result(seqs.len(), refused))
    }
    fn mint_id(&self) -> StorageResult<u64> {
        let mut connection = lock(&self.connection)?;
        let (connection, next) = connection
            .as_mut()
            .ok_or_else(|| backend("SqliteStorage is closed"))?;
        *next = (*next).max(
            Db {
                connection,
                p: &self.prefix,
            }
            .metadata()?
            .0,
        );
        if *next > MAX_SAFE_INTEGER {
            return Err(backend("ID space is exhausted"));
        }
        let id = *next;
        *next += 1;
        Ok(id)
    }
    fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>> {
        self.read(|db| db.record("conversations", id))
    }
    fn scan_conversations(
        &self,
        q: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>> {
        self.read(|db| {
            let mut filters = Filters::default();
            filters.id("owner_conversation_id = ?", q.owner_conversation_id);
            filters.id("owner_task_id = ?", q.owner_task_id);
            db.scan(
                "conversations",
                filters,
                scan_start(q.order, cursor, ScanOrder::Ascending)?,
                limit,
                |r: &ConversationRecord| r.id,
            )
        })
    }
    fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
        self.read(|db| db.entry(id))
    }
    fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>> {
        self.snapshot(|db| {
            let entry = db.entry(id)?;
            let mut visible = false;
            db.walk(conversation_id, id, id, |conv, _| {
                visible = entry
                    .as_ref()
                    .is_some_and(|r| r.entry.conversation_id == conv);
                Ok(entry.is_some() && !visible)
            })?;
            Ok(entry.filter(|_| visible))
        })
    }
    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        self.snapshot(|db| {
            let p=db.p;
            let sql = format!("SELECT record FROM {p}entries WHERE conversation_id=? AND id<=? AND head IS NOT NULL ORDER BY id DESC LIMIT 1");
            let mut found = None;
            db.walk(conversation_id,at.unwrap_or(MAX_SAFE_INTEGER).min(MAX_SAFE_INTEGER),0,|conv,upper| { found=db.one(&sql,params![sql_int(conv)?,sql_int(upper)?])?; Ok(found.is_none()) })?;
            Ok(found)
        })
    }
    fn scan_entries(
        &self,
        q: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>> {
        self.snapshot(|db| {
            let start=scan_start(q.order,cursor,ScanOrder::Descending)?;
            let mut min=q.min_entry_id.unwrap_or(0).min(MAX_SAFE_INTEGER+1) as i64;
            let mut max=q.max_entry_id.unwrap_or(MAX_SAFE_INTEGER).min(MAX_SAFE_INTEGER) as i64;
            if let Some(after)=start.after { match start.order { ScanOrder::Ascending => min=min.max(after+1), ScanOrder::Descending => max=max.min(after-1) } }
            let p=db.p;
            let direction=direction(start.order);
            let sql=format!("SELECT record FROM {p}entries WHERE conversation_id=? AND id>=? AND id<=? ORDER BY id {direction} LIMIT ?");
            let mut output=Vec::new();
            let capacity=limit.saturating_add(1);
            let mut segments=Vec::new();
            db.walk(q.conversation_id,max.max(0) as u64,min as u64,|conv,upper| {
                if start.order == ScanOrder::Ascending { segments.push((conv,upper)); }
                else { db.rows(&sql,params![sql_int(conv)?,min,max.min(upper as i64),sql_limit(capacity-output.len())],&mut output)?; }
                Ok(output.len()<capacity)
            })?;
            for (conv,upper) in segments.into_iter().rev() {
                db.rows(&sql,params![sql_int(conv)?,min,max.min(upper as i64),sql_limit(capacity-output.len())],&mut output)?;
                if output.len()>=capacity { break; }
            }
            Ok(paginate(output.into_iter(),|record:&EntryRecord|record.id,limit,start.order))
        })
    }
    fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>> {
        self.read(|db| db.record("tasks", id))
    }
    fn scan_tasks(
        &self,
        q: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>> {
        self.read(|db| {
            let mut filters = Filters::default();
            filters.id("conversation_id = ?", q.conversation_id);
            if let Some(kind) = &q.kind {
                filters.push("kind = ?", serde_json::to_string(kind)?);
            }
            if let Some(status) = q.status {
                filters.push("status = ?", status.as_str().to_owned());
            }
            if let Some(abort) = q.abort_requested {
                filters.push("abort_requested = ?", i64::from(abort));
            }
            if let Some(background) = q.background {
                filters.push("background = ?", i64::from(background));
            }
            db.scan(
                "tasks",
                filters,
                scan_start(q.order, cursor, ScanOrder::Ascending)?,
                limit,
                |r: &TaskRecord| r.id,
            )
        })
    }
    fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>> {
        self.read(|db| db.record("submissions", id))
    }
    fn scan_submissions(
        &self,
        q: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>> {
        self.read(|db| {
            let mut filters = Filters::default();
            filters.id("conversation_id = ?", q.conversation_id);
            if let Some(status) = q.status {
                filters.push("status = ?", status.as_str().to_owned());
            }
            db.scan(
                "submissions",
                filters,
                scan_start(q.order, cursor, ScanOrder::Ascending)?,
                limit,
                |r: &SubmissionRecord| r.id,
            )
        })
    }
    fn submission_by_request(
        &self,
        c: ConversationId,
        request: &str,
    ) -> StorageResult<Option<SubmissionRecord>> {
        self.read(|db| {
            let p = db.p;
            db.one(
                &format!(
                    "SELECT record FROM {p}submissions WHERE conversation_id=? AND request_id=?"
                ),
                params![sql_int(c)?, serde_json::to_string(request)?],
            )
        })
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.read(|db| db.find_document(address, at))
    }
    fn document(&self, id: DocumentId, at: DocumentPoint) -> StorageResult<Option<StoredDocument>> {
        self.snapshot(|db| db.materialize(id, at))
    }
    fn scan_documents(
        &self,
        q: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>> {
        self.read(|db| {
            let (scope, owner) = q.scope.columns();
            let mut filters = Filters::default();
            filters.push("scope_kind = ?", scope.to_owned());
            filters.push("owner_id = ?", owner as i64);
            if let Some(kind) = &q.kind {
                filters.push("kind = ?", serde_json::to_string(kind)?);
            }
            filters.alive_at(q.at);
            db.scan(
                "documents",
                filters,
                document_scan_start(cursor)?,
                limit,
                |r: &DocumentRecord| r.id,
            )
        })
    }
    fn close(&self) -> StorageResult<()> {
        *lock(&self.connection)? = None;
        Ok(())
    }
}

fn sql_int(value: u64) -> StorageResult<i64> {
    i64::try_from(value).map_err(|_| backend(format!("Value {value} exceeds SQLite INTEGER range")))
}

fn nonnegative(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(column)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}
