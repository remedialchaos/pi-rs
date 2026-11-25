//! Checked table histories and resumable, version-stamped eager migrations.
use super::{
    AlterEdit, ColumnRecord, ColumnType, ExprNode, Query, SetNode, StoreEngine, StoreWrite,
    TableDefinition, TableRecord, TableState, WhereNode, applier,
    table::{check_order, column_name, index_name, invalid, safe, scalar},
};
use crate::{Result, context::Context};
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::Arc;
pub const MIGRATION_CHUNK: usize = 100;
pub type RowPatch = Arc<dyn Fn(&Value) -> Result<Value> + Send + Sync>;
#[derive(Clone)]
pub enum DataStep {
    Rows(RowPatch),
    Update {
        condition: Option<WhereNode>,
        set: SetNode,
    },
}
#[derive(Clone)]
pub struct TableHistory {
    pub definition: TableDefinition,
    previous: Option<Arc<Self>>,
    migration: Option<Migration>,
}
#[derive(Clone)]
struct Migration {
    before: Vec<Edit>,
    step: Option<(TableDefinition, DataStep)>,
    after: Vec<Edit>,
}
#[derive(Clone)]
enum Edit {
    Alter(AlterEdit),
    Index(String, Vec<String>),
    DropIndex(String),
}
pub struct TableEditor {
    current: TableDefinition,
    before: Vec<Edit>,
    step: Option<(TableDefinition, DataStep)>,
    after: Vec<Edit>,
    tail: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TablePlan {
    Create,
    Ready,
    Migrate { versions: Vec<i64>, resume: bool },
}
impl TableHistory {
    pub fn base(definition: TableDefinition) -> Self {
        Self {
            definition,
            previous: None,
            migration: None,
        }
    }
    pub fn versions(&self) -> Vec<&Self> {
        let mut out = vec![];
        let mut at = Some(self);
        while let Some(v) = at {
            out.push(v);
            at = v.previous.as_deref();
        }
        out.reverse();
        out
    }
    pub fn evolve(&self, edits: impl FnOnce(&mut TableEditor) -> Result<()>) -> Result<Self> {
        let mut current = self.definition.clone();
        current.version = current
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("table version overflow"))?;
        safe(current.version, 1)?;
        let mut editor = TableEditor {
            current,
            before: vec![],
            step: None,
            after: vec![],
            tail: false,
        };
        edits(&mut editor)?;
        let definition = editor.materialize()?;
        Ok(Self {
            definition,
            previous: Some(Arc::new(self.clone())),
            migration: Some(Migration {
                before: editor.before,
                step: editor.step,
                after: editor.after,
            }),
        })
    }
    pub fn plan(&self, record: Option<&TableRecord>) -> Result<TablePlan> {
        let Some(record) = record else {
            return Ok(TablePlan::Create);
        };
        if record.version > self.definition.version {
            return Err(invalid("recorded table is newer than code"));
        }
        let versions = self.versions();
        let at = versions
            .iter()
            .find(|h| h.definition.version == record.version)
            .ok_or_else(|| invalid("recorded table predates history base"))?;
        if at.definition.fingerprint != record.fingerprint {
            return Err(invalid("table history edited in place"));
        }
        if record.state == TableState::Ready && record.version == self.definition.version {
            return Ok(TablePlan::Ready);
        }
        let resume = record.state == TableState::Migrating;
        if resume && at.migration.as_ref().is_none_or(|m| m.step.is_none()) {
            return Err(invalid("migrating table has no resumable data step"));
        }
        Ok(TablePlan::Migrate {
            versions: versions
                .iter()
                .filter(|v| {
                    if resume {
                        v.definition.version >= record.version
                    } else {
                        v.definition.version > record.version
                    }
                })
                .map(|v| v.definition.version)
                .collect(),
            resume,
        })
    }
}
impl TableEditor {
    fn before(&self) -> Result<()> {
        if self.tail {
            Err(invalid("rename/add/json/data step must precede tail edits"))
        } else {
            Ok(())
        }
    }
    fn materialize(&self) -> Result<TableDefinition> {
        let mut result = TableDefinition::new(
            self.current.name.clone(),
            self.current.version,
            self.current.columns.clone(),
            self.current.key.clone(),
            self.current.indexes.clone(),
        )?;
        result.hosts = self.current.hosts.clone();
        result.json = self.current.json.clone();
        Ok(result)
    }
    fn fresh(&self, name: &str, except: Option<&str>) -> Result<()> {
        if !column_name(name)
            || self
                .current
                .columns
                .keys()
                .any(|n| Some(n.as_str()) != except && n.eq_ignore_ascii_case(name))
        {
            Err(invalid("column name is invalid or already exists"))
        } else {
            Ok(())
        }
    }
    pub fn rename(&mut self, from: &str, to: &str) -> Result<&mut Self> {
        self.before()?;
        self.fresh(to, Some(from))?;
        if !self.current.columns.contains_key(from) {
            return Err(invalid("rename source missing"));
        }
        self.current.columns = self
            .current
            .columns
            .clone()
            .into_iter()
            .map(|(n, c)| (if n == from { to.into() } else { n }, c))
            .collect();
        for n in &mut self.current.key {
            if n == from {
                *n = to.into();
            }
        }
        for on in self.current.indexes.values_mut() {
            for n in on {
                if n == from {
                    *n = to.into();
                }
            }
        }
        if let Some(host) = self.current.hosts.shift_remove(from) {
            self.current.hosts.insert(to.into(), host);
        }
        if let Some(json) = self.current.json.shift_remove(from) {
            self.current.json.insert(to.into(), json);
        }
        self.before.push(Edit::Alter(AlterEdit::Rename {
            from: from.into(),
            to: to.into(),
        }));
        Ok(self)
    }
    pub fn add(&mut self, name: &str, column: ColumnRecord) -> Result<&mut Self> {
        self.before()?;
        self.fresh(name, None)?;
        column.check()?;
        if !column.optional && column.default.is_none() {
            return Err(invalid("added column needs optional or default"));
        }
        self.current.columns.insert(name.into(), column.clone());
        self.before.push(Edit::Alter(AlterEdit::Add {
            name: name.into(),
            column,
        }));
        Ok(self)
    }
    pub fn json(&mut self, name: &str, mut column: ColumnRecord) -> Result<&mut Self> {
        self.before()?;
        column.check()?;
        let old = self
            .current
            .columns
            .get(name)
            .ok_or_else(|| invalid("json source missing"))?;
        if old.kind != ColumnType::Json || column.kind != ColumnType::Json || column.optional {
            return Err(invalid("json edit needs bare JSON column"));
        }
        if column.version < old.version
            || (column.version == old.version && column.fingerprint != old.fingerprint)
        {
            return Err(invalid("JSON history goes back or edits same version"));
        }
        column.optional = old.optional;
        self.current.json.shift_remove(name);
        self.current.columns.insert(name.into(), column.clone());
        self.before.push(Edit::Alter(AlterEdit::Json {
            name: name.into(),
            column,
        }));
        Ok(self)
    }
    pub fn json_schema(&mut self, name: &str, json: super::JsonColumn) -> Result<&mut Self> {
        self.json(name, json.record())?;
        self.current.json.insert(name.into(), json);
        Ok(self)
    }
    pub fn rows(
        &mut self,
        patch: impl Fn(&Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Result<&mut Self> {
        self.before()?;
        self.step = Some((self.materialize()?, DataStep::Rows(Arc::new(patch))));
        self.tail = true;
        Ok(self)
    }
    pub fn update(&mut self, condition: Option<WhereNode>, set: SetNode) -> Result<&mut Self> {
        self.before()?;
        let definition = self.materialize()?;
        if let Some(c) = &condition {
            super::expression::where_sql(c, &definition.columns, &mut vec![])?;
        }
        if !set.is_empty() {
            super::expression::set_sql(
                &set,
                &definition.columns,
                &definition.key,
                &IndexMap::new(),
                &mut vec![],
            )?;
        }
        self.step = Some((definition, DataStep::Update { condition, set }));
        self.tail = true;
        Ok(self)
    }
    pub fn drop_column(&mut self, name: &str) -> Result<&mut Self> {
        self.tail = true;
        if self.current.key.contains(&name.to_owned()) {
            return Err(invalid("cannot drop key"));
        }
        if self.current.columns.shift_remove(name).is_none() {
            return Err(invalid("drop source missing"));
        }
        let indexes = self
            .current
            .indexes
            .iter()
            .filter(|(_, on)| on.contains(&name.to_owned()))
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>();
        for n in indexes {
            self.drop_index(&n)?;
        }
        self.current.hosts.shift_remove(name);
        self.current.json.shift_remove(name);
        self.after
            .push(Edit::Alter(AlterEdit::Drop { name: name.into() }));
        Ok(self)
    }
    pub fn index(&mut self, name: &str, columns: Vec<String>) -> Result<&mut Self> {
        self.tail = true;
        if !index_name(name) || self.current.indexes.contains_key(name) {
            return Err(invalid("index is invalid or already exists"));
        }
        check_order(&self.current.columns, &columns)?;
        self.current.indexes.insert(name.into(), columns.clone());
        self.after.push(Edit::Index(name.into(), columns));
        Ok(self)
    }
    pub fn drop_index(&mut self, name: &str) -> Result<&mut Self> {
        self.tail = true;
        if self.current.indexes.shift_remove(name).is_none() {
            return Err(invalid("index missing"));
        }
        self.after.push(Edit::DropIndex(name.into()));
        Ok(self)
    }
}
fn edits(table: &str, version: i64, edits: &[Edit]) -> Vec<StoreWrite> {
    edits
        .iter()
        .map(|edit| match edit {
            Edit::Alter(edit) => StoreWrite::Alter {
                table: table.into(),
                version,
                edit: edit.clone(),
            },
            Edit::Index(name, columns) => StoreWrite::Index {
                table: table.into(),
                name: name.clone(),
                columns: columns.clone(),
            },
            Edit::DropIndex(name) => StoreWrite::DropIndex {
                table: table.into(),
                name: name.clone(),
            },
        })
        .collect()
}
fn record(
    table: &str,
    columns: &TableDefinition,
    final_definition: &TableDefinition,
    state: TableState,
) -> StoreWrite {
    StoreWrite::Table {
        table: table.into(),
        version: final_definition.version,
        state,
        columns: columns.columns.clone(),
        key: columns.key.clone(),
        fingerprint: final_definition.fingerprint.clone(),
    }
}
fn json_versions(set: &SetNode, table: &TableDefinition) -> IndexMap<String, i64> {
    set.iter()
        .filter_map(|(n, e)| {
            table
                .columns
                .get(n)
                .filter(|c| c.kind == ColumnType::Json && !e.0.is_null())
                .and_then(|c| c.version.map(|v| (n.clone(), v)))
        })
        .collect()
}
fn patch(row: &Value, table: &TableDefinition, run: &RowPatch) -> Result<SetNode> {
    let result = run(row)?;
    let result = result
        .as_object()
        .ok_or_else(|| invalid("rows patch must be an object"))?;
    let mut set = SetNode::new();
    for (n, v) in result {
        let c = table
            .columns
            .get(n)
            .ok_or_else(|| invalid("patch names absent column"))?;
        if table.key.contains(n) {
            return Err(invalid("patch cannot change key"));
        }
        let e = if v.is_null() && c.optional {
            Value::Null
        } else if c.kind == ColumnType::Json {
            serde_json::json!({"json":v})
        } else {
            scalar(c.kind, v)?;
            v.clone()
        };
        set.insert(n.clone(), ExprNode(e));
    }
    Ok(set)
}
impl StoreEngine {
    /// Validate every history before admitting any DDL. Each admitted commit completes even if its waiter is cancelled.
    pub async fn prepare_tables(
        &self,
        owner: &str,
        tables: &[TableHistory],
        context: &Context,
    ) -> Result<()> {
        crate::contracts::check_name(owner, "owner")?;
        let catalog = self.database().call(applier::catalog).await?;
        let mut planned = vec![];
        let mut created = vec![];
        for history in tables {
            let table = format!("{owner}:{}", history.definition.name);
            let plan = history.plan(catalog.get(&table))?;
            match plan {
                TablePlan::Create => created.extend(history.definition.declaration(owner)?),
                TablePlan::Ready => {}
                TablePlan::Migrate { versions, resume } => {
                    planned.push((table, history, versions, resume))
                }
            }
        }
        context.check().map_err(|e| invalid(e.to_string()))?;
        if !created.is_empty() {
            self.commit(created).await?;
        }
        for (table, history, versions, resume) in planned {
            for (i, version) in versions.iter().enumerate() {
                context.check().map_err(|e| invalid(e.to_string()))?;
                let at = history
                    .versions()
                    .into_iter()
                    .find(|h| h.definition.version == *version)
                    .ok_or_else(|| invalid("missing history version"))?;
                let migration = at
                    .migration
                    .as_ref()
                    .ok_or_else(|| invalid("missing table migration"))?;
                let definition = &at.definition;
                let mut ready = edits(&table, *version, &migration.after);
                ready.push(record(&table, definition, definition, TableState::Ready));
                let Some((during, step)) = &migration.step else {
                    let mut all = edits(&table, *version, &migration.before);
                    all.extend(ready);
                    self.commit(all).await?;
                    continue;
                };
                if !(i == 0 && resume) {
                    let mut before = edits(&table, *version, &migration.before);
                    before.push(record(&table, during, definition, TableState::Migrating));
                    self.commit(before).await?;
                }
                match step {
                    DataStep::Update { condition, set } => {
                        let below = WhereNode::Lt(("$v".into(), Value::from(*version)));
                        let condition = condition
                            .clone()
                            .map_or(below.clone(), |c| WhereNode::And(vec![c, below]));
                        let mut writes = vec![StoreWrite::UpdateWhere {
                            table: table.clone(),
                            r#where: condition,
                            set: set.clone(),
                            columns: json_versions(set, during),
                            count: None,
                            version: Some(*version),
                        }];
                        writes.extend(ready);
                        self.commit(writes).await?;
                    }
                    DataStep::Rows(run) => loop {
                        context.check().map_err(|e| invalid(e.to_string()))?;
                        let query = Query::new(owner, during.clone())?
                            .filter(WhereNode::Lt(("$v".into(), Value::from(*version))))?
                            .with_limit(MIGRATION_CHUNK + 1)?;
                        // The migration's own definition is intentionally readable while its record is migrating.
                        let page = self
                            .database()
                            .call(move |c| super::query::read_page(c, &query))
                            .await?;
                        let last = page.rows.len() <= MIGRATION_CHUNK;
                        let mut writes = vec![];
                        for row in page.rows.iter().take(MIGRATION_CHUNK) {
                            let set = patch(row, during, run)?;
                            let key = during
                                .key
                                .iter()
                                .map(|n| {
                                    Ok((
                                        n.clone(),
                                        row.get(n)
                                            .cloned()
                                            .ok_or_else(|| invalid("missing migration key"))?,
                                    ))
                                })
                                .collect::<Result<IndexMap<_, _>>>()?;
                            writes.push(StoreWrite::Update {
                                table: table.clone(),
                                key,
                                columns: json_versions(&set, during),
                                set,
                                count: None,
                                version: Some(*version),
                            });
                        }
                        if last {
                            writes.extend(ready.clone());
                        }
                        self.commit(writes).await?;
                        if last {
                            break;
                        }
                        tokio::task::yield_now().await;
                    },
                }
            }
        }
        Ok(())
    }
}
