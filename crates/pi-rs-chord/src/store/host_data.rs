//! Address-scoped archives and transactional ownership transfer.
use super::{
    ColumnRecord, DocumentBase, DocumentContent, DocumentHistory, DocumentRecord, Step,
    StoreEngine, StoreWrite, TableRecord, TableState, WhereNode, applier,
    archive::{End, Manifest},
    table::{self, invalid, quote, safe},
};
use crate::{Error, Result, contracts::Address, delta::Op, json::JsonValue, sqlite::SqliteFuture};
use indexmap::IndexMap;
use rusqlite::types::Value as SqlValue;
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
};
pub(crate) type HostDataAdoption = Box<dyn FnOnce(&crate::context::Context) -> Result<()> + Send>;
#[derive(Default)]
pub(crate) struct HostDataOwners {
    pub(crate) documents: Vec<(crate::documents::Documents, crate::documents::LineLease)>,
    pub(crate) _gate: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostDataExisting {
    #[default]
    Refuse,
    Replace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostDataCounts {
    pub documents: usize,
    pub rows: usize,
}

/// Validated archive bound to a target address. Destination conflicts are checked at commit.
pub struct PreparedHostData {
    pub(crate) target: Address,
    tables: Vec<OwnedTable>,
    documents: Vec<(DocumentRecord, JsonValue)>,
}

struct OwnedTable {
    record: TableRecord,
    column: String,
    writes: Vec<StoreWrite>,
}

pub(super) fn host_column<'a>(
    columns: &'a IndexMap<String, ColumnRecord>,
    kind: &str,
) -> Option<&'a str> {
    columns
        .iter()
        .find(|(_, c)| c.host.as_deref() == Some(kind))
        .map(|(n, _)| n.as_str())
}

fn check_address(address: &Address) -> Result<()> {
    if address.kind.is_empty()
        || address.key.contains('\0')
        || crate::contracts::kind_of_key(&address.key) != address.kind
    {
        return Err(invalid("invalid host address key"));
    }
    Ok(())
}

impl StoreEngine {
    /// Export only the live documents and table rows belonging to `address`.
    pub fn export_host_data(&self, address: &Address) -> SqliteFuture<Vec<u8>> {
        let address = address.clone();
        self.database().transaction(move |c| {
            check_address(&address)?;
            let mut bytes = Vec::new();
            super::archive::export_scoped(c, super::read_meta(c)?, &mut bytes, Some(&address))?;
            Ok(bytes)
        })
    }

    /// Validate the archive without modifying the store, and remap owned keys to `target`.
    pub fn prepare_host_data(
        &self,
        source: &Address,
        target: &Address,
        bytes: &[u8],
    ) -> Result<PreparedHostData> {
        check_address(source)?;
        check_address(target)?;
        if source.kind != target.kind {
            return Err(invalid("cannot copy host data to another address kind"));
        }
        parse(source, target, bytes)
    }

    /// Import into an unoccupied address or replace its data in one SQLite transaction.
    pub fn import_host_data(
        &self,
        archive: PreparedHostData,
        existing: HostDataExisting,
    ) -> SqliteFuture<HostDataCounts> {
        self.import_owned_host_data(archive, existing, HostDataOwners::default())
    }
    pub(crate) fn import_owned_host_data(
        &self,
        archive: PreparedHostData,
        existing: HostDataExisting,
        owners: HostDataOwners,
    ) -> SqliteFuture<HostDataCounts> {
        self.change_host_data(archive.target.clone(), owners, move |step| {
            let scope = ScopedData::read(step, &archive.target)?;
            if existing == HostDataExisting::Replace {
                scope.remove(step, &archive.target)?;
            } else if !scope.documents.is_empty()
                || scope.tables.iter().any(|(_, _, count)| *count != 0)
            {
                return Err(invalid("target address already owns data"));
            }
            let mut counts = HostDataCounts::default();
            for table in archive.tables {
                apply_table(step, table, &mut counts)?;
            }
            for (mut record, value) in archive.documents {
                record.id = step.mint()?;
                record.created_at = step.seq;
                record.revision = 1;
                let base = DocumentBase {
                    version: record.version,
                    fingerprint: record.fingerprint.clone(),
                    value,
                };
                step.apply(StoreWrite::DocumentCreate {
                    record,
                    base: Some(base),
                })?;
                counts.documents += 1;
            }
            Ok(counts)
        })
    }

    /// Delete the address's data, not its table declarations or other addresses' data.
    pub fn delete_host_data(&self, address: &Address) -> SqliteFuture<HostDataCounts> {
        self.delete_owned_host_data(address, HostDataOwners::default())
    }
    pub(crate) fn delete_owned_host_data(
        &self,
        address: &Address,
        owners: HostDataOwners,
    ) -> SqliteFuture<HostDataCounts> {
        let address = address.clone();
        self.change_host_data(address.clone(), owners, move |step| {
            check_address(&address)?;
            ScopedData::read(step, &address)?.remove(step, &address)
        })
    }
}

fn parse(source: &Address, target: &Address, bytes: &[u8]) -> Result<PreparedHostData> {
    let mut reader = BufReader::new(flate2::bufread::GzDecoder::new(bytes));
    let mut parts = (&mut reader)
        .lines()
        .map(|line| -> Result<Value> { Ok(serde_json::from_str(&line?)?) });
    let manifest: Manifest = serde_json::from_value(
        parts
            .next()
            .ok_or_else(|| invalid("missing host manifest"))??,
    )?;
    safe(manifest.last_seq, 0)?;
    safe(manifest.next_id, 2)?;
    if manifest.kind != "manifest"
        || manifest.format != i64::from(super::ARCHIVE_FORMAT)
        || !(1..=i64::from(super::STORE_SCHEMA)).contains(&manifest.schema)
        || manifest.host.as_deref() != Some(&source.key)
    {
        return Err(invalid(
            "archive manifest does not belong to source address",
        ));
    }
    let mut archive = PreparedHostData {
        target: target.clone(),
        tables: Vec::new(),
        documents: Vec::new(),
    };
    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    let mut addresses = HashSet::new();
    let mut current = None;
    let mut keys = HashSet::new();
    let mut parts = parts.enumerate();
    while let Some((count, part)) = parts.next() {
        let part = part?;
        if part.get("kind").and_then(Value::as_str) == Some("end") {
            let end: End = serde_json::from_value(part)?;
            if usize::try_from(end.writes).ok() != Some(count) {
                return Err(invalid("host archive write count differs"));
            }
            finish_document(&mut archive, current)?;
            if parts.next().is_some() || !reader.get_ref().get_ref().is_empty() {
                return Err(invalid("host archive part after end"));
            }
            return Ok(archive);
        }
        match serde_json::from_value::<StoreWrite>(part)? {
            StoreWrite::Table {
                table,
                version,
                state,
                columns,
                key,
                fingerprint,
            } => {
                if current.is_some() || !names.insert(table.to_ascii_lowercase()) {
                    return Err(invalid("duplicate or late table in host archive"));
                }
                table::sql_name(&table)?;
                safe(version, 1)?;
                table::check_columns(&columns, &key)?;
                let column = host_column(&columns, &source.kind)
                    .ok_or_else(|| invalid("archive table is not scoped to source kind"))?
                    .to_owned();
                archive.tables.push(OwnedTable {
                    column,
                    record: TableRecord {
                        table,
                        version,
                        state,
                        columns,
                        key,
                        fingerprint,
                        indexes: IndexMap::new(),
                    },
                    writes: Vec::new(),
                });
                keys.clear();
            }
            StoreWrite::Put {
                table,
                mut row,
                version,
                columns,
            } => {
                let group = archive
                    .tables
                    .last_mut()
                    .filter(|g| {
                        current.is_none() && g.record.table == table && g.record.indexes.is_empty()
                    })
                    .ok_or_else(|| invalid("host row outside its table group"))?;
                if row.get(&group.column).and_then(Value::as_str) != Some(&source.key) {
                    return Err(invalid("archive row does not belong to source address"));
                }
                row.insert(group.column.clone(), Value::from(target.key.clone()));
                applier::put_params(&group.record, &row, version, &columns)?;
                let key = extract_key(&group.record, &row)?;
                let identity = serde_json::to_string(&key)?;
                if !keys.insert(identity) {
                    return Err(invalid("duplicate primary key in host archive"));
                }
                group.writes.push(StoreWrite::Put {
                    table,
                    row,
                    version,
                    columns,
                });
            }
            StoreWrite::Index {
                table,
                name,
                columns,
            } => {
                let group = archive
                    .tables
                    .last_mut()
                    .filter(|g| current.is_none() && g.record.table == table)
                    .ok_or_else(|| invalid("host index outside its table group"))?;
                if !table::index_name(&name) || group.record.indexes.contains_key(&name) {
                    return Err(invalid("duplicate or invalid host table index"));
                }
                table::check_order(&group.record.columns, &columns)?;
                group.record.indexes.insert(name, columns);
            }
            StoreWrite::DocumentCreate { mut record, base } => {
                finish_document(&mut archive, current.take())?;
                for n in [
                    record.id,
                    record.created_at,
                    record.revision,
                    record.version,
                ] {
                    safe(n, 1)?;
                }
                if record.id >= manifest.next_id
                    || record.created_at > manifest.last_seq
                    || !ids.insert(record.id)
                    || base.is_some()
                    || record.history != DocumentHistory::None
                    || record.retired_at.is_some()
                    || record.meta.is_some()
                    || record.host.as_deref() != Some(&source.key)
                    || record.key.as_deref() != Some(&source.key)
                    || [&record.owner, &record.namespace, &record.kind]
                        .into_iter()
                        .any(|s| s.is_empty() || s.contains('\0'))
                    || !addresses.insert((
                        record.owner.clone(),
                        record.namespace.clone(),
                        record.kind.clone(),
                    ))
                {
                    return Err(invalid("invalid or duplicate host document"));
                }
                record.host = Some(target.key.clone());
                record.key = Some(target.key.clone());
                current = Some(ArchiveDocument {
                    record,
                    seq: 0,
                    value: None,
                });
            }
            StoreWrite::DocumentRevision { id, seq, content } => {
                let document = current
                    .as_mut()
                    .filter(|d| {
                        d.record.id == id
                            && seq > d.seq
                            && seq >= d.record.created_at
                            && seq <= manifest.last_seq
                    })
                    .ok_or_else(|| invalid("invalid host document revision order"))?;
                safe(content.version(), 1)?;
                match content {
                    DocumentContent::Base {
                        version,
                        fingerprint: None,
                        value,
                    } => document.value = Some((version, value)),
                    DocumentContent::Delta { version, ops } => {
                        let (_, value) = document
                            .value
                            .take()
                            .filter(|(base, _)| *base == version)
                            .ok_or_else(|| invalid("host delta without a base at its version"))?;
                        let ops = ops
                            .iter()
                            .map(Op::from_json)
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        document.value = Some((version, crate::delta::apply(value, &ops)?));
                    }
                    _ => return Err(invalid("host archive base has fingerprint")),
                }
                document.seq = seq;
            }
            _ => return Err(invalid("mutation is not a host archive part")),
        }
    }
    Err(invalid("host archive has no end"))
}

struct ArchiveDocument {
    record: DocumentRecord,
    seq: i64,
    value: Option<(i64, JsonValue)>,
}

fn finish_document(
    archive: &mut PreparedHostData,
    document: Option<ArchiveDocument>,
) -> Result<()> {
    if let Some(document) = document {
        let (_, value) = document
            .value
            .filter(|(v, _)| *v == document.record.version)
            .ok_or_else(|| invalid("host document has no base at its current version"))?;
        archive.documents.push((document.record, value));
    }
    Ok(())
}

fn extract_key(
    record: &TableRecord,
    row: &IndexMap<String, Value>,
) -> Result<IndexMap<String, Value>> {
    record
        .key
        .iter()
        .map(|n| {
            Ok((
                n.clone(),
                row.get(n)
                    .cloned()
                    .ok_or_else(|| invalid("missing host row primary key"))?,
            ))
        })
        .collect()
}

fn apply_table(step: &mut Step<'_>, table: OwnedTable, counts: &mut HostDataCounts) -> Result<()> {
    let r = table.record;
    if let Some(existing) = step.table(&r.table) {
        if existing.version != r.version
            || existing.state != TableState::Ready
            || r.state != TableState::Ready
            || existing.columns != r.columns
            || existing.key != r.key
            || existing.fingerprint != r.fingerprint
            || existing.indexes != r.indexes
        {
            return Err(invalid(format!(
                "host archive table {} differs from destination schema",
                r.table
            )));
        }
    } else {
        step.apply(StoreWrite::Table {
            table: r.table.clone(),
            version: r.version,
            state: r.state,
            columns: r.columns.clone(),
            key: r.key.clone(),
            fingerprint: r.fingerprint.clone(),
        })?;
        for (name, columns) in &r.indexes {
            step.apply(StoreWrite::Index {
                table: r.table.clone(),
                name: name.clone(),
                columns: columns.clone(),
            })?;
        }
    }
    for write in table.writes {
        if let StoreWrite::Put { row, .. } = &write {
            let mut params = Vec::new();
            let cond = applier::key_sql(&r, &extract_key(&r, row)?, &mut params)?;
            if !step
                .read(
                    &format!("SELECT 1 FROM {} WHERE {cond}", quote(&r.table)),
                    params,
                )?
                .is_empty()
            {
                return Err(invalid(format!("host archive row collides in {}", r.table)));
            }
        }
        step.apply(write)?;
        counts.rows += 1;
    }
    Ok(())
}

struct ScopedData {
    documents: Vec<(i64, i64)>,
    tables: Vec<(String, String, i64)>,
}

impl ScopedData {
    fn read(step: &Step<'_>, address: &Address) -> Result<Self> {
        let params = || vec![SqlValue::Text(address.key.clone())];
        let documents = step.read("SELECT id, revision FROM chord_documents WHERE host = ? AND retired_at IS NULL ORDER BY id", params())?.into_iter().map(|row| match row.as_slice() {
            [SqlValue::Integer(id), SqlValue::Integer(revision)] => Ok((*id, *revision)),
            _ => Err(invalid("invalid stored host document")),
        }).collect::<Result<Vec<_>>>()?;
        let mut tables = Vec::new();
        for (name, record) in step.tables() {
            if let Some(column) = host_column(&record.columns, &address.kind) {
                let sql = format!(
                    "SELECT count(*) FROM {} WHERE {} = ?",
                    quote(name),
                    quote(column)
                );
                let rows = step.read(&sql, params())?;
                let [SqlValue::Integer(count)] = rows.first().map_or(&[][..], Vec::as_slice) else {
                    return Err(invalid("invalid host row count"));
                };
                tables.push((name.clone(), column.to_owned(), *count));
            }
        }
        Ok(Self { documents, tables })
    }

    fn remove(self, step: &mut Step<'_>, address: &Address) -> Result<HostDataCounts> {
        let mut counts = HostDataCounts {
            documents: self.documents.len(),
            rows: 0,
        };
        for (id, revision) in self.documents {
            step.apply(StoreWrite::DocumentRetire { id, revision })?;
        }
        for (table, column, count) in self.tables {
            if count != 0 {
                step.apply(StoreWrite::DeleteWhere {
                    table,
                    r#where: WhereNode::Eq((column, Value::from(address.key.clone()))),
                    count: Some(count),
                })?;
                counts.rows = counts
                    .rows
                    .checked_add(
                        usize::try_from(count)
                            .map_err(|_| Error::Invalid("invalid host row count".into()))?,
                    )
                    .ok_or_else(|| invalid("host row count overflow"))?;
            }
        }
        Ok(counts)
    }
}
