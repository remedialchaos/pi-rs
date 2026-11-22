//! One typed SQL step shared by table mutations and document transactions.
use super::{
    OutboxCommit, StoreWrite, TableRecord, applier, read_meta,
    runtime::persist_commit,
    table::{invalid, safe},
};
use crate::Result;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WriteOrder {
    Retire,
    Create,
    Write,
}
pub struct Step<'a> {
    connection: &'a Connection,
    pub seq: i64,
    next_id: i64,
    outbox: bool,
    tables: applier::Catalog,
    writes: Vec<StoreWrite>,
    pub created: Vec<i64>,
}
impl<'a> Step<'a> {
    pub fn new(connection: &'a Connection) -> Result<Self> {
        let meta = read_meta(connection)?;
        let seq = meta
            .last_seq
            .checked_add(1)
            .ok_or_else(|| invalid("store sequence overflow"))?;
        safe(seq, 1)?;
        Ok(Self {
            connection,
            seq,
            next_id: meta.next_id,
            outbox: meta.outbox,
            tables: applier::catalog(connection)?,
            writes: vec![],
            created: vec![],
        })
    }
    pub fn table(&self, name: &str) -> Option<&TableRecord> {
        self.tables.get(name)
    }
    pub fn mint(&mut self) -> Result<i64> {
        let id = self.next_id;
        safe(id, 2)?;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("document allocator overflow"))?;
        safe(self.next_id, 2)?;
        self.created.push(id);
        Ok(id)
    }
    pub fn apply(&mut self, mut write: StoreWrite) -> Result<()> {
        if let StoreWrite::DocumentCreate { record, .. } = &write {
            let next = record
                .id
                .checked_add(1)
                .ok_or_else(|| invalid("document allocator overflow"))?;
            safe(next, 2)?;
            self.next_id = self.next_id.max(next);
        }
        applier::apply_write(
            self.connection,
            &mut self.tables,
            self.seq,
            &mut write,
            false,
            false,
        )?;
        self.writes.push(write);
        Ok(())
    }
    /// Reads see all typed writes already applied in this step. Values, never SQL fragments, carry parameters.
    pub fn read(&self, sql: &str, params: Vec<SqlValue>) -> Result<Vec<Vec<SqlValue>>> {
        let mut statement = self.connection.prepare_cached(sql)?;
        if !statement.readonly() {
            return Err(invalid("step reader cannot execute writes"));
        }
        let columns = statement.column_count();
        Ok(statement
            .query_map(params_from_iter(params), |r| {
                (0..columns)
                    .map(|i| r.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn finish(self) -> Result<OutboxCommit> {
        applier::write_catalog(self.connection, &self.tables)?;
        let commit = OutboxCommit {
            seq: self.seq,
            next_id: self.next_id,
            writes: self
                .writes
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        persist_commit(self.connection, &commit, self.outbox)?;
        Ok(commit)
    }
}
/// A staged mutation serializes its wire form before admission. SQL and in-memory adoption remain separate.
pub struct StagedSql {
    writes: Vec<StoreWrite>,
    order: WriteOrder,
}
impl StagedSql {
    pub fn new(writes: Vec<StoreWrite>, order: WriteOrder) -> Result<Self> {
        for write in &writes {
            serde_json::to_string(write)?;
        }
        Ok(Self { writes, order })
    }
}
impl crate::documents::Staged for StagedSql {
    fn persistent(&self) -> bool {
        true
    }
    fn order(&self) -> WriteOrder {
        self.order
    }
    fn apply(&self, step: &mut Step<'_>) -> Result<()> {
        for write in &self.writes {
            step.apply(write.clone())?;
        }
        Ok(())
    }
    fn adopt(&self) -> Result<()> {
        Ok(())
    }
}
