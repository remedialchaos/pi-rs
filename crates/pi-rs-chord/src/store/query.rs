//! Immutable queries, keyset pages and wire-compatible cursors.
use super::{TableDefinition, expression::*, table::*};
use crate::Result;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
#[derive(Clone, Debug)]
pub struct Query {
    pub table: String,
    pub definition: TableDefinition,
    pub condition: Option<WhereNode>,
    pub order: Vec<String>,
    pub descending: bool,
    pub limit: usize,
    pub from: Option<Vec<Value>>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub rows: Vec<Value>,
    pub cursor: Option<String>,
}
impl Query {
    pub fn new(owner: &str, definition: TableDefinition) -> Result<Self> {
        crate::contracts::check_name(owner, "owner")?;
        Ok(Self {
            table: format!("{owner}:{}", definition.name),
            definition,
            condition: None,
            order: Vec::new(),
            descending: false,
            limit: 100,
            from: None,
        })
    }
    pub fn filter(mut self, condition: WhereNode) -> Result<Self> {
        where_sql(&condition, &self.definition.columns, &mut Vec::new())?;
        self.condition = Some(match self.condition.take() {
            None => condition,
            Some(old) => WhereNode::And(vec![old, condition]),
        });
        Ok(self)
    }
    pub fn order_by(mut self, column: &str, descending: bool) -> Result<Self> {
        check_order(&self.definition.columns, &[column.into()])?;
        if self.order.contains(&column.into())
            || (!self.order.is_empty() && self.descending != descending)
        {
            return Err(invalid("order repeats a column or changes direction"));
        }
        self.order.push(column.into());
        self.descending = descending;
        Ok(self)
    }
    pub fn with_limit(mut self, limit: usize) -> Result<Self> {
        if limit == 0 || limit as u128 >= SAFE as u128 {
            return Err(invalid("limit is a positive safe integer"));
        }
        self.limit = limit;
        Ok(self)
    }
    pub fn order_columns(&self) -> Vec<String> {
        let mut on = self.order.clone();
        on.extend(
            self.definition
                .key
                .iter()
                .filter(|n| !self.order.contains(n))
                .cloned(),
        );
        on
    }
    pub fn after(mut self, cursor: Option<&str>) -> Result<Self> {
        let Some(cursor) = cursor else {
            self.from = None;
            return Ok(self);
        };
        let value: Value = serde_json::from_slice(&unbase64(cursor)?)?;
        let order = self.order_columns();
        let dir = if self.descending { "desc" } else { "asc" };
        if value.get("t") != Some(&Value::from(self.table.clone()))
            || value.get("o") != Some(&json!(order))
            || value.get("d") != Some(&Value::from(dir))
        {
            return Err(invalid("cursor does not belong to this table and order"));
        }
        let values = value
            .get("v")
            .and_then(Value::as_array)
            .filter(|v| v.len() == order.len())
            .ok_or_else(|| invalid("cursor value count differs"))?;
        for (n, v) in order.iter().zip(values) {
            scalar(
                self.definition
                    .columns
                    .get(n)
                    .ok_or_else(|| invalid("cursor column absent"))?
                    .kind,
                v,
            )?;
        }
        self.from = Some(values.clone());
        Ok(self)
    }
    pub(crate) fn statement(&self, count: bool) -> Result<Statement> {
        check_order(&self.definition.columns, &self.order_columns())?;
        if self.limit == 0 || self.limit as u128 >= SAFE as u128 {
            return Err(invalid("limit is a positive safe integer"));
        }
        let mut params = Vec::new();
        let mut conditions = Vec::new();
        if let Some(condition) = &self.condition {
            conditions.push(where_sql(condition, &self.definition.columns, &mut params)?)
        }
        let order = self.order_columns();
        if !count && let Some(from) = &self.from {
            if from.len() != order.len() {
                return Err(invalid("invalid keyset position"));
            }
            conditions.push(format!(
                "({}) {} ({})",
                order
                    .iter()
                    .map(|n| quote(n))
                    .collect::<Vec<_>>()
                    .join(", "),
                if self.descending { "<" } else { ">" },
                vec!["?"; order.len()].join(", ")
            ));
            for (n, v) in order.iter().zip(from) {
                params.push(scalar(self.definition.columns[n].kind, v)?);
            }
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let select = if count {
            "count(*)".into()
        } else {
            let mut names = self.definition.columns.keys().cloned().collect::<Vec<_>>();
            names.extend(
                self.definition
                    .columns
                    .iter()
                    .filter(|(_, c)| c.kind == ColumnType::Json)
                    .map(|(n, _)| format!("{n}$v")),
            );
            names
                .iter()
                .map(|n| quote(n))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut sql = format!("SELECT {select} FROM {}{filter}", quote(&self.table));
        if !count {
            sql.push_str(&format!(
                " ORDER BY {} LIMIT ?",
                order
                    .iter()
                    .map(|n| format!(
                        "{} {}",
                        quote(n),
                        if self.descending { "DESC" } else { "ASC" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            params.push(SqlValue::Integer((self.limit + 1) as i64));
        }
        Ok(Statement { sql, params })
    }
}
pub(crate) fn read_page(c: &Connection, query: &Query) -> Result<Page> {
    let statement = query.statement(false)?;
    let mut s = c.prepare_cached(&statement.sql)?;
    let mut rows = s.query(params_from_iter(statement.params))?;
    let mut output = Vec::new();
    while let Some(row) = rows.next()? {
        let mut value = serde_json::Map::new();
        for (i, (n, col)) in query.definition.columns.iter().enumerate() {
            let v: SqlValue = row.get(i)?;
            let v = match v {
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
                        let version: Option<i64> = row.get(format!("{n}$v").as_str())?;
                        let version = version
                            .filter(|v| *v >= 1 && Some(*v) <= col.version)
                            .ok_or_else(|| invalid("unreadable json cell version"))?;
                        if let Some(json) = query.definition.json.get(n) {
                            json.read(&text, version)?.to_serde()?
                        } else if Some(version) == col.version {
                            serde_json::from_str(&text)?
                        } else {
                            return Err(crate::data::ChordError::new(
                                "unavailable",
                                "JSON cell has no migration binding",
                            )
                            .into());
                        }
                    } else {
                        Value::String(text)
                    }
                }
                SqlValue::Blob(_) => return Err(invalid("blob in declared table")),
            };
            value.insert(n.clone(), v);
        }
        output.push(Value::Object(value));
    }
    let cursor = if output.len() > query.limit {
        output.truncate(query.limit);
        let last = output.last().ok_or_else(|| invalid("empty page"))?;
        let order = query.order_columns();
        let values = order
            .iter()
            .map(|n| {
                last.get(n)
                    .cloned()
                    .ok_or_else(|| invalid("missing order cell"))
            })
            .collect::<Result<Vec<_>>>()?;
        Some(base64(&serde_json::to_vec(
            &json!({"t":query.table,"o":order,"d":if query.descending{"desc"}else{"asc"},"v":values}),
        )?))
    } else {
        None
    };
    for row in &mut output {
        for (name, column) in &query.definition.columns {
            if column.host.is_none() {
                continue;
            }
            let host = query
                .definition
                .hosts
                .get(name)
                .ok_or_else(|| invalid("host column needs its runtime kind binding"))?;
            let key = row
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("host cell is not a stored key"))?;
            let address = host.address_of_key(key)?;
            row[name] =
                json!({"kind":address.kind,"params":address.params.to_serde()?,"key":address.key});
        }
    }
    Ok(Page {
        rows: output,
        cursor,
    })
}
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut bits, mut count) = (0u32, 0);
    for b in bytes {
        bits = (bits << 8) | u32::from(*b);
        count += 8;
        while count >= 6 {
            count -= 6;
            out.push(ALPHABET[((bits >> count) & 63) as usize] as char);
        }
    }
    if count > 0 {
        out.push(ALPHABET[((bits << (6 - count)) & 63) as usize] as char)
    }
    out
}
fn unbase64(text: &str) -> Result<Vec<u8>> {
    let (mut bits, mut count) = (0u32, 0);
    let mut out = Vec::new();
    for b in text.trim_end_matches('=').bytes() {
        let b = match b {
            b'+' => b'-',
            b'/' => b'_',
            other => other,
        };
        let v = ALPHABET
            .iter()
            .position(|x| *x == b)
            .ok_or_else(|| invalid("cursor is not base64"))?;
        bits = (bits << 6) | v as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8)
        }
    }
    if count == 6 {
        return Err(invalid("malformed base64"));
    }
    Ok(out)
}
