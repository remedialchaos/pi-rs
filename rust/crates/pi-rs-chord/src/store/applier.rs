//! One checked write applier for local commits, replicas and archives.
use super::{
    TableRecord,
    expression::{set_sql, where_sql},
    table::*,
    writes::*,
};
use crate::{Error, Result};
use indexmap::IndexMap;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use serde_json::Value;
pub(crate) type Catalog = IndexMap<String, TableRecord>;
pub(crate) fn catalog(c: &Connection) -> Result<Catalog> {
    let mut s = c.prepare_cached("SELECT name,definition FROM chord_tables ORDER BY name")?;
    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|row| {
            let (n, t) = row?;
            Ok((n, serde_json::from_str(&t)?))
        })
        .collect()
}
pub(crate) fn write_catalog(c: &Connection, tables: &Catalog) -> Result<()> {
    for (n, r) in tables {
        c.execute(
            "INSERT OR REPLACE INTO chord_tables(name,definition) VALUES(?,?)",
            params![n, serde_json::to_string(r)?],
        )?;
    }
    Ok(())
}
fn record<'a>(tables: &'a Catalog, n: &str) -> Result<&'a TableRecord> {
    tables
        .get(n)
        .ok_or_else(|| invalid(format!("table {n} not recorded")))
}
pub(crate) fn key_sql(
    r: &TableRecord,
    key: &IndexMap<String, Value>,
    p: &mut Vec<SqlValue>,
) -> Result<String> {
    if key.len() != r.key.len() {
        return Err(invalid("not a primary key"));
    }
    let mut cond = Vec::new();
    for n in &r.key {
        let value = key.get(n).ok_or_else(|| invalid("missing key column"))?;
        let c = r
            .columns
            .get(n)
            .ok_or_else(|| invalid("key missing in record"))?;
        p.push(scalar(c.kind, value)?);
        cond.push(format!("{} = ?", quote(n)))
    }
    Ok(cond.join(" AND "))
}
pub(crate) fn put_params(
    r: &TableRecord,
    row: &IndexMap<String, Value>,
    version: i64,
    versions: &IndexMap<String, i64>,
) -> Result<Vec<SqlValue>> {
    safe(version, 1)?;
    if version > r.version {
        return Err(invalid("row version newer than table"));
    }
    if row.keys().any(|n| !r.columns.contains_key(n))
        || versions.keys().any(|n| {
            !row.contains_key(n) || r.columns.get(n).is_none_or(|c| c.kind != ColumnType::Json)
        })
    {
        return Err(invalid("unknown row column or spurious json version"));
    }
    let mut p = Vec::new();
    let mut json = Vec::new();
    for (n, c) in &r.columns {
        let Some(v) = row.get(n) else {
            if !c.optional {
                return Err(invalid(format!("missing column {n}")));
            }
            p.push(SqlValue::Null);
            if c.kind == ColumnType::Json {
                json.push(SqlValue::Null)
            }
            continue;
        };
        if c.kind == ColumnType::Json {
            let vnum = *versions
                .get(n)
                .ok_or_else(|| invalid("json cell missing version"))?;
            safe(vnum, 1)?;
            if Some(vnum) > c.version {
                return Err(invalid("unreadable json version"));
            }
            p.push(SqlValue::Text(serde_json::to_string(v)?));
            json.push(SqlValue::Integer(vnum));
        } else {
            p.push(scalar(c.kind, v)?)
        }
    }
    p.extend(json);
    p.push(SqlValue::Integer(version));
    Ok(p)
}
pub(crate) fn put_sql(r: &TableRecord) -> String {
    let names = r.physical();
    format!(
        "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
        quote(&r.table),
        names
            .iter()
            .map(|n| quote(n))
            .collect::<Vec<_>>()
            .join(", "),
        vec!["?"; names.len()].join(", ")
    )
}
fn index_sql(r: &TableRecord, n: &str, columns: &[String]) -> String {
    let mut on = columns.to_vec();
    on.extend(r.key.iter().filter(|n| !columns.contains(n)).cloned());
    format!(
        "CREATE INDEX {} ON {} ({})",
        quote(&format!("{}:{n}", r.table)),
        quote(&r.table),
        on.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", ")
    )
}
fn counted(
    c: &Connection,
    sql: &str,
    p: Vec<SqlValue>,
    count: &mut Option<i64>,
    replay: bool,
) -> Result<()> {
    let actual = c.execute(sql, params_from_iter(p))? as i64;
    if replay && count.is_none() {
        return Err(invalid("recorded mutation missing count"));
    }
    if let Some(expected) = count {
        safe(*expected, 0)?;
        if actual != *expected {
            return Err(Error::Diverged(format!(
                "changed {actual} rows, recorded {expected}"
            )));
        }
    }
    *count = Some(actual);
    Ok(())
}
pub(crate) fn apply_write(
    c: &Connection,
    tables: &mut Catalog,
    seq: i64,
    write: &mut StoreWrite,
    replay: bool,
    archive: bool,
) -> Result<()> {
    match write {
        StoreWrite::Table {
            table,
            version,
            state,
            columns,
            key,
            fingerprint,
        } => {
            sql_name(table)?;
            safe(*version, 1)?;
            check_columns(columns, key)?;
            let previous = tables.get(table);
            let r = TableRecord {
                table: table.clone(),
                version: *version,
                state: *state,
                columns: columns.clone(),
                key: key.clone(),
                fingerprint: fingerprint.clone(),
                indexes: previous.map(|r| r.indexes.clone()).unwrap_or_default(),
            };
            if previous.is_none() {
                c.execute(&create_table(&r)?, [])?;
            }
            tables.insert(table.clone(), r);
        }
        StoreWrite::Index {
            table,
            name,
            columns,
        } => {
            let mut r = record(tables, table)?.clone();
            if !index_name(name) || r.indexes.contains_key(name) {
                return Err(invalid("not a new index name"));
            }
            check_order(&r.columns, columns)?;
            c.execute(&index_sql(&r, name, columns), [])?;
            r.indexes.insert(name.clone(), columns.clone());
            tables.insert(table.clone(), r);
        }
        StoreWrite::DropIndex { table, name } => {
            let mut r = record(tables, table)?.clone();
            if r.indexes.shift_remove(name).is_none() {
                return Err(invalid("no such index"));
            }
            c.execute(
                &format!("DROP INDEX {}", quote(&format!("{table}:{name}"))),
                [],
            )?;
            tables.insert(table.clone(), r);
        }
        StoreWrite::Alter {
            table,
            version,
            edit,
        } => {
            let mut r = record(tables, table)?.clone();
            if *version != r.version && *version != r.version + 1 {
                return Err(invalid("edit has incompatible table version"));
            }
            let id = quote(table);
            match edit {
                AlterEdit::Rename { from, to } => {
                    if !column_name(to)
                        || r.columns
                            .keys()
                            .any(|n| n != from && n.eq_ignore_ascii_case(to))
                    {
                        return Err(invalid("rename target exists or is invalid"));
                    }
                    let old = r
                        .columns
                        .get(from)
                        .ok_or_else(|| invalid("rename source absent"))?;
                    c.execute(
                        &format!(
                            "ALTER TABLE {id} RENAME COLUMN {} TO {}",
                            quote(from),
                            quote(to)
                        ),
                        [],
                    )?;
                    if old.kind == ColumnType::Json {
                        c.execute(
                            &format!(
                                "ALTER TABLE {id} RENAME COLUMN {} TO {}",
                                quote(&format!("{from}$v")),
                                quote(&format!("{to}$v"))
                            ),
                            [],
                        )?;
                    }
                    r.columns = r
                        .columns
                        .into_iter()
                        .map(|(n, c)| (if n == *from { to.clone() } else { n }, c))
                        .collect();
                    for n in &mut r.key {
                        if n == from {
                            *n = to.clone()
                        }
                    }
                    for on in r.indexes.values_mut() {
                        for n in on {
                            if n == from {
                                *n = to.clone()
                            }
                        }
                    }
                }
                AlterEdit::Add { name, column } => {
                    if !column_name(name)
                        || r.columns.keys().any(|n| n.eq_ignore_ascii_case(name))
                        || (!column.optional && column.default.is_none())
                    {
                        return Err(invalid(
                            "added column must be fresh and optional or defaulted",
                        ));
                    }
                    column.check()?;
                    for def in column_sql(name, column)? {
                        c.execute(&format!("ALTER TABLE {id} ADD COLUMN {def}"), [])?;
                    }
                    r.columns.insert(name.clone(), column.clone());
                }
                AlterEdit::Drop { name } => {
                    if r.key.contains(name) || r.indexes.values().any(|on| on.contains(name)) {
                        return Err(invalid("cannot drop indexed column"));
                    }
                    let old = r
                        .columns
                        .shift_remove(name)
                        .ok_or_else(|| invalid("drop column absent"))?;
                    c.execute(&format!("ALTER TABLE {id} DROP COLUMN {}", quote(name)), [])?;
                    if old.kind == ColumnType::Json {
                        c.execute(
                            &format!(
                                "ALTER TABLE {id} DROP COLUMN {}",
                                quote(&format!("{name}$v"))
                            ),
                            [],
                        )?;
                    }
                }
                AlterEdit::Json { name, column } => {
                    column.check()?;
                    if column.kind != ColumnType::Json
                        || r.columns
                            .get(name)
                            .is_none_or(|c| c.kind != ColumnType::Json)
                    {
                        return Err(invalid("json edit needs existing json column"));
                    }
                    r.columns.insert(name.clone(), column.clone());
                }
            }
            tables.insert(table.clone(), r);
        }
        StoreWrite::Put {
            table,
            row,
            version,
            columns,
        } => {
            let r = record(tables, table)?;
            let params = put_params(r, row, *version, columns)?;
            c.execute(&put_sql(r), params_from_iter(params))?;
        }
        StoreWrite::Update {
            table,
            key,
            set,
            columns,
            count,
            version,
        } => {
            let r = record(tables, table)?;
            let mut p = Vec::new();
            let mut assigns = if set.is_empty() && version.is_some() {
                String::new()
            } else {
                set_sql(set, &r.columns, &r.key, columns, &mut p)?
            };
            if let Some(v) = version {
                safe(*v, 1)?;
                if !assigns.is_empty() {
                    assigns.push_str(", ");
                }
                assigns.push_str("\"$v\" = ?");
                p.push(SqlValue::Integer(*v));
            }
            let cond = key_sql(r, key, &mut p)?;
            counted(
                c,
                &format!("UPDATE {} SET {assigns} WHERE {cond}", quote(table)),
                p,
                count,
                replay,
            )?;
        }
        StoreWrite::Delete { table, key, count } => {
            let r = record(tables, table)?;
            let mut p = Vec::new();
            let cond = key_sql(r, key, &mut p)?;
            counted(
                c,
                &format!("DELETE FROM {} WHERE {cond}", quote(table)),
                p,
                count,
                replay,
            )?;
        }
        StoreWrite::UpdateWhere {
            table,
            r#where,
            set,
            columns,
            count,
            version,
        } => {
            let r = record(tables, table)?;
            let mut p = Vec::new();
            let mut assigns = if set.is_empty() && version.is_some() {
                String::new()
            } else {
                set_sql(set, &r.columns, &r.key, columns, &mut p)?
            };
            if let Some(v) = version {
                safe(*v, 1)?;
                if !assigns.is_empty() {
                    assigns.push_str(", ");
                }
                assigns.push_str("\"$v\" = ?");
                p.push(SqlValue::Integer(*v));
            }
            let cond = where_sql(r#where, &r.columns, &mut p)?;
            counted(
                c,
                &format!("UPDATE {} SET {assigns} WHERE {cond}", quote(table)),
                p,
                count,
                replay,
            )?;
        }
        StoreWrite::DeleteWhere {
            table,
            r#where,
            count,
        } => {
            let r = record(tables, table)?;
            let mut p = Vec::new();
            let cond = where_sql(r#where, &r.columns, &mut p)?;
            counted(
                c,
                &format!("DELETE FROM {} WHERE {cond}", quote(table)),
                p,
                count,
                replay,
            )?;
        }
        _ => apply_document(c, seq, write, archive)?,
    }
    Ok(())
}
pub(crate) fn document_row(c: &Connection, id: i64) -> Result<Option<DocumentRecord>> {
    Ok(c.query_row("SELECT id,owner,namespace,kind,key,history,created_at,retired_at,meta,host,revision,version,fingerprint FROM chord_documents WHERE id=?",[id],|r|{
    let history:String=r.get(5)?;let history=match history.as_str(){"none"=>DocumentHistory::None,"latest"=>DocumentHistory::Latest,_=>DocumentHistory::Rewindable};let key:String=r.get(4)?;
    Ok(DocumentRecord{id:r.get(0)?,owner:r.get(1)?,namespace:r.get(2)?,kind:r.get(3)?,key:(!key.is_empty()).then_some(key),history,created_at:r.get(6)?,retired_at:r.get(7)?,meta:r.get(8)?,host:r.get(9)?,revision:r.get(10)?,version:r.get(11)?,fingerprint:r.get(12)?})
}).optional()?)
}
fn live(c: &Connection, id: i64, revision: i64) -> Result<DocumentRecord> {
    safe(id, 1)?;
    safe(revision, 1)?;
    document_row(c, id)?
        .filter(|r| r.retired_at.is_none() && r.revision == revision)
        .ok_or_else(|| Error::Diverged(format!("document {id} is not live at revision {revision}")))
}
fn insert_revision(c: &Connection, id: i64, seq: i64, content: &DocumentContent) -> Result<()> {
    safe(id, 1)?;
    safe(seq, 1)?;
    safe(content.version(), 1)?;
    let (kind, text) = content.content()?;
    c.execute(
        "INSERT INTO chord_revisions(document_id,seq,kind,version,content) VALUES(?,?,?,?,?)",
        params![id, seq, kind, content.version(), text],
    )?;
    Ok(())
}
fn prune_retired(c: &Connection, r: &DocumentRecord) -> Result<()> {
    if r.history != DocumentHistory::Rewindable {
        c.execute("DELETE FROM chord_revisions WHERE document_id=?", [r.id])?;
    }
    if r.history == DocumentHistory::None {
        c.execute("DELETE FROM chord_documents WHERE id=?", [r.id])?;
    }
    Ok(())
}
fn apply_document(c: &Connection, seq: i64, write: &StoreWrite, archive: bool) -> Result<()> {
    match write {
        StoreWrite::DocumentCreate { record: r, base } => {
            for n in [r.id, r.created_at, r.revision, r.version] {
                safe(n, 1)?;
            }
            if let Some(n) = r.retired_at {
                safe(n, 1)?;
            }
            if base.is_none() && !archive {
                return Err(invalid("commit document creation requires base"));
            }
            c.execute("INSERT INTO chord_documents(id,owner,namespace,kind,key,history,created_at,retired_at,meta,host,revision,version,fingerprint) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",params![r.id,r.owner,r.namespace,r.kind,r.key.as_deref().unwrap_or(""),r.history.name(),r.created_at,r.retired_at,r.meta,r.host,r.revision,r.version,r.fingerprint])?;
            if let Some(b) = base {
                insert_revision(
                    c,
                    r.id,
                    r.created_at,
                    &DocumentContent::Base {
                        version: b.version,
                        fingerprint: None,
                        value: b.value.clone(),
                    },
                )?;
            }
            if r.retired_at.is_some() {
                prune_retired(c, r)?;
            }
        }
        StoreWrite::DocumentChange {
            id,
            revision,
            content,
        } => {
            let r = live(c, *id, *revision)?;
            safe(revision + 1, 1)?;
            safe(content.version(), 1)?;
            let fingerprint = match content {
                DocumentContent::Base { fingerprint, .. } => fingerprint
                    .as_ref()
                    .ok_or_else(|| invalid("changed base needs fingerprint"))?,
                DocumentContent::Delta { .. } => {
                    if content.version() != r.version {
                        return Err(Error::Diverged(
                            "delta version differs from current base".into(),
                        ));
                    }
                    &r.fingerprint
                }
            };
            c.execute(
                "UPDATE chord_documents SET revision=revision+1,version=?,fingerprint=? WHERE id=?",
                params![content.version(), fingerprint, id],
            )?;
            insert_revision(c, *id, seq, content)?;
            if matches!(content, DocumentContent::Base { .. })
                && r.history != DocumentHistory::Rewindable
            {
                c.execute(
                    "DELETE FROM chord_revisions WHERE document_id=? AND seq<?",
                    params![id, seq],
                )?;
            }
        }
        StoreWrite::DocumentRetire { id, revision } => {
            let r = live(c, *id, *revision)?;
            c.execute(
                "UPDATE chord_documents SET retired_at=? WHERE id=?",
                params![seq, id],
            )?;
            prune_retired(c, &r)?;
        }
        StoreWrite::DocumentRevision { id, seq, content } => {
            if !archive {
                return Err(invalid("document.revision is archive-only"));
            }
            if document_row(c, *id)?.is_none() {
                return Err(invalid("revision without document record"));
            }
            insert_revision(c, *id, *seq, content)?;
        }
        _ => return Err(invalid("not a document write")),
    }
    Ok(())
}
