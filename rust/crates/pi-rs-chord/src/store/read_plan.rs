//! Checked cross-table read plans. SQL identifiers come only from admitted bindings.
use super::{
    ColumnRecord, ColumnType, Query, Statement, TableDefinition, WhereNode,
    expression::{ExistsNode, render_where},
    table::{SAFE, invalid, quote, scalar},
};
use crate::Result;
use indexmap::{IndexMap, IndexSet};
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use std::sync::{Arc, Mutex};
#[derive(Clone, Debug)]
pub struct Bound {
    pub table: String,
    pub definition: TableDefinition,
}
#[derive(Clone, Debug)]
pub struct ColumnSet {
    pub name: String,
    pub columns: IndexMap<String, ColumnRecord>,
    pub key: Vec<String>,
    pub indexes: Vec<Vec<String>>,
}
impl ColumnSet {
    pub fn new(
        name: &str,
        columns: IndexMap<String, ColumnRecord>,
        key: Vec<String>,
        indexes: Vec<Vec<String>>,
    ) -> Result<Self> {
        crate::contracts::check_name(name, "column set")?;
        // A set's exposed schema obeys the same key and index rules as a table.
        TableDefinition::new(
            name,
            1,
            columns.clone(),
            key.clone(),
            indexes
                .iter()
                .enumerate()
                .map(|(i, on)| (format!("i{i}"), on.clone()))
                .collect(),
        )?;
        Ok(Self {
            name: name.into(),
            columns,
            key,
            indexes,
        })
    }
}
#[derive(Clone, Debug, Default)]
pub struct Bindings(IndexMap<String, Bound>);
impl Bindings {
    pub fn bind(&mut self, set: &ColumnSet, bound: Bound) -> Result<()> {
        for (name, column) in &set.columns {
            if bound.definition.columns.get(name) != Some(column) {
                return Err(invalid(format!(
                    "column set {}: incompatible {name}",
                    set.name
                )));
            }
        }
        if set.key != bound.definition.key {
            return Err(invalid("column set key differs from served table"));
        }
        self.0.insert(set.name.clone(), bound);
        Ok(())
    }
    pub fn unbind(&mut self, set: &str) {
        self.0.shift_remove(set);
    }
    pub fn get(&self, set: &str) -> Result<&Bound> {
        self.0.get(set).ok_or_else(|| {
            crate::data::ChordError::new("unavailable", format!("column set {set} is not served"))
                .into()
        })
    }
}
#[derive(Clone, Debug)]
pub struct Joined {
    pub set: ColumnSet,
    pub pairs: Vec<(String, String)>,
}
#[derive(Clone, Debug, Default)]
pub struct Projection {
    pub reads: Option<Vec<String>>,
    pub partner_reads: Option<Vec<String>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRole {
    Own,
    Joined,
    Exists,
}
#[derive(Clone, Debug)]
pub struct TableUse {
    pub role: ReadRole,
    pub table: String,
    pub depends: IndexSet<String>,
    pub enters: IndexSet<String>,
    pub paired_by: Vec<String>,
    pub pairs: Option<Vec<String>>,
    pub keyed: bool,
    pub stable: bool,
}
#[derive(Clone, Debug, Default)]
pub struct ReadRange {
    pub after: Option<Vec<SqlValue>>,
    pub through: Option<Vec<SqlValue>>,
    pub limit: usize,
}
pub type StoredRead = IndexMap<String, SqlValue>;
#[derive(Clone)]
pub struct ReadPlan {
    pub query: Query,
    pub uses: Vec<TableUse>,
    pub bindings: Bindings,
    pub joined: Option<Joined>,
    select: String,
    from: String,
    condition: Option<String>,
    params: Vec<SqlValue>,
    schema: IndexMap<String, ColumnRecord>,
    texts: Arc<Mutex<IndexMap<String, String>>>,
}
fn column_sql(name: &str, joined: Option<&Joined>) -> String {
    if let Some(joined) = joined
        && let Some(n) = name.strip_prefix(&format!("{}:", joined.set.name))
    {
        return format!("p.{}", quote(n));
    }
    format!("t.{}", quote(name))
}
fn used_columns(node: &WhereNode, into: &mut IndexSet<String>) {
    match node {
        WhereNode::And(nodes) | WhereNode::Or(nodes) => {
            for node in nodes {
                used_columns(node, into)
            }
        }
        WhereNode::Not(node) => used_columns(node, into),
        WhereNode::Exists((_, pairs, _)) => into.extend(pairs.iter().map(|(_, n)| n.clone())),
        WhereNode::Eq((n, _))
        | WhereNode::Ne((n, _))
        | WhereNode::Lt((n, _))
        | WhereNode::Lte((n, _))
        | WhereNode::Gt((n, _))
        | WhereNode::Gte((n, _))
        | WhereNode::InArray((n, _))
        | WhereNode::Prefix((n, _))
        | WhereNode::IsNull(n) => {
            into.insert(n.clone());
        }
    }
}
fn fixed_columns(node: &WhereNode, into: &mut IndexSet<String>) {
    match node {
        WhereNode::And(nodes) => {
            for node in nodes {
                fixed_columns(node, into)
            }
        }
        WhereNode::Eq((n, _)) | WhereNode::InArray((n, _)) => {
            into.insert(n.clone());
        }
        _ => {}
    }
}
fn keyed(table: &TableDefinition, fixed: &IndexSet<String>, columns: &[String]) -> bool {
    std::iter::once(table.key.clone())
        .chain(table.indexes.values().map(|on| {
            let mut path = on.clone();
            path.extend(table.key.iter().filter(|n| !on.contains(n)).cloned());
            path
        }))
        .any(|path| {
            let mut covered = 0;
            for name in path {
                if columns.contains(&name) {
                    covered += 1;
                } else if !fixed.contains(&name) {
                    break;
                }
                if covered == columns.len() {
                    return true;
                }
            }
            false
        })
}
fn check_pairs(
    pairs: &[(String, String)],
    inner: &TableDefinition,
    outer: &IndexMap<String, ColumnRecord>,
    whole_key: bool,
) -> Result<()> {
    let names = pairs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    let distinct = names.iter().collect::<IndexSet<_>>();
    if names.is_empty() || distinct.len() != names.len() {
        return Err(invalid("match needs distinct columns"));
    }
    if whole_key {
        if names.len() != inner.key.len() || !inner.key.iter().all(|n| names.contains(n)) {
            return Err(invalid("join must match the whole partner key"));
        }
    } else if !std::iter::once(&inner.key)
        .chain(inner.indexes.values())
        .any(|on| names.len() <= on.len() && on.iter().take(names.len()).all(|n| names.contains(n)))
    {
        return Err(invalid("exists match must lead a key or index"));
    }
    for (name, enclosing) in pairs {
        let a = inner
            .columns
            .get(name)
            .ok_or_else(|| invalid("missing matched column"))?;
        let b = outer
            .get(enclosing)
            .ok_or_else(|| invalid("missing enclosing column"))?;
        if a.kind == ColumnType::Json || a.kind != b.kind || a.host != b.host {
            return Err(invalid("matched column types differ"));
        }
    }
    Ok(())
}
fn render_condition(
    node: &WhereNode,
    schema: &IndexMap<String, ColumnRecord>,
    column: &dyn Fn(&str) -> String,
    bindings: &Bindings,
    params: &mut Vec<SqlValue>,
    uses: &mut Vec<TableUse>,
    depth: usize,
) -> Result<String> {
    if depth > 128 {
        return Err(invalid("exists nesting exceeds 128"));
    }
    let checked = |n: &str| -> Result<String> {
        let col = schema
            .get(n)
            .ok_or_else(|| invalid(format!("unknown scalar column {n}")))?;
        if col.kind == ColumnType::Json {
            return Err(invalid("json column cannot be a condition operand"));
        }
        Ok(column(n))
    };
    render_where(
        node,
        params,
        &checked,
        &mut |(set, pairs, condition): &ExistsNode, params| {
            let bound = bindings.get(set)?;
            check_pairs(pairs, &bound.definition, schema, false)?;
            let alias = format!("e{depth}");
            let inner_col = |name: &str| format!("{alias}.{}", quote(name));
            let mut depends = pairs
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<IndexSet<_>>();
            if let Some(condition) = condition {
                used_columns(condition, &mut depends);
            }
            uses.push(TableUse {
                role: ReadRole::Exists,
                table: bound.table.clone(),
                depends: depends.clone(),
                enters: depends,
                paired_by: pairs.iter().map(|(n, _)| n.clone()).collect(),
                pairs: (depth == 0).then(|| pairs.iter().map(|(_, n)| n.clone()).collect()),
                keyed: false,
                stable: pairs.iter().all(|(n, _)| bound.definition.key.contains(n)),
            });
            let mut matched = pairs
                .iter()
                .map(|(n, enclosing)| format!("{} = {}", inner_col(n), column(enclosing)))
                .collect::<Vec<_>>();
            if let Some(condition) = condition {
                matched.push(render_condition(
                    condition,
                    &bound.definition.columns,
                    &inner_col,
                    bindings,
                    params,
                    uses,
                    depth + 1,
                )?);
            }
            Ok(format!(
                "EXISTS (SELECT 1 FROM {} AS {alias} WHERE {})",
                quote(&bound.table),
                matched.join(" AND ")
            ))
        },
    )
}
impl ReadPlan {
    pub fn new(
        query: Query,
        joined: Option<Joined>,
        projection: Projection,
        counted: bool,
        bindings: Bindings,
    ) -> Result<Self> {
        let mut schema = query.definition.columns.clone();
        let mut from = format!("{} AS t", quote(&query.table));
        if let Some(joined) = &joined {
            let bound = bindings.get(&joined.set.name)?;
            check_pairs(&joined.pairs, &bound.definition, &schema, true)?;
            for (n, c) in &joined.set.columns {
                if bound.definition.columns.get(n) != Some(c) {
                    return Err(invalid("joined column declaration changed"));
                }
                schema.insert(format!("{}:{n}", joined.set.name), c.clone());
            }
            let on = joined
                .pairs
                .iter()
                .map(|(n, enclosing)| format!("p.{} = t.{}", quote(n), quote(enclosing)))
                .collect::<Vec<_>>()
                .join(" AND ");
            from.push_str(&format!(" CROSS JOIN {} AS p ON {on}", quote(&bound.table)));
        }
        let mut used = IndexSet::new();
        let mut fixed = IndexSet::new();
        if let Some(condition) = &query.condition {
            used_columns(condition, &mut used);
            fixed_columns(condition, &mut fixed);
        }
        if !counted {
            used.extend(query.order_columns());
            for n in query.order_columns() {
                let col = schema
                    .get(&n)
                    .ok_or_else(|| invalid("missing order column"))?;
                if col.optional || !matches!(col.kind, ColumnType::Str | ColumnType::Int) {
                    return Err(invalid("order needs nonoptional str or int"));
                }
            }
        }
        let reads = if counted {
            vec![]
        } else {
            projection
                .reads
                .unwrap_or_else(|| query.definition.columns.keys().cloned().collect())
        };
        let partner_reads = if counted {
            vec![]
        } else {
            projection.partner_reads.unwrap_or_else(|| {
                joined
                    .as_ref()
                    .map_or_else(Vec::new, |j| j.set.columns.keys().cloned().collect())
            })
        };
        let prefix = joined.as_ref().map(|j| format!("{}:", j.set.name));
        let own = |n: &&String| prefix.as_ref().is_none_or(|p| !n.starts_with(p));
        let mut enters = used.iter().filter(own).cloned().collect::<IndexSet<_>>();
        if let Some(j) = &joined {
            enters.extend(j.pairs.iter().map(|(_, n)| n.clone()));
        }
        let mut depends = enters.clone();
        depends.extend(reads.clone());
        let mut uses = vec![TableUse {
            role: ReadRole::Own,
            table: query.table.clone(),
            depends,
            enters,
            paired_by: query.definition.key.clone(),
            pairs: Some(query.definition.key.clone()),
            keyed: true,
            stable: true,
        }];
        if let Some(j) = &joined {
            let bound = bindings.get(&j.set.name)?;
            let enters = used
                .iter()
                .filter_map(|n| {
                    n.strip_prefix(&format!("{}:", j.set.name))
                        .map(String::from)
                })
                .collect::<IndexSet<_>>();
            let mut depends = enters.clone();
            depends.extend(partner_reads.clone());
            depends.extend(j.set.key.clone());
            let pairs = j.pairs.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>();
            uses.push(TableUse {
                role: ReadRole::Joined,
                table: bound.table.clone(),
                depends,
                enters,
                paired_by: j.set.key.clone(),
                keyed: keyed(&query.definition, &fixed, &pairs),
                pairs: Some(pairs),
                stable: true,
            });
        }
        let mut params = vec![];
        let condition = query
            .condition
            .as_ref()
            .map(|node| {
                render_condition(
                    node,
                    &schema,
                    &|n| column_sql(n, joined.as_ref()),
                    &bindings,
                    &mut params,
                    &mut uses,
                    0,
                )
            })
            .transpose()?;
        for usage in uses.iter_mut().filter(|u| u.role == ReadRole::Exists) {
            if let Some(pairs) = &usage.pairs {
                let own_pairs = pairs
                    .iter()
                    .map(|n| {
                        if let Some(j) = &joined
                            && let Some(n) = n.strip_prefix(&format!("{}:", j.set.name))
                        {
                            j.pairs
                                .iter()
                                .find(|(key, _)| key == n)
                                .map(|(_, n)| n.clone())
                        } else {
                            Some(n.clone())
                        }
                    })
                    .collect::<Option<Vec<_>>>();
                usage.keyed = own_pairs
                    .as_ref()
                    .is_some_and(|pairs| keyed(&query.definition, &fixed, pairs));
                usage.pairs = own_pairs;
            }
        }
        let mut selected = reads.iter().cloned().collect::<IndexSet<_>>();
        selected.extend(query.definition.key.clone());
        if !counted {
            selected.extend(query.order_columns());
        }
        for u in &uses {
            if let Some(pairs) = &u.pairs {
                selected.extend(pairs.clone());
            }
        }
        for n in &reads {
            if query
                .definition
                .columns
                .get(n)
                .is_some_and(|c| c.kind == ColumnType::Json)
            {
                selected.insert(format!("{n}$v"));
            }
        }
        if let Some(j) = &joined {
            for n in &partner_reads {
                selected.insert(format!("{}:{n}", j.set.name));
                if j.set
                    .columns
                    .get(n)
                    .is_some_and(|c| c.kind == ColumnType::Json)
                {
                    selected.insert(format!("{}:{n}$v", j.set.name));
                }
            }
        }
        for n in &selected {
            let base = n.strip_suffix("$v").unwrap_or(n);
            if !schema.contains_key(base) {
                return Err(invalid(format!("unknown projection column {n}")));
            }
        }
        let select = selected
            .iter()
            .map(|n| format!("{} AS {}", column_sql(n, joined.as_ref()), quote(n)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Self {
            query,
            uses,
            bindings,
            joined,
            select,
            from,
            condition,
            params,
            schema,
            texts: Arc::new(Mutex::new(IndexMap::new())),
        })
    }
    fn bound(&self, range: &ReadRange) -> Result<(String, Vec<SqlValue>)> {
        if range.limit as u128 > SAFE as u128 {
            return Err(invalid("range limit is not safe"));
        }
        let mut params = self.params.clone();
        let mut conditions = self.condition.iter().cloned().collect::<Vec<_>>();
        let order = self.query.order_columns();
        for (position, through) in [(&range.after, false), (&range.through, true)] {
            if let Some(position) = position {
                if position.is_empty() || position.len() > order.len() {
                    return Err(invalid("invalid range position length"));
                }
                for (n, value) in order.iter().zip(position) {
                    let c = self
                        .schema
                        .get(n)
                        .ok_or_else(|| invalid("invalid range column"))?;
                    let json = match value {
                        SqlValue::Text(s) => serde_json::Value::String(s.clone()),
                        SqlValue::Integer(n) => serde_json::Value::from(*n),
                        _ => return Err(invalid("range position is not an order scalar")),
                    };
                    scalar(c.kind, &json)?;
                }
                let columns = order
                    .iter()
                    .take(position.len())
                    .map(|n| column_sql(n, self.joined.as_ref()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let op = match (self.query.descending, through) {
                    (false, false) => ">",
                    (false, true) => "<=",
                    (true, false) => "<",
                    (true, true) => ">=",
                };
                conditions.push(format!(
                    "({columns}) {op} ({})",
                    vec!["?"; position.len()].join(", ")
                ));
                params.extend(position.clone());
            }
        }
        Ok((
            if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.join(" AND "))
            },
            params,
        ))
    }
    fn cached(&self, key: String, render: impl FnOnce() -> String) -> String {
        let mut texts = self.texts.lock().unwrap_or_else(|e| e.into_inner());
        texts.entry(key).or_insert_with(render).clone()
    }
    pub fn select(&self, range: &ReadRange) -> Result<Statement> {
        let (filter, mut params) = self.bound(range)?;
        let key = format!(
            "select|{:?}|{:?}",
            range.after.as_ref().map(Vec::len),
            range.through.as_ref().map(Vec::len)
        );
        let sql = self.cached(key, || {
            format!(
                "SELECT {} FROM {}{filter} ORDER BY {} LIMIT ?",
                self.select,
                self.from,
                self.query
                    .order_columns()
                    .iter()
                    .map(|n| format!(
                        "{} {}",
                        column_sql(n, self.joined.as_ref()),
                        if self.query.descending { "DESC" } else { "ASC" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        params.push(SqlValue::Integer(if range.limit == 0 {
            -1
        } else {
            range.limit as i64
        }));
        Ok(Statement { sql, params })
    }
    pub fn count(&self) -> Result<Statement> {
        let (filter, params) = self.bound(&ReadRange::default())?;
        Ok(Statement {
            sql: self.cached("count".into(), || {
                format!("SELECT count(*) AS count FROM {}{filter}", self.from)
            }),
            params,
        })
    }
    pub fn probe(
        &self,
        range: &ReadRange,
        columns: &[String],
        values: &[SqlValue],
    ) -> Result<Statement> {
        if columns.is_empty() || columns.len() != values.len() {
            return Err(invalid("probe columns and values differ"));
        }
        for n in columns {
            if !self.query.definition.columns.contains_key(n) {
                return Err(invalid("probe needs own columns"));
            }
        }
        let (filter, mut params) = self.bound(range)?;
        let key = format!(
            "probe|{:?}|{:?}|{columns:?}",
            range.after.as_ref().map(Vec::len),
            range.through.as_ref().map(Vec::len)
        );
        let sql = self.cached(key, || {
            format!(
                "SELECT 1 AS found FROM {}{filter}{}{} LIMIT 1",
                self.from,
                if filter.is_empty() {
                    " WHERE "
                } else {
                    " AND "
                },
                columns
                    .iter()
                    .map(|n| format!("t.{} = ?", quote(n)))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            )
        });
        params.extend_from_slice(values);
        Ok(Statement { sql, params })
    }
    pub fn check(&self, c: &Connection) -> Result<()> {
        let catalog = super::applier::catalog(c)?;
        for bound in std::iter::once(Bound {
            table: self.query.table.clone(),
            definition: self.query.definition.clone(),
        })
        .chain(self.bindings.0.values().cloned())
        {
            let recorded = catalog
                .get(&bound.table)
                .ok_or_else(|| invalid("read table absent"))?;
            if recorded.state != super::TableState::Ready
                || recorded.version != bound.definition.version
                || recorded.fingerprint != bound.definition.fingerprint
            {
                return Err(invalid("read table is held or its declaration changed"));
            }
        }
        Ok(())
    }
    pub fn read(&self, c: &Connection, range: &ReadRange) -> Result<Vec<StoredRead>> {
        self.check(c)?;
        let sql = self.select(range)?;
        let mut statement = c.prepare_cached(&sql.sql)?;
        let names = statement
            .column_names()
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>();
        Ok(statement
            .query_map(params_from_iter(sql.params), |row| {
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| Ok((n.clone(), row.get(i)?)))
                    .collect::<rusqlite::Result<StoredRead>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
