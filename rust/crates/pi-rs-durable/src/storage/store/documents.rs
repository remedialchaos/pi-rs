use pi_rs_chord::{
    Error, Result,
    delta::{Op, apply},
    json::JsonValue,
    store::{
        DocumentBase, DocumentContent as EngineContent, DocumentHistory as EngineHistory,
        DocumentRecord as EngineRecord, StoreWrite,
    },
};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::storage_types::*;

use super::{checked_id, number, tables::encode_indexed};

pub(super) const SCOPE_NAMESPACES: [&str; 3] = ["session", "conversation:", "task:"];

fn namespace(scope: DocumentScope) -> String {
    match scope {
        DocumentScope::Session => "session".into(),
        DocumentScope::Conversation { conversation_id } => {
            format!("conversation:{conversation_id}")
        }
        DocumentScope::Task { task_id } => format!("task:{task_id}"),
    }
}

fn point(at: DocumentPoint) -> Option<i64> {
    match at {
        DocumentPoint::Current => None,
        DocumentPoint::Seq(seq) => Some(number(seq)),
    }
}

pub(super) fn create(
    owner: &str,
    record: DocumentCreate,
    base: DocumentBase,
    seq: i64,
    retire: bool,
) -> Result<StoreWrite> {
    let history = if matches!(record.scope, DocumentScope::Conversation { .. })
        && record.history == Some(DocumentHistory::Rewindable)
    {
        EngineHistory::Rewindable
    } else {
        EngineHistory::Latest
    };
    let mut engine = EngineRecord {
        id: checked_id(record.id)?,
        owner: owner.to_owned(),
        namespace: namespace(record.scope),
        kind: encode_indexed(&record.kind)?,
        key: record.key.as_deref().map(encode_indexed).transpose()?,
        history,
        created_at: seq,
        retired_at: retire.then_some(seq),
        meta: None,
        host: None,
        revision: 1,
        version: base.version,
        fingerprint: String::new(),
    };
    engine.meta = Some(serde_json::to_string(
        &record.stamp(super::unsigned(seq)?, None),
    )?);
    Ok(StoreWrite::DocumentCreate {
        record: engine,
        base: Some(base),
    })
}

pub(super) fn base(version: u64, value: JsonObject) -> Result<DocumentBase> {
    Ok(DocumentBase {
        version: super::checked_version(version)?,
        fingerprint: String::new(),
        value: JsonValue::from_serde(Value::Object(value)),
    })
}

pub(super) fn change(content: &DocumentContent) -> Result<EngineContent> {
    Ok(match content {
        DocumentContent::Base { version, value } => {
            let base = base(*version, value.clone())?;
            EngineContent::Base {
                version: base.version,
                fingerprint: Some(base.fingerprint),
                value: base.value,
            }
        }
        DocumentContent::Delta { version, ops } => EngineContent::Delta {
            version: super::checked_version(*version)?,
            ops: ops.iter().map(Op::to_json).collect(),
        },
    })
}

pub(super) struct RevisionRow {
    pub record: DocumentRecord,
    pub latest: bool,
    pub revision: i64,
    pub version: i64,
}

fn decode_record(text: &str, retired_at: Option<i64>) -> Result<DocumentRecord> {
    let mut record: DocumentRecord = serde_json::from_str(text)?;
    record.retired_at = retired_at.map(super::unsigned).transpose()?;
    Ok(record)
}

pub(super) fn by_id(c: &Connection, owner: &str, id: i64) -> Result<Option<RevisionRow>> {
    let mut statement = c.prepare_cached(
        "SELECT meta,retired_at,history,revision,version FROM chord_documents
         WHERE id=?1 AND owner=?2 AND meta IS NOT NULL AND
         (namespace='session' OR namespace GLOB 'conversation:*' OR namespace GLOB 'task:*')",
    )?;
    let mut rows = statement.query(params![id, owner])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(RevisionRow {
        record: decode_record(text(row, 0)?, row.get(1)?)?,
        latest: text(row, 2)? != "rewindable",
        revision: row.get(3)?,
        version: row.get(4)?,
    }))
}

pub(super) fn occupied(c: &Connection, id: i64) -> Result<bool> {
    Ok(
        c.prepare_cached("SELECT EXISTS(SELECT 1 FROM chord_documents WHERE id=?)")?
            .query_row([id], |row| row.get(0))?,
    )
}

pub(super) fn find(
    c: &Connection,
    owner: &str,
    address: &DocumentAddress,
    at: DocumentPoint,
) -> Result<Option<DocumentRecord>> {
    let mut statement = c.prepare_cached(match at {
        DocumentPoint::Current =>
            "SELECT meta,retired_at FROM chord_documents WHERE owner=?1 AND namespace=?2 AND kind=?3 AND key=?4 AND retired_at IS NULL AND meta IS NOT NULL",
        DocumentPoint::Seq(_) =>
            "SELECT meta,retired_at FROM chord_documents WHERE owner=?1 AND namespace=?2 AND kind=?3 AND key=?4 AND created_at<=?5 AND (retired_at IS NULL OR retired_at>?5) AND meta IS NOT NULL ORDER BY created_at DESC LIMIT 1",
    })?;
    let namespace = namespace(address.scope);
    let kind = encode_indexed(&address.kind)?;
    let key = address
        .key
        .as_deref()
        .map(encode_indexed)
        .transpose()?
        .unwrap_or_default();
    let mut rows = match point(at) {
        None => statement.query(params![owner, namespace, kind, key])?,
        Some(seq) => statement.query(params![owner, namespace, kind, key, seq])?,
    };
    rows.next()?
        .map(|row| decode_record(text(row, 0)?, row.get(1)?))
        .transpose()
}

pub(super) fn scan(
    c: &Connection,
    owner: &str,
    query: &DocumentQuery,
    after: i64,
    limit: usize,
) -> Result<Vec<DocumentRecord>> {
    let mut sql = String::from(
        "SELECT meta,retired_at FROM chord_documents WHERE owner=?1 AND namespace=?2 AND id>?3 AND meta IS NOT NULL",
    );
    sql.push_str(match query.at {
        DocumentPoint::Current => " AND retired_at IS NULL",
        DocumentPoint::Seq(_) => " AND created_at<=?6 AND (retired_at IS NULL OR retired_at>?6)",
    });
    if query.kind.is_some() {
        sql.push_str(" AND kind=?5");
    }
    sql.push_str(" ORDER BY id ASC LIMIT ?4");
    let mut statement = c.prepare_cached(&sql)?;
    let namespace = namespace(query.scope);
    let kind = query.kind.as_deref().map(encode_indexed).transpose()?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut rows = match (kind, point(query.at)) {
        (None, None) => statement.query(params![owner, namespace, after, limit])?,
        (kind, None) => statement.query(params![owner, namespace, after, limit, kind])?,
        (kind, Some(seq)) => statement.query(params![owner, namespace, after, limit, kind, seq])?,
    };
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push(decode_record(text(row, 0)?, row.get(1)?)?);
    }
    Ok(records)
}

pub(super) fn load(
    c: &Connection,
    owner: &str,
    id: i64,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>> {
    let Some(found) = by_id(c, owner, id)? else {
        return Ok(None);
    };
    if at != DocumentPoint::Current && found.latest {
        return Err(Error::Invalid(format!(
            "Document {id} does not retain historical content"
        )));
    }
    if !found.record.alive_at(at) {
        return Ok(None);
    }
    let seq = point(at).unwrap_or(i64::MAX);
    let mut statement = c.prepare_cached(
        "SELECT kind,version,content FROM chord_revisions WHERE document_id=?1 AND seq<=?2 AND seq>=(
           SELECT max(seq) FROM chord_revisions WHERE document_id=?1 AND kind='base' AND seq<=?2
         ) ORDER BY seq",
    )?;
    let mut rows = statement.query(params![id, seq])?;
    let missing_base = || {
        Error::Invalid(format!(
            "Document {id} has no stored base at the requested point"
        ))
    };
    let first = rows.next()?.ok_or_else(missing_base)?;
    if text(first, 0)? != "base" {
        return Err(missing_base());
    }
    let version: i64 = first.get(1)?;
    let mut value: JsonValue = serde_json::from_str(text(first, 2)?)?;
    let mut deltas_since_base = 0;
    while let Some(row) = rows.next()? {
        if text(row, 0)? != "delta" || row.get::<_, i64>(1)? != version {
            return Err(Error::Invalid(format!(
                "Document {id} has incompatible revisions"
            )));
        }
        let wire: Vec<JsonValue> = serde_json::from_str(text(row, 2)?)?;
        let ops = wire
            .iter()
            .map(Op::from_json)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        value = apply(value, &ops)?;
        deltas_since_base += 1;
    }
    let Value::Object(value) = value.to_serde()? else {
        return Err(Error::Invalid(format!(
            "Document {id} content is not a JSON object"
        )));
    };
    Ok(Some(StoredDocument {
        record: found.record,
        version: super::unsigned(version)?,
        value,
        deltas_since_base,
    }))
}

fn text<'a>(row: &'a rusqlite::Row<'_>, column: usize) -> Result<&'a str> {
    Ok(row
        .get_ref(column)?
        .as_str()
        .map_err(rusqlite::Error::from)?)
}
