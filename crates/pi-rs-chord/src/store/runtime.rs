//! FIFO store transactions and replica admission.
use super::{
    Claims, MetaRow, OutboxCommit, Page, Query, StoreWrite, applier, read_meta,
    table::{invalid, safe},
};
use crate::{
    Error, Result,
    sqlite::{SqliteDatabase, SqliteFuture},
};
use rusqlite::{Connection, params, params_from_iter};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;
#[derive(Clone)]
pub struct StoreEngine {
    inner: Arc<Inner>,
}
struct Inner {
    db: SqliteDatabase,
    claims: Claims,
    diverged: AtomicBool,
    changed: watch::Sender<i64>,
    batch: crate::state::Batch,
    observers: Mutex<Vec<Weak<super::live_query::LiveInner>>>,
    runtime: tokio::runtime::Handle,
}
impl StoreEngine {
    pub async fn open(db: SqliteDatabase) -> Result<Self> {
        Self::open_with_batch(db, crate::state::Batch::new(|_| {})).await
    }
    pub async fn open_with_batch(db: SqliteDatabase, batch: crate::state::Batch) -> Result<Self> {
        let meta = super::apply_store_schema(&db).await?;
        let (changed, _) = watch::channel(meta.last_seq);
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                claims: Claims::default(),
                diverged: AtomicBool::new(false),
                changed,
                batch,
                observers: Mutex::new(Vec::new()),
                runtime: tokio::runtime::Handle::current(),
            }),
        })
    }
    pub fn batch(&self) -> crate::state::Batch {
        self.inner.batch.clone()
    }
    pub fn live_query(&self, shape: super::LiveShape) -> Result<super::LiveQuery> {
        let live = super::LiveQuery::new(self.batch(), shape)?;
        crate::context::lock(&self.inner.observers).push(live.weak());
        Ok(live)
    }
    pub(crate) fn prepare_live(&self, c: &Connection) -> Vec<super::live_query::LiveRun> {
        super::live_query::prepare(c, &self.inner.observers)
    }
    pub(crate) fn adopt_live(&self, runs: Vec<super::live_query::LiveRun>) -> Result<()> {
        let _runtime = self.inner.runtime.enter();
        self.inner
            .batch
            .within(&crate::context::Context::background(), move || {
                super::live_query::adopt(runs)
            })
    }
    pub fn database(&self) -> &SqliteDatabase {
        &self.inner.db
    }
    pub fn claims(&self) -> &Claims {
        &self.inner.claims
    }
    pub fn diverged(&self) -> bool {
        self.inner.diverged.load(Ordering::Acquire)
    }
    pub fn meta(&self) -> SqliteFuture<MetaRow> {
        self.inner.db.call(read_meta)
    }
    pub(crate) fn publish(&self, seq: i64) {
        self.inner.changed.send_replace(seq);
    }
    pub fn changes(&self) -> watch::Receiver<i64> {
        self.inner.changed.subscribe()
    }
    pub fn commit(&self, writes: Vec<StoreWrite>) -> SqliteFuture<OutboxCommit> {
        self.commit_step(
            move |step| {
                for write in writes {
                    step.apply(write)?;
                }
                Ok(())
            },
            |(), commit| commit,
            None,
        )
    }
    /// Run an atomic read/write step against the store's declared tables and documents.
    pub fn transact<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut super::Step<'_>) -> Result<T> + Send + 'static,
    ) -> SqliteFuture<T> {
        self.commit_step(apply, |value, _| value, None)
    }
    pub(super) fn change_host_data(
        &self,
        address: crate::contracts::Address,
        owners: super::HostDataOwners,
        apply: impl FnOnce(&mut super::Step<'_>) -> Result<super::HostDataCounts> + Send + 'static,
    ) -> SqliteFuture<super::HostDataCounts> {
        self.commit_step(apply, |counts, _| counts, Some((address, owners)))
    }
    fn commit_step<T: Send + 'static, R: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut super::Step<'_>) -> Result<T> + Send + 'static,
        finish: impl FnOnce(T, OutboxCommit) -> R + Send + 'static,
        ownership: Option<(crate::contracts::Address, super::HostDataOwners)>,
    ) -> SqliteFuture<R> {
        let engine = self.clone();
        let inner = self.inner.clone();
        self.inner.db.call(move |c| {
            if inner.diverged.load(Ordering::Acquire) {
                return Err(Error::Diverged("store already diverged".into()));
            }
            let result = atomic(c, || {
                let mut step = super::Step::new(c)?;
                let value = apply(&mut step)?;
                let commit = step.finish()?;
                let runs = engine.prepare_live(c);
                let mut changes = Vec::new();
                if let Some((address, owners)) = &ownership {
                    for (documents, _) in &owners.documents {
                        changes.extend(documents.prepare_host_data(c, address)?);
                    }
                }
                Ok((value, commit, runs, changes))
            });
            match result {
                Ok((value, commit, runs, changes)) => {
                    let _runtime = inner.runtime.enter();
                    let context = crate::context::Context::background();
                    inner.batch.within(&context.clone(), move || {
                        let _ownership = ownership;
                        for change in changes {
                            change(&context)?;
                        }
                        super::live_query::adopt(runs)
                    })?;
                    inner.changed.send_replace(commit.seq);
                    Ok(finish(value, commit))
                }
                Err(error) => {
                    if matches!(error, Error::Diverged(_)) {
                        inner.diverged.store(true, Ordering::Release);
                    }
                    Err(error)
                }
            }
        })
    }
    pub fn apply_batch(&self, commits: Vec<OutboxCommit>) -> SqliteFuture<()> {
        let engine = self.clone();
        let inner = self.inner.clone();
        self.inner.db.call(move |c| {
            if inner.diverged.load(Ordering::Acquire) {
                return Err(Error::Diverged("store already diverged".into()));
            }
            let _lock = inner.claims.lock_replay()?;
            let result = atomic(c, || {
                let mut meta = read_meta(c)?;
                let mut tables = applier::catalog(c)?;
                for commit in commits {
                    safe(commit.seq, 1)?;
                    safe(commit.next_id, 2)?;
                    if commit.seq != meta.last_seq + 1 || commit.next_id < meta.next_id {
                        return Err(invalid("batch sequence or allocator is discontinuous"));
                    }
                    for value in &commit.writes {
                        let mut write: StoreWrite = serde_json::from_value(value.clone())?;
                        if let StoreWrite::DocumentCreate { record, .. } = &write
                            && record.id >= commit.next_id
                        {
                            return Err(invalid("document id is not below nextId"));
                        }
                        applier::apply_write(c, &mut tables, commit.seq, &mut write, true, false)?;
                    }
                    persist_commit(c, &commit, meta.outbox)?;
                    meta.last_seq = commit.seq;
                    meta.next_id = commit.next_id;
                }
                applier::write_catalog(c, &tables)?;
                Ok((meta.last_seq, engine.prepare_live(c)))
            });
            match result {
                Ok((seq, runs)) => {
                    engine.adopt_live(runs)?;
                    inner.changed.send_replace(seq);
                    Ok(())
                }
                Err(error) => {
                    if matches!(error, Error::Diverged(_)) {
                        inner.diverged.store(true, Ordering::Release)
                    }
                    Err(error)
                }
            }
        })
    }
    pub fn read(&self, query: Query) -> SqliteFuture<Page> {
        self.inner.db.call(move |c| {
            check_query(c, &query)?;
            super::query::read_page(c, &query)
        })
    }
    pub fn count(&self, query: Query) -> SqliteFuture<i64> {
        self.inner.db.call(move |c| {
            check_query(c, &query)?;
            let statement = query.statement(true)?;
            Ok(
                c.query_row(&statement.sql, params_from_iter(statement.params), |r| {
                    r.get(0)
                })?,
            )
        })
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        super::live_query::close_all(&self.observers);
    }
}
fn check_query(c: &Connection, query: &Query) -> Result<()> {
    let catalog = applier::catalog(c)?;
    let r = catalog
        .get(&query.table)
        .ok_or_else(|| invalid("table is not declared"))?;
    if r.version != query.definition.version
        || r.fingerprint != query.definition.fingerprint
        || r.state != super::TableState::Ready
    {
        return Err(invalid("table declaration differs from recorded table"));
    }
    Ok(())
}
pub(crate) fn atomic<T>(c: &Connection, run: impl FnOnce() -> Result<T>) -> Result<T> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let result = run().and_then(|value| {
        c.execute_batch("COMMIT")?;
        Ok(value)
    });
    match result {
        Ok(value) => Ok(value),
        Err(operation) => match c.execute_batch("ROLLBACK") {
            Ok(()) => Err(operation),
            Err(rollback) => Err(Error::Rollback {
                operation: Box::new(operation),
                rollback,
            }),
        },
    }
}
pub(crate) fn persist_commit(c: &Connection, commit: &OutboxCommit, outbox: bool) -> Result<()> {
    c.execute(
        "UPDATE chord_meta SET last_seq=?,next_id=? WHERE singleton=1",
        params![commit.seq, commit.next_id],
    )?;
    if outbox {
        c.execute(
            "INSERT INTO chord_outbox(seq,format,next_id,writes) VALUES(?,1,?,?)",
            params![
                commit.seq,
                commit.next_id,
                serde_json::to_string(&commit.writes)?
            ],
        )?;
    } else {
        c.execute(
            "UPDATE chord_meta SET outbox_floor=? WHERE singleton=1",
            [commit.seq],
        )?;
    }
    Ok(())
}
