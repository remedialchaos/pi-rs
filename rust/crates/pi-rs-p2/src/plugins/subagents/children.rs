//! Chord table `children` (`ChildRows`) storing per-session subagent metadata.

use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{HostKind, read_params, schema},
    json::JsonValue,
    store::{ColumnRecord, JsonColumn, TableDefinition, TableHistory},
};
use serde_json::json;

fn session_host_kind() -> Result<HostKind> {
    HostKind::new(
        "session",
        read_params(&[("id".into(), schema::str())], "session")?,
    )
}

fn child_schema(v3: bool) -> pi_rs_chord::json::JsonValue {
    let location = schema::obj([
        ("cwd", schema::required(schema::str())),
        (
            "env",
            schema::required(schema::union([schema::str(), schema::nul()])),
        ),
    ]);
    let mut props: Vec<(&str, pi_rs_chord::contracts::schema::Property)> = vec![
        ("name", schema::required(schema::str())),
        (
            "kind",
            schema::opt(JsonValue::from_serde(
                json!({"const": "agent", "type": "string"}),
            )),
        ),
        ("callId", schema::opt(schema::str())),
        ("archived", schema::opt(schema::bool())),
        ("conversationId", schema::required(schema::num())),
        ("model", schema::required(schema::str())),
        ("thinkingLevel", schema::required(schema::str())),
        ("tools", schema::required(schema::arr(schema::str()))),
        ("location", schema::opt(location)),
        ("lastEntryId", schema::required(schema::num())),
    ];
    if v3 {
        props.push(("updatedAt", schema::opt(schema::num())));
        props.push((
            "failure",
            schema::opt(schema::obj([
                ("reason", schema::required(schema::str())),
                ("detail", schema::opt(schema::str())),
            ])),
        ));
    }
    schema::arr(schema::obj(props))
}

/// Builds the `children` table history (v1 -> v2) with `session` host column and `children` JSON column.
pub fn child_rows_history() -> Result<TableHistory> {
    let session = session_host_kind()?;
    let col_v1 = JsonColumn::new(child_schema(false), 1, None)?;
    let def_v1 = TableDefinition::new(
        "children",
        1,
        IndexMap::from([
            ("session".into(), ColumnRecord::host("session")),
            ("children".into(), col_v1.record()),
        ]),
        vec!["session".into()],
        IndexMap::new(),
    )?
    .bind_host("session", session)?
    .bind_json("children", col_v1)?;

    let col_v2 = JsonColumn::new(
        child_schema(true),
        2,
        Some(Arc::new(|value, _from| Ok(value))),
    )?;
    TableHistory::base(def_v1).evolve(|editor| {
        editor.json_schema("children", col_v2)?;
        Ok(())
    })
}

/// Returns the current (v2) `children` `TableDefinition`.
pub fn child_rows_table() -> Result<TableDefinition> {
    Ok(child_rows_history()?.definition)
}
