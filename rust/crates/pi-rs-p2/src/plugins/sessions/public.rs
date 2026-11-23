use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    contracts::{
        AcceptsTable, Contract, MethodMapping, ValueAdapter, ValueMapping, VersionMappings,
    },
    data::ChordError,
    delta::Revision,
    host::{FacetHost, Members, ServeOptions},
    state::Source,
};
use serde_json::{Value, json};

use crate::api::v1::{Principal, p2_contracts::lookup};

use super::{arg, clean_title, old_row, wire};

pub(super) fn serve(
    host: &mut FacetHost,
    catalog: Arc<Contract>,
    index: Arc<Contract>,
) -> Result<()> {
    let sessions = Arc::new(lookup("p2.sessions")?);
    let operations = Arc::new(lookup("p2.session-operations")?);
    host.serve(
        sessions,
        move |_, principal, object| {
            let principal = principal.cloned().map(serde_json::from_value).transpose()?;
            let mut members = match principal {
                Some(Principal::Worker { .. }) => Members::variant("worker"),
                Some(Principal::Guest { .. }) => Members::variant("guest"),
                Some(Principal::Owner { .. }) => Members::variant("owner"),
                _ => return Err(ChordError::new("forbidden", "sessions are owner-only").into()),
            };
            let catalog = object.get(catalog.clone(), wire(json!({})))?;
            if let Some(Principal::Worker { session, .. }) = principal {
                let by_id = object.get(index.clone(), wire(json!({})))?.member("byId")?;
                let session_id = session.clone();
                members.state(
                    "session",
                    Source::derive(
                        object.batch(),
                        vec![by_id],
                        move |values| {
                            let by_id: Value = values[0].to_json().to_serde()?;
                            Ok(Revision::from_json(wire(by_id[&session_id].clone()))?)
                        },
                        |_| {},
                    ),
                )?;
                for method in ["renameSelf", "configureSelf", "treeSelf"] {
                    let (catalog, session) = (catalog.clone(), session.clone());
                    members.method(method, move |args, ctx| {
                        let (catalog, session) = (catalog.clone(), session.clone());
                        Box::pin(async move {
                            let mut args: Value = args.to_serde()?;
                            let current: Value = catalog
                                .call("get", wire(json!({"id":session})), &ctx.context)
                                .await?
                                .to_serde()?;
                            args["id"] = json!(session);
                            if method == "renameSelf" {
                                args["title"] = json!(clean_title(arg(&args, "title")?));
                            }
                            if method != "treeSelf"
                                || args["main"] != current["main"]
                                || args["branches"] != current["branches"]
                            {
                                catalog.call("update", wire(args), &ctx.context).await?;
                            }
                            Ok(wire(if method == "renameSelf" {
                                json!({"previous":current["title"]})
                            } else {
                                Value::Null
                            }))
                        })
                    })?;
                }
                return Ok(members);
            }
            let rows = catalog.member("sessions")?;
            if let Some(Principal::Guest { sessions, .. }) = principal {
                members.state(
                    "sessions",
                    Source::derive(
                        object.batch(),
                        vec![rows],
                        move |values| {
                            let rows: Vec<Value> =
                                serde_json::from_value(values[0].to_json().to_serde()?)?;
                            let shared: Vec<_> = rows
                                .into_iter()
                                .filter(|row| {
                                    row["archivedAt"].is_null()
                                        && row["id"]
                                            .as_str()
                                            .is_some_and(|id| sessions.contains_key(id))
                                })
                                .collect();
                            Ok(Revision::from_json(wire(json!(shared)))?)
                        },
                        |_| {},
                    ),
                )?;
                return Ok(members);
            }
            let lifecycle = object.get(operations.clone(), wire(json!({})))?;
            members.state("sessions", rows.clone())?;
            members.state("operations", lifecycle.member("operations")?)?;
            for (member, field) in [("defaultCwd", "cwd"), ("defaultEnv", "env")] {
                let fallback = if field == "cwd" {
                    json!(std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")))
                } else {
                    Value::Null
                };
                members.state(
                    member,
                    Source::derive(
                        object.batch(),
                        vec![rows.clone()],
                        move |values| {
                            let rows: Vec<Value> =
                                serde_json::from_value(values[0].to_json().to_serde()?)?;
                            let value = rows
                                .iter()
                                .find(|row| row["archivedAt"].is_null())
                                .map_or_else(|| fallback.clone(), |row| row[field].clone());
                            Ok(Revision::from_json(wire(value))?)
                        },
                        |_| {},
                    ),
                )?;
            }
            for method in [
                "create",
                "relocate",
                "clone",
                "fork",
                "archive",
                "restore",
                "dismissOperation",
                "rename",
                "pin",
            ] {
                let (catalog, lifecycle) = (catalog.clone(), lifecycle.clone());
                members.method(method, move |args, ctx| {
                    let (catalog, lifecycle) = (catalog.clone(), lifecycle.clone());
                    Box::pin(async move {
                        let mut args: Value = args.to_serde()?;
                        if matches!(method, "rename" | "pin") {
                            if method == "rename" {
                                let clean = clean_title(arg(&args, "title")?);
                                if clean.is_empty() || clean.encode_utf16().count() > 120 {
                                    return Err(Error::Invalid(
                                        "The title must be 1–120 characters".into(),
                                    ));
                                }
                                args["title"] = json!(clean);
                            }
                            catalog.call("update", wire(args), &ctx.context).await?;
                            return Ok(wire(Value::Null));
                        }
                        if matches!(method, "clone" | "fork") && args["copy"].is_null() {
                            args["copy"] = json!(uuid::Uuid::new_v4());
                        }
                        let name = if method == "dismissOperation" {
                            "dismiss"
                        } else {
                            method
                        };
                        lifecycle.call(name, wire(args), &ctx.context).await
                    })
                })?;
            }
            Ok(members)
        },
        ServeOptions {
            accepts: accepts(),
            ..Default::default()
        },
    )
}

fn accepts() -> AcceptsTable {
    (3..=6)
        .map(|version| {
            let row_version = (version - 1).min(4);
            let one: ValueAdapter = Arc::new(move |row| Ok(old_row(row_version, row)));
            let rows: ValueAdapter = Arc::new(move |rows| {
                let pi_rs_schema::Value::Array(rows) = rows else {
                    return Err(Error::Invalid("Expected session rows".into()));
                };
                Ok(pi_rs_schema::Value::Array(
                    rows.into_iter()
                        .map(|row| old_row(row_version, row))
                        .collect(),
                ))
            });
            (
                version,
                VersionMappings {
                    state: IndexMap::from([
                        (
                            ("owner".into(), "sessions".into()),
                            ValueMapping::Map(rows.clone()),
                        ),
                        (("guest".into(), "sessions".into()), ValueMapping::Map(rows)),
                        (
                            ("worker".into(), "session".into()),
                            ValueMapping::Map(one.clone()),
                        ),
                    ]),
                    methods: ["create", "clone", "fork"]
                        .into_iter()
                        .map(|method| {
                            (
                                method.into(),
                                MethodMapping::Adapt {
                                    args: (version <= 5).then(|| Arc::new(Ok) as ValueAdapter),
                                    result: Some(one.clone()),
                                },
                            )
                        })
                        .chain((version == 4 || version == 5 || version == 6).then(|| {
                            (
                                "treeSelf".into(),
                                MethodMapping::Adapt {
                                    args: Some(Arc::new(Ok)),
                                    result: None,
                                },
                            )
                        }))
                        .collect(),
                    ..Default::default()
                },
            )
        })
        .collect()
}
