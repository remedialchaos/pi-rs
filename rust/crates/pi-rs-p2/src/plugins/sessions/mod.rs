mod catalog;
mod copy;
mod lifecycle;
mod preferences;
mod public;
#[cfg(test)]
mod tests;
mod transfer;

use crate::api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    contracts::{AcceptsTable, MethodMapping, ValueAdapter, ValueMapping, VersionMappings},
    delta::Revision,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let catalog = Arc::new(lookup("p2.catalog")?);
    let index = Arc::new(lookup("p2.session-index")?);
    let table = catalog::CatalogTable::new()?;
    let preferences = preferences::Preferences::new()?;
    registry.register(
        "sessions",
        FacetKind::Server,
        define_facet(move |host| {
            preferences.serve(host)?;
            let table = table.clone();
            host.serve(
                catalog.clone(),
                move |_, _, object| table.members(object),
                ServeOptions {
                    accepts: catalog_accepts(),
                    ..Default::default()
                },
            )?;
            let index_catalog = catalog.clone();
            host.serve(
                index.clone(),
                move |_, _, object| {
                    let rows = object
                        .get(index_catalog.clone(), wire(json!({})))?
                        .member("sessions")?;
                    let by_id = Source::derive(
                        object.batch(),
                        vec![rows],
                        |values| {
                            let rows: Vec<Value> =
                                serde_json::from_value(values[0].to_json().to_serde()?)?;
                            let pairs = rows
                                .into_iter()
                                .map(|row| Ok((arg(&row, "id")?.to_owned(), row)))
                                .collect::<Result<serde_json::Map<_, _>>>()?;
                            Ok(Revision::from_json(wire(Value::Object(pairs)))?)
                        },
                        |_| {},
                    );
                    let mut members = Members::new();
                    members.state("byId", by_id)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
            lifecycle::serve(host, catalog.clone())?;
            public::serve(host, catalog.clone(), index.clone())
        }),
    )?;
    registry.register("sessions", FacetKind::Worker, transfer::facet()?)
}

fn wire(value: Value) -> JsonValue {
    JsonValue::from_serde(value)
}

fn arg<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("Missing {field}")))
}

fn clean_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

fn truncate(value: &str, units: usize) -> String {
    String::from_utf16_lossy(&value.encode_utf16().take(units).collect::<Vec<_>>())
}

fn old_row(version: u64, mut row: pi_rs_schema::Value) -> pi_rs_schema::Value {
    use pi_rs_schema::Value::{Array, Object};
    if let Object(fields) = &mut row {
        if version == 2 {
            fields.shift_remove("main");
            fields.shift_remove("branches");
        } else {
            let title = fields
                .get("title")
                .cloned()
                .unwrap_or(pi_rs_schema::Value::Null);
            if let Some(Array(branches)) = fields.get_mut("branches") {
                for branch in branches {
                    if let Object(fields) = branch {
                        fields.shift_remove("parent");
                        if version == 3
                            && fields
                                .get("label")
                                .is_some_and(pi_rs_schema::Value::is_null)
                        {
                            fields.insert("label".into(), title.clone());
                        }
                    }
                }
            }
        }
    }
    row
}

fn catalog_accepts() -> AcceptsTable {
    (2..=4)
        .map(|version| {
            let rows: ValueAdapter = Arc::new(move |value| match value {
                pi_rs_schema::Value::Array(rows) => Ok(pi_rs_schema::Value::Array(
                    rows.into_iter().map(|row| old_row(version, row)).collect(),
                )),
                _ => Err(Error::Invalid("Expected catalog rows".into())),
            });
            let row: ValueAdapter = Arc::new(move |row| Ok(old_row(version, row)));
            (
                version,
                VersionMappings {
                    state: IndexMap::from([(
                        ("members".into(), "sessions".into()),
                        ValueMapping::Map(rows),
                    )]),
                    methods: ["get", "create", "update"]
                        .into_iter()
                        .map(|method| {
                            (
                                method.into(),
                                MethodMapping::Adapt {
                                    args: (method == "update")
                                        .then(|| Arc::new(Ok) as ValueAdapter),
                                    result: Some(row.clone()),
                                },
                            )
                        })
                        .collect(),
                    ..Default::default()
                },
            )
        })
        .collect()
}
