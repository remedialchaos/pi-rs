//! Store-owned schema, declared tables and durable outbox.
mod applier;
mod archive;
mod claims;
mod evolution;
mod expression;
mod json_column;
pub use evolution::{DataStep, MIGRATION_CHUNK, RowPatch, TableEditor, TableHistory, TablePlan};
pub use json_column::{JsonColumn, JsonMigration};
pub(crate) mod live_query;
mod query;
mod read_plan;
pub use live_query::{LiveQuery, LiveShape};
pub use read_plan::{
    Bindings, Bound, ColumnSet, Joined, Projection, ReadPlan, ReadRange, ReadRole, StoredRead,
    TableUse,
};
mod runtime;
mod step;
mod table;
mod writes;
use crate::{
    Error, Result,
    data::ChordError,
    sqlite::{Migration, SqliteDatabase, SqliteFuture, apply_migrations_in},
};
pub use archive::{decode_archive, encode_archive};
pub use claims::{Claimant, Claims};
pub use expression::{ExprNode, SetNode, Statement, WhereNode};
pub use query::{Page, Query};
pub use runtime::StoreEngine;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use step::{StagedSql, Step, WriteOrder};
pub use table::{ColumnRecord, ColumnType, TableDefinition, TableRecord, TableState};
pub use writes::{
    AlterEdit, DocumentBase, DocumentContent, DocumentHistory, DocumentRecord, StoreWrite,
};
pub const STORE_SCHEMA: u32 = 3;
pub const OUTBOX_FORMAT: u32 = 1;
pub const ARCHIVE_FORMAT: u32 = 1;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaRow {
    pub last_seq: i64,
    pub next_id: i64,
    pub outbox: bool,
    pub outbox_floor: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxCommit {
    pub seq: i64,
    pub next_id: i64,
    pub writes: Vec<Value>,
}
pub fn migrations() -> Vec<Migration> {
    vec![
    Migration{version:1,statements:vec![
        "CREATE TABLE chord_meta (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), last_seq INTEGER NOT NULL, next_id INTEGER NOT NULL, outbox INTEGER NOT NULL CHECK (outbox IN (0, 1)), outbox_floor INTEGER NOT NULL) STRICT".into(),
        "INSERT INTO chord_meta (singleton,last_seq,next_id,outbox,outbox_floor) VALUES (1,0,2,0,0)".into(),
        "CREATE TABLE chord_tables (name TEXT PRIMARY KEY,definition TEXT NOT NULL) STRICT, WITHOUT ROWID".into(),
        "CREATE TABLE chord_documents (id INTEGER PRIMARY KEY,owner TEXT NOT NULL,namespace TEXT NOT NULL,kind TEXT NOT NULL,key TEXT NOT NULL,history TEXT NOT NULL CHECK (history IN ('none','latest','rewindable')),created_at INTEGER NOT NULL,retired_at INTEGER,meta TEXT,revision INTEGER NOT NULL,version INTEGER NOT NULL,fingerprint TEXT NOT NULL) STRICT".into(),
        "CREATE UNIQUE INDEX chord_documents_live ON chord_documents(owner,namespace,kind,key) WHERE retired_at IS NULL".into(),
        "CREATE TABLE chord_revisions (document_id INTEGER NOT NULL,seq INTEGER NOT NULL,kind TEXT NOT NULL CHECK (kind IN ('base','delta')),version INTEGER NOT NULL,content TEXT NOT NULL,PRIMARY KEY(document_id,seq)) STRICT, WITHOUT ROWID".into(),
        "CREATE TABLE chord_outbox (seq INTEGER PRIMARY KEY,format INTEGER NOT NULL,next_id INTEGER NOT NULL,writes TEXT NOT NULL) STRICT".into(),
    ]},
    Migration{version:2,statements:vec![
        "CREATE INDEX chord_documents_address ON chord_documents(owner,namespace,kind,key,created_at)".into(),
        "CREATE INDEX chord_documents_scope ON chord_documents(owner,namespace,id)".into(),
        "CREATE INDEX chord_revisions_bases ON chord_revisions(document_id,seq) WHERE kind = 'base'".into(),
    ]},
    Migration{version:3,statements:vec!["ALTER TABLE chord_documents ADD COLUMN host TEXT".into(),"CREATE INDEX chord_documents_host ON chord_documents(host) WHERE host IS NOT NULL".into()]},
]
}
fn read_meta(db: &Connection) -> Result<MetaRow> {
    Ok(db.query_row(
        "SELECT last_seq,next_id,outbox,outbox_floor FROM chord_meta WHERE singleton=1",
        [],
        |r| {
            Ok(MetaRow {
                last_seq: r.get(0)?,
                next_id: r.get(1)?,
                outbox: r.get(2)?,
                outbox_floor: r.get(3)?,
            })
        },
    )?)
}
pub async fn apply_store_schema(db: &SqliteDatabase) -> Result<MetaRow> {
    let current=db.call(|db|{
        let row=db.query_row("SELECT s.version,e.encoding,m.last_seq,m.next_id,m.outbox,m.outbox_floor FROM chord_store_schema s,pragma_encoding e,chord_meta m WHERE s.singleton=1 AND m.singleton=1",[],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,String>(1)?,MetaRow{last_seq:r.get(2)?,next_id:r.get(3)?,outbox:r.get(4)?,outbox_floor:r.get(5)?})));
        Ok(match row {Ok((STORE_SCHEMA,encoding,meta)) if encoding=="UTF-8"=>Some(meta),_=>None})
    }).await?;
    if let Some(meta) = current {
        return Ok(meta);
    }
    db.transaction(|db| {
        let encoding: String = db.query_row("PRAGMA encoding", [], |r| r.get(0))?;
        if encoding != "UTF-8" {
            return Err(Error::Invalid(format!(
                "store: the database encoding is {encoding}, not UTF-8"
            )));
        }
        apply_migrations_in(db, "chord_store_schema", &migrations())?;
        read_meta(db)
    })
    .await
}
pub fn read_outbox(
    db: &SqliteDatabase,
    after_seq: i64,
    limit: usize,
) -> SqliteFuture<Vec<OutboxCommit>> {
    db.transaction(move |db| {
        let meta = read_meta(db)?;
        if limit == 0 || limit as u128 > 9_007_199_254_740_991u128 {
            return Err(
                ChordError::invalid("args", "outbox: limit is a positive integer", None).into(),
            );
        }
        if after_seq < meta.outbox_floor || after_seq > meta.last_seq {
            return Err(ChordError::invalid(
                "args",
                format!(
                    "outbox: read after {after_seq}, outside {}..{}",
                    meta.outbox_floor, meta.last_seq
                ),
                None,
            )
            .into());
        }
        let mut statement = db.prepare_cached(
            "SELECT seq,format,next_id,writes FROM chord_outbox WHERE seq>? ORDER BY seq LIMIT ?",
        )?;
        let rows = statement.query_map(params![after_seq, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, format, next_id, writes) = row?;
            if format != OUTBOX_FORMAT {
                return Err(ChordError::new(
                    "unavailable",
                    format!(
                        "outbox: commit {seq} is in format {format}, newer than {OUTBOX_FORMAT}"
                    ),
                )
                .into());
            }
            Ok(OutboxCommit {
                seq,
                next_id,
                writes: serde_json::from_str(&writes)?,
            })
        })
        .collect()
    })
}
pub fn trim_outbox(db: &SqliteDatabase, through_seq: i64) -> SqliteFuture<()> {
    db.transaction(move |db| {
        let meta = read_meta(db)?;
        if through_seq > meta.last_seq {
            return Err(ChordError::invalid(
                "args",
                format!(
                    "outbox: trim through {through_seq}, beyond {}",
                    meta.last_seq
                ),
                None,
            )
            .into());
        }
        if through_seq > meta.outbox_floor {
            db.execute("DELETE FROM chord_outbox WHERE seq<=?", [through_seq])?;
            db.execute(
                "UPDATE chord_meta SET outbox_floor=? WHERE singleton=1",
                [through_seq],
            )?;
        }
        Ok(())
    })
}
/// Returns the recorded definition without inventing a declaration from a SQL table.
pub fn recorded_table(db: &SqliteDatabase, name: String) -> SqliteFuture<Option<Value>> {
    db.call(move |db| {
        let value = db
            .query_row(
                "SELECT definition FROM chord_tables WHERE name=?",
                [name],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(Error::from))
            .transpose()
    })
}
