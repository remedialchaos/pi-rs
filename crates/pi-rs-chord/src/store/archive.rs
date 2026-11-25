//! Format-1 gzip JSONL archive admission and lossless stored-value export.
use super::{
    DocumentContent, DocumentHistory, DocumentRecord, StoreEngine, StoreWrite, applier,
    runtime::atomic,
    table::{ColumnType, SAFE, invalid, quote, safe},
};
use crate::{
    Result,
    sqlite::{SqliteDatabase, SqliteFuture, apply_migrations_in},
};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use indexmap::IndexMap;
use rusqlite::{OptionalExtension, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Cursor, Read, Write},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Manifest {
    pub kind: String,
    pub format: i64,
    pub schema: i64,
    pub last_seq: i64,
    pub next_id: i64,
    pub host: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct End {
    pub kind: String,
    pub writes: i64,
}
/// Decodes one gzip member. Malformed UTF-8, empty lines and malformed JSON are refused.
pub fn decode_archive(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut text = String::new();
    GzDecoder::new(bytes).read_to_string(&mut text)?;
    text.lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
pub fn encode_archive(parts: &[Value]) -> Result<Vec<u8>> {
    let mut out = GzEncoder::new(Vec::new(), Compression::default());
    for part in parts {
        serde_json::to_writer(&mut out, part)?;
        out.write_all(b"\n")?;
    }
    Ok(out.finish()?)
}
impl StoreEngine {
    /// The SQLite actor holds the queue through the snapshot and compression.
    pub fn export_gzip(&self) -> SqliteFuture<Vec<u8>> {
        self.database().call(|c| {
            let mut bytes = Vec::new();
            export_to(c, super::read_meta(c)?, &mut bytes)?;
            Ok(bytes)
        })
    }
    /// Emits at most two 64 KiB chunks ahead of the consumer. Dropping the body
    /// releases the SQLite queue even if the receiver never reads the first byte.
    pub async fn export_stream(&self) -> Result<(i64, crate::sync::Body)> {
        let (body, mut writer) = crate::sync::Body::channel();
        let (head, receive) = tokio::sync::oneshot::channel();
        let work = self.database().call(move |c| {
            let meta = match super::read_meta(c) {
                Ok(meta) => meta,
                Err(error) => {
                    let _ = head.send(Err(error));
                    return Ok(());
                }
            };
            if head.send(Ok(meta.last_seq)).is_err() {
                return Ok(());
            }
            if let Err(error) = export_to(c, meta, &mut writer) {
                writer.fail(error);
            }
            Ok(())
        });
        // SqliteDatabase admits work at call time. The result travels in head/body.
        drop(work);
        Ok((receive.await.map_err(|_| crate::Error::Closed)??, body))
    }
    /// Import is atomic. The caller retains the database after failure for inspection or retry.
    pub async fn import_gzip(db: SqliteDatabase, bytes: Vec<u8>, outbox: bool) -> Result<Self> {
        Self::import_reader(db, Cursor::new(bytes), outbox).await
    }
    /// Parses and applies one JSONL record at a time inside the atomic import.
    pub async fn import_reader(
        db: SqliteDatabase,
        reader: impl Read + Send + 'static,
        outbox: bool,
    ) -> Result<Self> {
        db.call(move|c| {
            let existing=c.query_row("SELECT 1 FROM sqlite_master WHERE name='chord_meta'",[],|r|r.get::<_,i64>(0)).optional()?;if existing.is_some(){return Err(invalid("import: backend already holds a store"))}
            atomic(c,|| {
                apply_migrations_in(c,"chord_store_schema",&super::migrations())?;
                let mut iter=BufReader::new(GzDecoder::new(reader)).lines().map(|line| -> Result<Value> { Ok(serde_json::from_str(&line?)?) });let manifest:Manifest=serde_json::from_value(iter.next().ok_or_else(||invalid("archive has no manifest"))??)?;
                safe(manifest.last_seq,0)?;safe(manifest.next_id,2)?;
                if manifest.kind!="manifest"||manifest.format!=1||manifest.schema<1||manifest.schema>3||manifest.host.is_some(){return Err(invalid("archive manifest unsupported"))}
                let mut tables=IndexMap::new();let mut table=None;let mut indexes=false;let mut documents=false;let mut ended=false;let mut count=0i64;let mut ids=HashSet::new();let mut lifetimes:HashMap<String,Vec<(i64,i64)>>=HashMap::new();let mut current:Option<(DocumentRecord,i64,Option<i64>)>=None;
                for value in iter {
                    let value = value?;
                    if ended {return Err(invalid("archive part after end"))}
                    if value.get("kind")==Some(&Value::from("end")) {let end:End=serde_json::from_value(value)?;if end.kind!="end"||end.writes!=count {return Err(invalid("archive end count differs"))}finish_document(current.take())?;ended=true;continue;}
                    let mut write:StoreWrite=serde_json::from_value(value)?;
                    match &write {
                        StoreWrite::Table{table:name,..}=>{if documents||tables.contains_key(name){return Err(invalid("archive duplicate or late table"))}table=Some(name.clone());indexes=false;},
                        StoreWrite::Put{table:name,..}=>{if documents||indexes||table.as_ref()!=Some(name){return Err(invalid("archive put outside table group"))}},
                        StoreWrite::Index{table:name,..}=>{if documents||table.as_ref()!=Some(name){return Err(invalid("archive index outside table group"))}indexes=true;},
                        StoreWrite::DocumentCreate{record:r,base}=> {
                            finish_document(current.take())?;documents=true;
                            if base.is_some()||!ids.insert(r.id)||r.id>manifest.next_id||(r.id==manifest.next_id&&r.id!=SAFE)||r.created_at>manifest.last_seq||r.retired_at.is_some_and(|to|to<r.created_at||to>manifest.last_seq){return Err(invalid("archive document outside allocator or lifetime"))}
                            for text in [Some(r.owner.as_str()),Some(r.namespace.as_str()),Some(r.kind.as_str()),r.key.as_deref(),r.meta.as_deref(),r.host.as_deref()].into_iter().flatten(){if text.contains('\0'){return Err(invalid("archive text contains NUL"))}}
                            let address=serde_json::to_string(&(&r.owner,&r.namespace,&r.kind,r.key.as_deref().unwrap_or("")))?;let interval=(r.created_at,r.retired_at.unwrap_or(i64::MAX));let others=lifetimes.entry(address).or_default();
                            if interval.0<interval.1&&others.iter().any(|(from,to)|*from<interval.1&&interval.0<*to){return Err(invalid("archive document lifetimes overlap"))}others.push(interval);current=Some((r.clone(),0,None));
                        },
                        StoreWrite::DocumentRevision{id,seq,content}=> {
                            let(r,last,base)=current.as_mut().ok_or_else(||invalid("archive revision without document"))?;
                            if *id!=r.id||*seq<=*last||*seq<r.created_at||*seq>r.retired_at.unwrap_or(manifest.last_seq)||(r.retired_at.is_some()&&r.history!=DocumentHistory::Rewindable){return Err(invalid("archive revision order or lifetime differs"))}
                            match content {DocumentContent::Base{version,fingerprint,..}=>{if fingerprint.is_some(){return Err(invalid("archive base has fingerprint"))}*base=Some(*version)},DocumentContent::Delta{version,..}=>{if *base!=Some(*version){return Err(invalid("archive delta without base at version"))}}}*last = *seq;
                        },
                        _=>return Err(invalid("mutation is not an archive part")),
                    }
                    applier::apply_write(c,&mut tables,manifest.last_seq,&mut write,false,true)?;count+=1;
                }
                if !ended {return Err(invalid("archive has no end"))}
                applier::write_catalog(c,&tables)?;c.execute("UPDATE chord_meta SET last_seq=?,next_id=?,outbox=?,outbox_floor=? WHERE singleton=1",rusqlite::params![manifest.last_seq,manifest.next_id,outbox,manifest.last_seq])?;Ok(())
            })
        }).await?;
        Self::open(db).await
    }
}
fn export_to(c: &rusqlite::Connection, meta: super::MetaRow, writer: impl Write) -> Result<()> {
    export_scoped(c, meta, writer, None)
}
pub(super) fn export_scoped(
    c: &rusqlite::Connection,
    meta: super::MetaRow,
    writer: impl Write,
    scope: Option<&crate::contracts::Address>,
) -> Result<()> {
    let mut gzip = GzEncoder::new(writer, Compression::default());
    let mut count = 0i64;
    let mut emit = |part: Value| -> Result<()> {
        serde_json::to_writer(&mut gzip, &part)?;
        gzip.write_all(b"\n")?;
        count += 1;
        Ok(())
    };
    let mut manifest = json!({"kind":"manifest","format":1,"schema":3,"lastSeq":meta.last_seq,"nextId":meta.next_id});
    if let Some(scope) = scope {
        manifest["host"] = json!(scope.key);
    }
    emit(manifest)?;
    for (name, r) in applier::catalog(c)? {
        let (filter, key) = if let Some(scope) = scope {
            let Some(column) = super::host_data::host_column(&r.columns, &scope.kind) else {
                continue;
            };
            (
                format!(" WHERE {} = ?", quote(column)),
                Some(scope.key.as_str()),
            )
        } else {
            (String::new(), None)
        };
        emit(serde_json::to_value(StoreWrite::Table {
            table: name.clone(),
            version: r.version,
            state: r.state,
            columns: r.columns.clone(),
            key: r.key.clone(),
            fingerprint: r.fingerprint.clone(),
        })?)?;
        let sql = format!(
            "SELECT {} FROM {}{filter} ORDER BY {}",
            r.physical()
                .iter()
                .map(|n| quote(n))
                .collect::<Vec<_>>()
                .join(", "),
            quote(&name),
            r.key
                .iter()
                .map(|n| quote(n))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut statement = c.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(key))?;
        while let Some(stored) = rows.next()? {
            let mut row = IndexMap::new();
            let mut versions = IndexMap::new();
            for (n, col) in &r.columns {
                let cell: SqlValue = stored.get(n.as_str())?;
                let value = match cell {
                    SqlValue::Null => continue,
                    SqlValue::Integer(n) => {
                        if col.kind == ColumnType::Bool {
                            Value::Bool(n == 1)
                        } else {
                            Value::from(n)
                        }
                    }
                    SqlValue::Real(n) => Value::from(n),
                    SqlValue::Text(text) => {
                        if col.kind == ColumnType::Json {
                            versions.insert(
                                n.clone(),
                                stored.get::<_, i64>(format!("{n}$v").as_str())?,
                            );
                            serde_json::from_str(&text)?
                        } else {
                            Value::String(text)
                        }
                    }
                    SqlValue::Blob(_) => return Err(invalid("archive: unexpected blob")),
                };
                row.insert(n.clone(), value);
            }
            emit(serde_json::to_value(StoreWrite::Put {
                table: name.clone(),
                row,
                version: stored.get("$v")?,
                columns: versions,
            })?)?;
        }
        for (index, columns) in r.indexes {
            emit(serde_json::to_value(StoreWrite::Index {
                table: name.clone(),
                name: index,
                columns,
            })?)?;
        }
    }
    let mut docs = c.prepare(if scope.is_some() {
        "SELECT id FROM chord_documents WHERE host = ? AND retired_at IS NULL ORDER BY id"
    } else {
        "SELECT id FROM chord_documents ORDER BY id"
    })?;
    let mut documents = docs.query(rusqlite::params_from_iter(scope.map(|s| s.key.as_str())))?;
    while let Some(document) = documents.next()? {
        let id = document.get(0)?;
        let record = applier::document_row(c, id)?
            .ok_or_else(|| invalid("archive: document disappeared"))?;
        emit(serde_json::to_value(StoreWrite::DocumentCreate {
            record,
            base: None,
        })?)?;
        let mut revisions = c.prepare(
            "SELECT seq,kind,version,content FROM chord_revisions WHERE document_id=? ORDER BY seq",
        )?;
        let mut rows = revisions.query([id])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(1)?;
            let version = row.get(2)?;
            let text: String = row.get(3)?;
            let content = if kind == "base" {
                DocumentContent::Base {
                    version,
                    fingerprint: None,
                    value: serde_json::from_str(&text)?,
                }
            } else {
                DocumentContent::Delta {
                    version,
                    ops: serde_json::from_str(&text)?,
                }
            };
            emit(serde_json::to_value(StoreWrite::DocumentRevision {
                id,
                seq: row.get(0)?,
                content,
            })?)?;
        }
    }
    serde_json::to_writer(&mut gzip, &json!({"kind":"end","writes":count-1}))?;
    gzip.write_all(b"\n")?;
    gzip.finish()?;
    Ok(())
}
fn finish_document(document: Option<(DocumentRecord, i64, Option<i64>)>) -> Result<()> {
    if let Some((record, _, base)) = document {
        let contentless = record.retired_at.is_some() && record.history == DocumentHistory::Latest;
        if match base {
            None => !contentless,
            Some(v) => v != record.version,
        } {
            return Err(invalid("archive document has no base at current version"));
        }
    }
    Ok(())
}
