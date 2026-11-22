//! Checked expression vocabulary compiled to bound SQLite parameters.
use super::table::{ColumnRecord, ColumnType, invalid, quote, scalar};
use crate::Result;
use indexmap::IndexMap;
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhereNode {
    Eq((String, Value)),
    Ne((String, Value)),
    Lt((String, Value)),
    Lte((String, Value)),
    Gt((String, Value)),
    Gte((String, Value)),
    InArray((String, Vec<Value>)),
    IsNull(String),
    Prefix((String, String)),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    Exists((String, Vec<(String, String)>, Option<Box<Self>>)),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExprNode(pub Value);
pub type SetNode = IndexMap<String, ExprNode>;
#[derive(Clone, Debug)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<SqlValue>,
}
fn scalar_value(v: &Value) -> Result<SqlValue> {
    match v {
        Value::String(_) => scalar(ColumnType::Str, v),
        Value::Bool(_) => scalar(ColumnType::Bool, v),
        Value::Number(_) => scalar(ColumnType::Num, v),
        _ => Err(invalid("expression requires scalar value")),
    }
}
fn ref_sql(columns: &IndexMap<String, ColumnRecord>, n: &str, scalar_only: bool) -> Result<String> {
    if n == "$v" {
        return Ok(quote(n));
    }
    if columns
        .get(n)
        .is_none_or(|c| scalar_only && c.kind == ColumnType::Json)
    {
        return Err(invalid(format!("unknown scalar column {n}")));
    }
    Ok(quote(n))
}
pub type ExistsNode = (String, Vec<(String, String)>, Option<Box<WhereNode>>);
pub fn where_sql(
    node: &WhereNode,
    columns: &IndexMap<String, ColumnRecord>,
    params: &mut Vec<SqlValue>,
) -> Result<String> {
    render_where(node, params, &|n| ref_sql(columns, n, true), &mut |_, _| {
        Err(invalid("exists requires a resolved read plan"))
    })
}
pub(crate) fn render_where(
    node: &WhereNode,
    params: &mut Vec<SqlValue>,
    column: &dyn Fn(&str) -> Result<String>,
    exists: &mut dyn FnMut(&ExistsNode, &mut Vec<SqlValue>) -> Result<String>,
) -> Result<String> {
    Ok(match node {
        WhereNode::And(nodes) | WhereNode::Or(nodes) => {
            let and = matches!(node, WhereNode::And(_));
            if nodes.is_empty() {
                if and { "1" } else { "0" }.into()
            } else {
                format!(
                    "({})",
                    nodes
                        .iter()
                        .map(|n| render_where(n, params, column, exists))
                        .collect::<Result<Vec<_>>>()?
                        .join(if and { " AND " } else { " OR " })
                )
            }
        }
        WhereNode::Not(n) => format!("(NOT {})", render_where(n, params, column, exists)?),
        WhereNode::IsNull(n) => format!("{} IS NULL", column(n)?),
        WhereNode::InArray((n, values)) => {
            for v in values {
                scalar_value(v)?;
            }
            params.push(SqlValue::Text(serde_json::to_string(
                &values
                    .iter()
                    .map(|v| {
                        if let Some(b) = v.as_bool() {
                            Value::from(i64::from(b))
                        } else {
                            v.clone()
                        }
                    })
                    .collect::<Vec<_>>(),
            )?));
            format!("{} IN (SELECT value FROM json_each(?))", column(n)?)
        }
        WhereNode::Prefix((n, prefix)) => {
            let col = column(n)?;
            params.push(scalar(ColumnType::Str, &Value::String(prefix.clone()))?);
            if let Some(upper) = prefix_upper_bound(prefix) {
                params.push(SqlValue::Text(upper));
                format!("({col} >= ? AND {col} < ?)")
            } else {
                format!("{col} >= ?")
            }
        }
        WhereNode::Exists(node) => exists(node, params)?,
        WhereNode::Eq((n, v))
        | WhereNode::Ne((n, v))
        | WhereNode::Lt((n, v))
        | WhereNode::Lte((n, v))
        | WhereNode::Gt((n, v))
        | WhereNode::Gte((n, v)) => {
            let op = match node {
                WhereNode::Eq(_) => "=",
                WhereNode::Ne(_) => "<>",
                WhereNode::Lt(_) => "<",
                WhereNode::Lte(_) => "<=",
                WhereNode::Gt(_) => ">",
                _ => ">=",
            };
            params.push(scalar_value(v)?);
            format!("{} {op} ?", column(n)?)
        }
    })
}
pub fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut chars = prefix.chars().collect::<Vec<_>>();
    while let Some(c) = chars.pop() {
        let n = c as u32;
        if n < 0x10ffff {
            let next = if n == 0xd7ff { 0xe000 } else { n + 1 };
            if let Some(c) = char::from_u32(next) {
                chars.push(c);
                return Some(chars.into_iter().collect());
            }
        }
    }
    None
}
pub fn expr_sql(
    node: &ExprNode,
    columns: &IndexMap<String, ColumnRecord>,
    params: &mut Vec<SqlValue>,
) -> Result<String> {
    expr(&node.0, columns, params, 0)
}
fn expr(
    node: &Value,
    columns: &IndexMap<String, ColumnRecord>,
    params: &mut Vec<SqlValue>,
    depth: usize,
) -> Result<String> {
    if depth > 128 {
        return Err(invalid("expression nesting exceeds 128"));
    }
    if node.is_null() {
        return Ok("NULL".into());
    }
    if !node.is_object() {
        params.push(scalar_value(node)?);
        return Ok("?".into());
    }
    let map = node
        .as_object()
        .ok_or_else(|| invalid("malformed expression"))?;
    if map.len() != 1 {
        return Err(invalid("expression has one operation"));
    }
    let (op, operand) = map
        .iter()
        .next()
        .ok_or_else(|| invalid("empty expression"))?;
    if op == "col" {
        return ref_sql(
            columns,
            operand
                .as_str()
                .ok_or_else(|| invalid("column name is text"))?,
            false,
        );
    }
    if op == "json" {
        params.push(SqlValue::Text(serde_json::to_string(operand)?));
        return Ok("?".into());
    }
    let args = operand
        .as_array()
        .ok_or_else(|| invalid("operation takes tuple"))?;
    if op == "when" && args.len() == 3 {
        let cond: WhereNode = serde_json::from_value(args[0].clone())?;
        let cond = where_sql(&cond, columns, params)?;
        return Ok(format!(
            "(CASE WHEN {cond} THEN {} ELSE {} END)",
            expr(&args[1], columns, params, depth + 1)?,
            expr(&args[2], columns, params, depth + 1)?
        ));
    }
    if op == "replacePrefix" && args.len() == 3 {
        let from = args[1].as_str().ok_or_else(|| invalid("prefix is text"))?;
        let to = args[2]
            .as_str()
            .ok_or_else(|| invalid("replacement is text"))?;
        let len = from.chars().count() as i64;
        let a = expr(&args[0], columns, params, depth + 1)?;
        params.extend([
            SqlValue::Integer(len),
            SqlValue::Text(from.into()),
            SqlValue::Text(to.into()),
        ]);
        let b = expr(&args[0], columns, params, depth + 1)?;
        params.push(SqlValue::Integer(len + 1));
        let c = expr(&args[0], columns, params, depth + 1)?;
        return Ok(format!(
            "(CASE WHEN substr({a}, 1, ?) = ? THEN ? || substr({b}, ?) ELSE {c} END)"
        ));
    }
    if args.len() != 2
        || !matches!(
            op.as_str(),
            "add" | "sub" | "max" | "min" | "concat" | "coalesce"
        )
    {
        return Err(invalid("malformed expression operation"));
    }
    let a = expr(&args[0], columns, params, depth + 1)?;
    let b = expr(&args[1], columns, params, depth + 1)?;
    Ok(match op.as_str() {
        "add" => format!("({a} + {b})"),
        "sub" => format!("({a} - {b})"),
        "concat" => format!("({a} || {b})"),
        _ => format!("{op}({a}, {b})"),
    })
}
pub(crate) fn set_sql(
    set: &SetNode,
    columns: &IndexMap<String, ColumnRecord>,
    key: &[String],
    versions: &IndexMap<String, i64>,
    params: &mut Vec<SqlValue>,
) -> Result<String> {
    if set.is_empty() {
        return Err(invalid("empty set"));
    }
    let mut assignments = Vec::new();
    for (n, e) in set {
        let c = columns
            .get(n)
            .ok_or_else(|| invalid("set: unknown column"))?;
        if key.contains(n) {
            return Err(invalid("set cannot change primary key"));
        }
        assignments.push(format!("{} = {}", quote(n), expr_sql(e, columns, params)?));
        if c.kind == ColumnType::Json {
            let v = versions
                .get(n)
                .copied()
                .or(c.version)
                .ok_or_else(|| invalid("missing json version"))?;
            super::table::safe(v, 1)?;
            if Some(v) > c.version {
                return Err(invalid("json version newer than recorded"));
            }
            assignments.push(format!("{} = ?", quote(&format!("{n}$v"))));
            params.push(if e.0.is_null() {
                SqlValue::Null
            } else {
                SqlValue::Integer(v)
            });
        }
    }
    Ok(assignments.join(", "))
}
