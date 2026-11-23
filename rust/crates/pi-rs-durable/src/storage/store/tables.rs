use pi_rs_chord::{
    Error, Result,
    contracts::schema,
    store::{ColumnRecord, ColumnType, JsonColumn, StoreWrite, TableDefinition, TableState},
};
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::storage_types::{ScanOrder, StoredEntry};

pub(super) struct Tables {
    pub ids: Table,
    pub conversations: Table,
    pub entries: Table,
    pub heads: Table,
    pub tasks: Table,
    pub submissions: Table,
}

pub(super) struct Table {
    definition: TableDefinition,
    name: String,
}

impl Tables {
    pub fn new(owner: &str) -> Result<Self> {
        use ColumnType::{Int, Str};
        let record = JsonColumn::new(schema::unknown(), 1, None)?.record();
        let table = |name, columns: &[(&str, ColumnType)], stored, indexes: &[(&str, &[&str])]| {
            let mut columns: indexmap::IndexMap<_, _> = columns
                .iter()
                .map(|&(name, kind)| (name.to_owned(), ColumnRecord::new(kind)))
                .collect();
            if stored {
                columns.insert("record".into(), record.clone());
            }
            Ok::<_, Error>(Table {
                definition: TableDefinition::new(
                    name,
                    1,
                    columns,
                    vec!["id".into()],
                    indexes
                        .iter()
                        .map(|&(name, columns)| {
                            (
                                name.to_owned(),
                                columns.iter().map(|&c| c.to_owned()).collect(),
                            )
                        })
                        .collect(),
                )?,
                name: format!("{owner}:{name}"),
            })
        };
        Ok(Self {
            ids: table(
                "record_ids",
                &[("id", Int), ("record_type", Str)],
                false,
                &[],
            )?,
            conversations: table(
                "conversations",
                &[
                    ("id", Int),
                    ("owner_conversation_id", Int),
                    ("owner_task_id", Int),
                ],
                true,
                &[
                    ("by_owner_conversation", &["owner_conversation_id"]),
                    ("by_owner_task", &["owner_task_id"]),
                ],
            )?,
            entries: table(
                "entries",
                &[("id", Int), ("conversation_id", Int), ("commit_seq", Int)],
                true,
                &[("by_conversation", &["conversation_id"])],
            )?,
            heads: table(
                "entry_heads",
                &[("id", Int), ("conversation_id", Int)],
                false,
                &[("by_conversation", &["conversation_id"])],
            )?,
            tasks: table(
                "tasks",
                &[
                    ("id", Int),
                    ("conversation_id", Int),
                    ("kind", Str),
                    ("status", Str),
                    ("abort_requested", Int),
                    ("background", Int),
                ],
                true,
                &[
                    ("by_status", &["status"]),
                    ("by_conversation", &["conversation_id"]),
                    ("by_kind", &["kind"]),
                    ("by_abort_requested", &["abort_requested"]),
                    ("by_background", &["background"]),
                ],
            )?,
            submissions: table(
                "submissions",
                &[
                    ("id", Int),
                    ("conversation_id", Int),
                    ("request_id", Str),
                    ("status", Str),
                ],
                true,
                &[
                    ("by_request", &["conversation_id", "request_id"]),
                    ("by_conversation", &["conversation_id"]),
                    ("by_status", &["status"]),
                ],
            )?,
        })
    }

    pub fn all(&self) -> [&Table; 6] {
        [
            &self.ids,
            &self.conversations,
            &self.entries,
            &self.heads,
            &self.tasks,
            &self.submissions,
        ]
    }
}

impl Table {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn prepare(&self, owner: &str, step: &mut pi_rs_chord::store::Step<'_>) -> Result<bool> {
        if let Some(existing) = step.table(&self.name) {
            if existing.version != self.definition.version
                || existing.fingerprint != self.definition.fingerprint
                || existing.state != TableState::Ready
            {
                return Err(Error::Invalid(format!(
                    "Store table {} differs from Durable's declaration",
                    self.name
                )));
            }
            return Ok(false);
        }
        for write in self.definition.declaration(owner)? {
            step.apply(write)?;
        }
        Ok(true)
    }

    pub fn put(&self, owner: &str, row: Value) -> Result<StoreWrite> {
        self.definition.put(owner, row)
    }

    pub fn claim(&self, owner: &str, id: i64, kind: &str) -> Result<StoreWrite> {
        self.put(owner, json!({"id": id, "record_type": kind}))
    }

    pub fn record_type(&self, c: &Connection, id: i64) -> Result<Option<String>> {
        Ok(c.prepare_cached(&format!(
            "SELECT record_type FROM \"{}\" WHERE id=?",
            self.name
        ))?
        .query_row([id], |r| r.get(0))
        .optional()?)
    }

    pub fn highest_id(&self, c: &Connection) -> Result<Option<i64>> {
        Ok(
            c.query_row(&format!("SELECT max(id) FROM \"{}\"", self.name), [], |r| {
                r.get(0)
            })?,
        )
    }

    pub fn get<T: DeserializeOwned>(&self, c: &Connection, id: i64) -> Result<Option<T>> {
        let mut statement = c.prepare_cached(&format!(
            "SELECT record, \"record$v\" FROM \"{}\" WHERE id=?",
            self.name
        ))?;
        let row = statement
            .query_row([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .optional()?;
        row.map(|(text, version)| decode(&text, version))
            .transpose()
    }

    pub fn entry(&self, c: &Connection, id: i64) -> Result<Option<StoredEntry>> {
        let mut statement = c.prepare_cached(&format!(
            "SELECT record, \"record$v\", commit_seq FROM \"{}\" WHERE id=?",
            self.name
        ))?;
        let row = statement
            .query_row([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .optional()?;
        row.map(|(text, version, commit_seq)| {
            Ok(StoredEntry {
                entry: decode(&text, version)?,
                commit_seq: super::unsigned(commit_seq)?,
            })
        })
        .transpose()
    }

    pub fn select<T: DeserializeOwned>(
        &self,
        c: &Connection,
        conditions: &Conditions,
        order: ScanOrder,
        limit: usize,
        out: &mut Vec<T>,
    ) -> Result<()> {
        let sql = conditions.sql("record, \"record$v\"", &self.name, order);
        let mut statement = c.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = statement.query(params_from_iter(conditions.bind(&limit)))?;
        while let Some(row) = rows.next()? {
            let text = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            out.push(decode(text, row.get(1)?)?);
        }
        Ok(())
    }

    pub fn first_id(&self, c: &Connection, conditions: &Conditions) -> Result<Option<i64>> {
        let sql = conditions.sql("id", &self.name, ScanOrder::Descending);
        Ok(c.prepare_cached(&sql)?
            .query_row(params_from_iter(conditions.bind(&1)), |r| r.get(0))
            .optional()?)
    }
}

fn decode<T: DeserializeOwned>(text: &str, version: i64) -> Result<T> {
    if version != 1 {
        return Err(Error::Invalid(format!(
            "Durable cannot read table record version {version}"
        )));
    }
    Ok(serde_json::from_str(text)?)
}

pub(super) fn encode_indexed(text: &str) -> Result<String> {
    Ok(serde_json::to_string(text)?)
}

#[derive(Default)]
pub(super) struct Conditions(Vec<(&'static str, SqlValue)>);

impl Conditions {
    pub fn add(&mut self, clause: &'static str, value: impl Into<SqlValue>) {
        self.0.push((clause, value.into()));
    }

    pub fn after(&mut self, order: ScanOrder, after: Option<i64>) {
        if let Some(after) = after {
            self.add(
                match order {
                    ScanOrder::Ascending => "id>?",
                    ScanOrder::Descending => "id<?",
                },
                after,
            );
        }
    }

    fn sql(&self, projection: &str, table: &str, order: ScanOrder) -> String {
        let mut sql = format!("SELECT {projection} FROM \"{table}\"");
        for (i, (clause, _)) in self.0.iter().enumerate() {
            sql.push_str(if i == 0 { " WHERE " } else { " AND " });
            sql.push_str(clause);
        }
        sql.push_str(match order {
            ScanOrder::Ascending => " ORDER BY id ASC LIMIT ?",
            ScanOrder::Descending => " ORDER BY id DESC LIMIT ?",
        });
        sql
    }

    fn bind<'a>(&'a self, limit: &'a i64) -> impl Iterator<Item = &'a dyn rusqlite::ToSql> {
        self.0
            .iter()
            .map(|(_, value)| value as &dyn rusqlite::ToSql)
            .chain(std::iter::once(limit as &dyn rusqlite::ToSql))
    }
}
