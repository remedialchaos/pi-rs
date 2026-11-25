//! Declared tables and their lossless recorded form.
use crate::{
    Error, Result,
    contracts::{check_name, hash64},
};
use indexmap::IndexMap;
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub(crate) const SAFE: i64 = 9_007_199_254_740_991;
pub(crate) fn invalid(message: impl Into<String>) -> Error {
    crate::data::ChordError::invalid("args", message, None).into()
}
pub(crate) fn safe(n: i64, min: i64) -> Result<()> {
    if (min..=SAFE).contains(&n) {
        Ok(())
    } else {
        Err(invalid("store: integer outside safe range"))
    }
}
pub(crate) fn column_name(n: &str) -> bool {
    let mut b = n.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
pub(crate) fn index_name(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
pub(crate) fn sql_name(n: &str) -> Result<()> {
    let parts = n.split(':').collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(invalid("table: expected owner:table"));
    }
    for p in parts {
        check_name(p, "table name part")?;
    }
    Ok(())
}
pub(crate) fn quote(n: &str) -> String {
    format!("\"{}\"", n.replace('"', "\"\""))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Str,
    Int,
    Num,
    Bool,
    Json,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnRecord {
    #[serde(rename = "type")]
    pub kind: ColumnType,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}
impl ColumnRecord {
    pub fn new(kind: ColumnType) -> Self {
        Self {
            kind,
            optional: false,
            default: None,
            version: None,
            fingerprint: None,
            host: None,
        }
    }
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
    pub fn defaulted(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }
    pub fn json(version: i64, fingerprint: impl Into<String>) -> Self {
        Self {
            version: Some(version),
            fingerprint: Some(fingerprint.into()),
            ..Self::new(ColumnType::Json)
        }
    }
    pub fn host(kind: impl Into<String>) -> Self {
        Self {
            host: Some(kind.into()),
            ..Self::new(ColumnType::Str)
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        if (self.kind == ColumnType::Json) != (self.version.is_some() && self.fingerprint.is_some())
            || (self.kind != ColumnType::Json
                && (self.version.is_some() || self.fingerprint.is_some()))
        {
            return Err(invalid(
                "json column requires version and fingerprint, scalar column forbids them",
            ));
        }
        if let Some(v) = self.version {
            safe(v, 1)?;
        }
        if let Some(host) = &self.host {
            check_name(host, "host column")?;
            if self.kind != ColumnType::Str || self.optional || self.default.is_some() {
                return Err(invalid("host column is a non-opt str without default"));
            }
        }
        if let Some(default) = &self.default {
            if self.optional || self.kind == ColumnType::Json {
                return Err(invalid("only non-opt scalar columns have defaults"));
            }
            scalar(self.kind, default)?;
        }
        Ok(())
    }
}
pub(crate) fn scalar(kind: ColumnType, v: &Value) -> Result<SqlValue> {
    let value = match kind {
        ColumnType::Str => v
            .as_str()
            .filter(|s| !s.contains('\0'))
            .map(|s| SqlValue::Text(s.into())),
        ColumnType::Int => v
            .as_f64()
            .filter(|n| n.is_finite() && n.fract() == 0.0 && n.abs() <= SAFE as f64)
            .map(|n| SqlValue::Integer(n as i64)),
        ColumnType::Num => v.as_f64().filter(|n| n.is_finite()).map(SqlValue::Real),
        ColumnType::Bool => v.as_bool().map(|b| SqlValue::Integer(i64::from(b))),
        ColumnType::Json => None,
    };
    value.ok_or_else(|| invalid(format!("{v} is not a {kind:?} value")))
}
#[derive(Clone, Debug)]
pub struct TableDefinition {
    pub name: String,
    pub version: i64,
    pub columns: IndexMap<String, ColumnRecord>,
    pub key: Vec<String>,
    pub indexes: IndexMap<String, Vec<String>>,
    pub fingerprint: String,
    pub(crate) hosts: IndexMap<String, crate::contracts::HostKind>,
    pub(crate) json: IndexMap<String, super::JsonColumn>,
}
impl TableDefinition {
    pub fn new(
        name: impl Into<String>,
        version: i64,
        columns: IndexMap<String, ColumnRecord>,
        key: Vec<String>,
        mut indexes: IndexMap<String, Vec<String>>,
    ) -> Result<Self> {
        let name = name.into();
        check_name(&name, "table")?;
        safe(version, 1)?;
        check_columns(&columns, &key)?;
        for (n, on) in &indexes {
            if !index_name(n) {
                return Err(invalid("invalid index name"));
            }
            check_order(&columns, on)?;
        }
        let host = columns
            .iter()
            .find(|(_, c)| c.host.is_some())
            .map(|(n, _)| n.clone());
        let auto = host.filter(|h| key.first() != Some(h));
        if indexes
            .get("host")
            .is_some_and(|on| auto.as_ref().is_none_or(|h| on != std::slice::from_ref(h)))
        {
            return Err(invalid("index host is reserved for the host column"));
        }
        if let Some(h) = auto {
            indexes.insert("host".into(), vec![h]);
        }
        let fingerprint = table_fingerprint(&columns, &key, &indexes)?;
        Ok(Self {
            name,
            version,
            columns,
            key,
            indexes,
            fingerprint,
            hosts: IndexMap::new(),
            json: IndexMap::new(),
        })
    }
    /// Runtime host schemas are not persisted in the recorded table form.
    pub fn bind_host(mut self, column: &str, host: crate::contracts::HostKind) -> Result<Self> {
        if self.columns.get(column).and_then(|c| c.host.as_deref()) != Some(host.kind.as_str()) {
            return Err(invalid("host binding differs from column kind"));
        }
        self.hosts.insert(column.into(), host);
        Ok(self)
    }
    pub fn bind_json(mut self, column: &str, json: super::JsonColumn) -> Result<Self> {
        let record = self
            .columns
            .get(column)
            .ok_or_else(|| invalid("JSON binding column absent"))?;
        if record.kind != ColumnType::Json
            || record.version != Some(json.version())
            || record.fingerprint.as_deref() != Some(json.fingerprint())
        {
            return Err(invalid("JSON binding differs from recorded column"));
        }
        self.json.insert(column.into(), json);
        Ok(self)
    }
    pub fn declaration(&self, owner: &str) -> Result<Vec<super::StoreWrite>> {
        check_name(owner, "owner")?;
        let table = format!("{owner}:{}", self.name);
        let mut writes = vec![super::StoreWrite::Table {
            table: table.clone(),
            version: self.version,
            state: TableState::Ready,
            columns: self.columns.clone(),
            key: self.key.clone(),
            fingerprint: self.fingerprint.clone(),
        }];
        writes.extend(
            self.indexes
                .iter()
                .map(|(name, columns)| super::StoreWrite::Index {
                    table: table.clone(),
                    name: name.clone(),
                    columns: columns.clone(),
                }),
        );
        Ok(writes)
    }
    pub fn put(&self, owner: &str, row: Value) -> Result<super::StoreWrite> {
        check_name(owner, "owner")?;
        let fields = row
            .as_object()
            .ok_or_else(|| invalid("put: expected object"))?;
        if fields.keys().any(|k| !self.columns.contains_key(k)) {
            return Err(invalid("put: unknown column"));
        }
        let mut row = IndexMap::new();
        let mut columns = IndexMap::new();
        for (n, c) in &self.columns {
            let value = fields.get(n).or(c.default.as_ref());
            let Some(value) = value else {
                if !c.optional {
                    return Err(invalid(format!("put: column {n} missing")));
                }
                continue;
            };
            let canonical;
            let value = if c.host.is_some() {
                let host = self
                    .hosts
                    .get(n)
                    .ok_or_else(|| invalid("host column needs its runtime kind binding"))?;
                let input = crate::json::JsonValue::from_serde(value.clone());
                canonical = Value::String(
                    host.key_of_address(&input)?
                        .ok_or_else(|| invalid("host cell is not an address of its kind"))?,
                );
                &canonical
            } else {
                value
            };
            if c.kind != ColumnType::Json {
                scalar(c.kind, value)?;
            } else {
                columns.insert(
                    n.clone(),
                    c.version.ok_or_else(|| invalid("json version missing"))?,
                );
            }
            row.insert(n.clone(), value.clone());
        }
        Ok(super::StoreWrite::Put {
            table: format!("{owner}:{}", self.name),
            row,
            version: self.version,
            columns,
        })
    }
}
pub fn table_fingerprint(
    columns: &IndexMap<String, ColumnRecord>,
    key: &[String],
    indexes: &IndexMap<String, Vec<String>>,
) -> Result<String> {
    let mut columns: Vec<_> = columns.iter().collect();
    columns.sort_by(|a, b| a.0.cmp(b.0));
    let mut indexes: Vec<_> = indexes.iter().collect();
    indexes.sort_by(|a, b| a.0.cmp(b.0));
    let columns: IndexMap<_, _> = columns.into_iter().collect();
    let indexes: IndexMap<_, _> = indexes.into_iter().collect();
    Ok(hash64(&serde_json::to_string(
        &json!({"tableFormVersion":1,"value":{"columns":columns,"key":key,"indexes":indexes}}),
    )?))
}
pub(crate) fn check_columns(
    columns: &IndexMap<String, ColumnRecord>,
    key: &[String],
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (n, c) in columns {
        if !column_name(n) || !names.insert(n.to_ascii_lowercase()) {
            return Err(invalid("invalid or case-colliding column name"));
        }
        c.check()?;
    }
    if columns.values().filter(|c| c.host.is_some()).count() > 1 {
        return Err(invalid("more than one host column"));
    }
    if key.is_empty() {
        return Err(invalid("table key is empty"));
    }
    check_order(columns, key)
}
pub(crate) fn check_order(columns: &IndexMap<String, ColumnRecord>, on: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for n in on {
        let c = columns
            .get(n)
            .ok_or_else(|| invalid("unknown order column"))?;
        if !seen.insert(n) || c.optional || !matches!(c.kind, ColumnType::Str | ColumnType::Int) {
            return Err(invalid("order columns are distinct non-opt str or int"));
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableState {
    Ready,
    Migrating,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableRecord {
    pub table: String,
    pub version: i64,
    pub state: TableState,
    pub columns: IndexMap<String, ColumnRecord>,
    pub key: Vec<String>,
    pub fingerprint: String,
    pub indexes: IndexMap<String, Vec<String>>,
}
impl TableRecord {
    pub(crate) fn physical(&self) -> Vec<String> {
        let mut p = self.columns.keys().cloned().collect::<Vec<_>>();
        p.extend(
            self.columns
                .iter()
                .filter(|(_, c)| c.kind == ColumnType::Json)
                .map(|(n, _)| format!("{n}$v")),
        );
        p.push("$v".into());
        p
    }
}
pub(crate) fn column_sql(name: &str, c: &ColumnRecord) -> Result<Vec<String>> {
    let id = quote(name);
    let nullable = if c.optional { "" } else { " NOT NULL" };
    let default = match &c.default {
        None => String::new(),
        Some(Value::String(s)) => format!(" DEFAULT '{}'", s.replace('\'', "''")),
        Some(Value::Bool(b)) => format!(" DEFAULT {}", i64::from(*b)),
        Some(n) => format!(" DEFAULT {n}"),
    };
    let ty = match c.kind {
        ColumnType::Str | ColumnType::Json => "TEXT",
        ColumnType::Int | ColumnType::Bool => "INTEGER",
        ColumnType::Num => "REAL",
    };
    let check = match c.kind {
        ColumnType::Int => format!(" CHECK ({id} BETWEEN -{SAFE} AND {SAFE})"),
        ColumnType::Bool => format!(" CHECK ({id} IN (0, 1))"),
        _ => String::new(),
    };
    let mut defs = vec![format!("{id} {ty}{nullable}{default}{check}")];
    if c.kind == ColumnType::Json {
        defs.push(format!("{} INTEGER{nullable}", quote(&format!("{name}$v"))))
    }
    Ok(defs)
}
pub(crate) fn create_table(record: &TableRecord) -> Result<String> {
    let mut defs = Vec::new();
    for (n, c) in &record.columns {
        defs.extend(column_sql(n, c)?)
    }
    defs.push("\"$v\" INTEGER NOT NULL".into());
    defs.push(format!(
        "PRIMARY KEY ({})",
        record
            .key
            .iter()
            .map(|n| quote(n))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let rowid = record.key.len() == 1
        && record
            .columns
            .get(&record.key[0])
            .is_some_and(|c| c.kind == ColumnType::Int);
    Ok(format!(
        "CREATE TABLE {} ({}) STRICT{}",
        quote(&record.table),
        defs.join(", "),
        if rowid { "" } else { ", WITHOUT ROWID" }
    ))
}
