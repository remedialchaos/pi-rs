//! Server facet for the `subagents` plugin (`p2.child-store` and `p2.subagents`).

use std::{collections::BTreeSet, sync::Arc};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{MethodMapping, ValueMapping, VersionMappings},
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    store::{Bindings, LiveShape, Projection, Query, ReadPlan, ReadRange, StoreEngine},
    watch::WatchCall,
};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{Principal, server::server_of},
    runtime::location::Location,
};

use super::{
    children::child_rows_table,
    contract::{Child, ChildFailure, child_store_contract, subagents_contract},
};

const WINDOW: usize = 100_000;

fn without_detail(child: &mut Child) {
    if let Some(failure) = &mut child.failure
        && failure.detail.is_some()
    {
        *failure = ChildFailure {
            reason: failure.reason.clone(),
            detail: None,
        };
    }
}

fn filter_children_for_guest(value: Value, allowed: &BTreeSet<String>) -> Value {
    let Some(map) = value.as_object() else {
        return json!({});
    };
    let mut out = Map::new();
    for session in allowed {
        let list: Vec<Child> = map
            .get(session)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let filtered: Vec<Child> = list
            .into_iter()
            .map(|mut child| {
                without_detail(&mut child);
                child
            })
            .collect();
        out.insert(
            session.clone(),
            serde_json::to_value(filtered).unwrap_or_else(|_| json!([])),
        );
    }
    Value::Object(out)
}

fn rows_to_children_map(rows: Value) -> JsonValue {
    let Some(items) = rows.as_array() else {
        return JsonValue::Object(IndexMap::new());
    };
    let mut map = IndexMap::new();
    for row in items {
        let Some(session_id) = row
            .get("session")
            .and_then(|s| s.get("params"))
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if let Some(children) = row.get("children") {
            map.insert(
                session_id.to_owned().into(),
                JsonValue::from_serde(children.clone()),
            );
        }
    }
    JsonValue::Object(map)
}

/// Extracts `subagentLocations` (`conversationId -> Location`) for worker init from `ChildStore.children`.
pub fn subagent_locations_for_session(children_map: &Value, session: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(list) = children_map.get(session).and_then(Value::as_array) else {
        return out;
    };
    for item in list {
        if let Ok(child) = serde_json::from_value::<Child>(item.clone())
            && let Some(loc) = child.location
            && let Ok(val) = serde_json::to_value::<Location>(loc)
        {
            out.insert(child.conversation_id.to_string(), val);
        }
    }
    out
}

fn strip_children_map_v3_args(
    value: pi_rs_chord::data::args::Value,
) -> pi_rs_chord::data::args::Value {
    use pi_rs_chord::data::args::Value as ArgValue;
    let ArgValue::Object(map) = value else {
        return value;
    };
    let mut out = IndexMap::with_capacity(map.len());
    for (session, list) in map {
        let stripped = match list {
            ArgValue::Array(items) => ArgValue::Array(
                items
                    .into_iter()
                    .map(|item| match item {
                        ArgValue::Object(mut obj) => {
                            obj.shift_remove("updatedAt");
                            obj.shift_remove("failure");
                            ArgValue::Object(obj)
                        }
                        other => other,
                    })
                    .collect(),
            ),
            other => other,
        };
        out.insert(session, stripped);
    }
    ArgValue::Object(out)
}

fn subagents_accepts() -> pi_rs_chord::contracts::AcceptsTable {
    let mut v2 = VersionMappings::default();
    v2.state.insert(
        ("reader".into(), "children".into()),
        ValueMapping::Map(Arc::new(|value| Ok(strip_children_map_v3_args(value)))),
    );
    v2.methods.insert(
        "report".into(),
        MethodMapping::Adapt {
            args: Some(Arc::new(Ok)),
            result: None,
        },
    );
    let mut table = IndexMap::new();
    table.insert(2, v2);
    table
}

/// Builds the server facet for the `subagents` plugin.
pub fn server_facet() -> Result<Facet> {
    let child_store = Arc::new(child_store_contract()?);
    let subagents = Arc::new(subagents_contract()?);
    let table = Arc::new(child_rows_table()?);

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let child_store_contract = child_store.clone();
        let table_def = table.clone();

        host.serve(
            child_store_contract,
            move |_params, _principal, self_| {
                self_.table((*table_def).clone())?;
                let store: StoreEngine = server_of(&host_handle)?.store().clone();
                let query_table = (*table_def).clone();
                let query_store = store.clone();

                let children_source: Source = self_.follow(move |target, ctx| {
                    let store = query_store.clone();
                    let table_def = query_table.clone();
                    Box::pin(async move {
                        let live = store.live_query(LiveShape::Rows {
                            range: ReadRange {
                                limit: WINDOW,
                                ..Default::default()
                            },
                            paged: false,
                        })?;
                        let plan = ReadPlan::new(
                            Query::new("p2.child-store", table_def)?,
                            None,
                            Projection::default(),
                            false,
                            Bindings::default(),
                        )?;
                        live.read(&store, plan, ctx.context.clone()).await?;
                        let initial = live.value()?.to_json().to_serde()?;
                        target.set(rows_to_children_map(initial), &ctx.context)?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let subscription = live.source().subscribe(
                            move |call, context| match call {
                                WatchCall::Value { value, .. } => {
                                    let serde_val = value.to_serde()?;
                                    changed.set(rows_to_children_map(serde_val), context)
                                }
                                WatchCall::Failed { error } => changed.fail(error, context),
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || {
                            drop(subscription);
                            drop(live);
                        }) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let mut members = Members::new();
                members.state("children", children_source)?;

                let report_host = host_handle.clone();
                let report_store = store;
                let report_table = table_def.clone();
                members.method("report", move |args, _ctx| {
                    let host = report_host.clone();
                    let store = report_store.clone();
                    let table_def = report_table.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let session = val
                            .get("session")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing session".into()))?;
                        let generation = val
                            .get("generation")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| Error::Invalid("missing generation".into()))?;
                        let reported = val
                            .get("children")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing children".into()))?;

                        let workers = server_of(&host)?.workers.clone();
                        let state = workers.observe().borrow().get(session).cloned();
                        let Some(worker) =
                            state.filter(|w| w.running && w.generation == generation)
                        else {
                            return Ok(JsonValue::Null);
                        };
                        let _ = worker;
                        let write = table_def.put(
                            "p2.child-store",
                            json!({
                                "session": { "kind": "session", "params": { "id": session } },
                                "children": reported,
                            }),
                        )?;
                        store.commit(vec![write]).await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let subagents_store_contract = child_store.clone();
        host.serve(
            subagents.clone(),
            move |_params, principal_raw, self_| {
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => {
                        return Err(ChordError::new("forbidden", "owners and guests only").into());
                    }
                };
                let store_ref = self_.get(
                    subagents_store_contract.clone(),
                    JsonValue::Object(IndexMap::new()),
                )?;

                match principal {
                    Principal::Worker {
                        session,
                        generation,
                    } => {
                        let mut members = Members::variant("reporter");
                        members.method("report", move |args, ctx| {
                            let store_ref = store_ref.clone();
                            let session = session.clone();
                            Box::pin(async move {
                                let mut payload = args.to_serde()?;
                                if let Some(obj) = payload.as_object_mut() {
                                    obj.insert("session".into(), json!(session));
                                    obj.insert("generation".into(), json!(generation));
                                }
                                store_ref
                                    .call("report", JsonValue::from_serde(payload), &ctx.context)
                                    .await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                        Ok(members)
                    }
                    Principal::Guest { sessions, .. } => {
                        let allowed: BTreeSet<String> = sessions.keys().cloned().collect();
                        let source = store_ref.member("children")?;
                        let filtered = self_.follow(move |target, ctx| {
                            let source = source.clone();
                            let allowed = allowed.clone();
                            Box::pin(async move {
                                let initial = source.value()?.to_json().to_serde()?;
                                target.set(
                                    JsonValue::from_serde(filter_children_for_guest(
                                        initial, &allowed,
                                    )),
                                    &ctx.context,
                                )?;
                                let changed = target.clone();
                                let ended = target.clone();
                                let subscription = source.subscribe(
                                    move |call, context| match call {
                                        WatchCall::Value { value, .. } => {
                                            let serde_val = value.to_serde()?;
                                            changed.set(
                                                JsonValue::from_serde(filter_children_for_guest(
                                                    serde_val, &allowed,
                                                )),
                                                context,
                                            )
                                        }
                                        WatchCall::Failed { error } => changed.fail(error, context),
                                    },
                                    move |_| ended.detach("ended"),
                                );
                                Ok(Box::new(move || drop(subscription))
                                    as Box<dyn FnOnce() + Send>)
                            })
                        })?;
                        let mut members = Members::variant("reader");
                        members.state("children", filtered)?;
                        Ok(members)
                    }
                    Principal::Owner { .. } => {
                        let source = store_ref.member("children")?;
                        let followed = self_.follow_source(source)?;
                        let mut members = Members::variant("reader");
                        members.state("children", followed)?;
                        Ok(members)
                    }
                    _ => Err(ChordError::new("forbidden", "owners and guests only").into()),
                }
            },
            ServeOptions {
                accepts: subagents_accepts(),
                ..Default::default()
            },
        )?;

        let _ = Context::background();
        Ok(())
    }))
}
