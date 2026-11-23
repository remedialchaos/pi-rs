//! `status` built-in plugin: generation-fenced live worker status (`p2.status-store`,
//! `p2.session-status`, `p2.worker-control`, `p2.session-metrics`) and worker task-graph reporting
//! (`p2.status-report`).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    watch::WatchCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::v1::{
    FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::server_of,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WorkStep {
    Thinking,
    Tools { names: Vec<String> },
    Compacting,
    Working,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    pub since: u64,
    pub rounds: u64,
    pub step: WorkStep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFailure {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredReport {
    generation: u64,
    tasks: u64,
    busy: Vec<u64>,
    work: IndexMap<String, Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<RunFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct StoreReportArgs {
    session: String,
    generation: u64,
    tasks: u64,
    busy: Vec<u64>,
    work: IndexMap<String, Work>,
    failure: Option<RunFailure>,
    usage: Option<Value>,
}

/// Computes a busy conversation's step: running tools first, then compaction, generation, working.
pub fn step_of(running_tool_names: &[String], task_kinds: &BTreeSet<String>) -> WorkStep {
    if !running_tool_names.is_empty() {
        return WorkStep::Tools {
            names: running_tool_names.to_vec(),
        };
    }
    if task_kinds.contains("pi.compaction") {
        return WorkStep::Compacting;
    }
    if task_kinds.contains("pi.generation") {
        return WorkStep::Thinking;
    }
    WorkStep::Working
}

/// Redacts owner-only error/detail fields for guests while keeping running/tasks/busy/work/usage.
pub fn redact_statuses_for_guest(statuses: &Value, allowed_sessions: &BTreeSet<String>) -> Value {
    let mut out = Map::new();
    let Some(map) = statuses.as_object() else {
        return Value::Object(out);
    };
    for session in allowed_sessions {
        let Some(status) = map.get(session).and_then(Value::as_object) else {
            continue;
        };
        let mut entry = Map::new();
        for key in ["running", "tasks", "busy", "work", "usage"] {
            if let Some(value) = status.get(key) {
                entry.insert(key.into(), value.clone());
            }
        }
        if let Some(failure) = status.get("failure").and_then(Value::as_object)
            && let Some(reason) = failure.get("reason")
        {
            entry.insert("failure".into(), json!({ "reason": reason }));
        }
        out.insert(session.clone(), Value::Object(entry));
    }
    Value::Object(out)
}

fn combine_statuses(workers: &Value, reports: &BTreeMap<String, StoredReport>) -> Value {
    let mut out = Map::new();
    let Some(worker_map) = workers.as_object() else {
        return Value::Object(out);
    };
    for (session, worker) in worker_map {
        let running = worker
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let generation = worker
            .get("generation")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !running {
            let mut entry = Map::new();
            entry.insert("running".into(), Value::Bool(false));
            entry.insert("tasks".into(), json!(0));
            entry.insert("busy".into(), json!([]));
            entry.insert("work".into(), json!({}));
            if let Some(error) = worker.get("error").and_then(Value::as_str)
                && !error.is_empty()
            {
                entry.insert("error".into(), Value::String(error.into()));
            }
            out.insert(session.clone(), Value::Object(entry));
            continue;
        }
        match reports.get(session) {
            Some(report) if report.generation == generation => {
                let mut entry = Map::new();
                entry.insert("running".into(), Value::Bool(true));
                entry.insert("tasks".into(), json!(report.tasks));
                entry.insert("busy".into(), json!(report.busy));
                entry.insert(
                    "work".into(),
                    serde_json::to_value(&report.work).unwrap_or_else(|_| json!({})),
                );
                if let Some(ref failure) = report.failure {
                    entry.insert(
                        "failure".into(),
                        serde_json::to_value(failure).unwrap_or(Value::Null),
                    );
                }
                if let Some(ref usage) = report.usage {
                    entry.insert("usage".into(), usage.clone());
                }
                out.insert(session.clone(), Value::Object(entry));
            }
            _ => {
                out.insert(
                    session.clone(),
                    json!({ "running": true, "tasks": 0, "busy": [], "work": {} }),
                );
            }
        }
    }
    Value::Object(out)
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let store_contract = Arc::new(lookup("p2.status-store")?);
    let status_contract = Arc::new(lookup("p2.session-status")?);
    let control_contract = Arc::new(lookup("p2.worker-control")?);
    let metrics_contract = Arc::new(lookup("p2.session-metrics")?);
    let hosts_contract = Arc::new(lookup("p2.hosts")?);

    registry.register(
        "status",
        FacetKind::Server,
        define_facet(move |host| {
            let reports_mem: Arc<Mutex<BTreeMap<String, StoredReport>>> = Arc::default();
            let viewers_mem: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>> = Arc::default();
            let store_for_status = store_contract.clone();
            let store_for_metrics = store_contract.clone();
            let hosts_ref = hosts_contract.clone();

            let store_reports = reports_mem.clone();
            host.serve(
                store_contract.clone(),
                move |_, _, object| {
                    let hosts = object
                        .host()
                        .served_contract("p2.hosts")
                        .map(|_| object.get(hosts_ref.clone(), JsonValue::from_serde(json!({}))))
                        .transpose()?;
                    let workers_snapshot = Arc::new(Mutex::new(json!({})));
                    let statuses_src = object.writable(JsonValue::from_serde(json!({})))?;

                    if let Some(hosts_held) = hosts.clone() {
                        let w_snap = workers_snapshot.clone();
                        let r_mem = store_reports.clone();
                        let s_src = statuses_src.clone();
                        object.on_live(move |ctx| {
                            let hosts_held = hosts_held.clone();
                            let w_snap = w_snap.clone();
                            let r_mem = r_mem.clone();
                            let s_src = s_src.clone();
                            Box::pin(async move {
                                let workers_member = hosts_held.member("workers")?;
                                let publish = |workers_val: Value,
                                               src: &Source,
                                               c: &pi_rs_chord::context::Context|
                                 -> Result<()> {
                                    *w_snap.lock().unwrap_or_else(|p| p.into_inner()) =
                                        workers_val.clone();
                                    let combined = {
                                        let reports =
                                            r_mem.lock().unwrap_or_else(|p| p.into_inner());
                                        combine_statuses(&workers_val, &reports)
                                    };
                                    src.set(JsonValue::from_serde(combined), c)
                                };
                                let initial = workers_member.value()?.to_json().to_serde()?;
                                publish(initial, &s_src, &ctx.context)?;
                                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                                let _sub = workers_member.subscribe(
                                    move |call, _| {
                                        if let WatchCall::Value { value, .. } = call
                                            && let Ok(v) = value.to_serde()
                                        {
                                            let _ = tx.send(v);
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                loop {
                                    let Ok(Some(next)) = ctx.context.await_with(rx.recv()).await
                                    else {
                                        break;
                                    };
                                    publish(next, &s_src, &ctx.context)?;
                                }
                                Ok(())
                            })
                        });
                    }

                    let mut members = Members::new();
                    members.state("statuses", statuses_src.clone())?;

                    let rep_mem = store_reports.clone();
                    let w_snap = workers_snapshot.clone();
                    let s_src = statuses_src.clone();
                    members.method("report", move |args, ctx| {
                        let rep_mem = rep_mem.clone();
                        let w_snap = w_snap.clone();
                        let s_src = s_src.clone();
                        Box::pin(async move {
                            let args: StoreReportArgs = serde_json::from_value(args.to_serde()?)?;
                            let workers_val =
                                w_snap.lock().unwrap_or_else(|p| p.into_inner()).clone();
                            let worker = workers_val.get(&args.session);
                            let running = worker
                                .and_then(|w| w.get("running"))
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let generation = worker
                                .and_then(|w| w.get("generation"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            if !running || generation != args.generation {
                                return Ok(JsonValue::Null);
                            }
                            let mut busy = args.busy;
                            busy.sort_unstable();
                            let combined = {
                                let mut reports = rep_mem.lock().unwrap_or_else(|p| p.into_inner());
                                reports.insert(
                                    args.session,
                                    StoredReport {
                                        generation: args.generation,
                                        tasks: args.tasks,
                                        busy,
                                        work: args.work,
                                        failure: args.failure,
                                        usage: args.usage,
                                    },
                                );
                                combine_statuses(&workers_val, &reports)
                            };
                            s_src.set(JsonValue::from_serde(combined), &ctx.context)?;
                            Ok(JsonValue::Null)
                        })
                    })?;

                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            host.serve(
                control_contract.clone(),
                move |params, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| {
                            ChordError::new("forbidden", "Only owners may retry session workers")
                        })?;
                    if !parsed.is_owner() {
                        return Err(ChordError::new(
                            "forbidden",
                            "Only owners may retry session workers",
                        )
                        .into());
                    }
                    let session = params.to_serde()?["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?
                        .to_owned();
                    let host_handle = object.host();
                    let mut members = Members::variant("owner");
                    members.method("restart", move |_, _| {
                        let host_handle = host_handle.clone();
                        let session = session.clone();
                        Box::pin(async move {
                            let natives = server_of(&host_handle)?;
                            natives.workers.retry(&session).await?;
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            let viewers = viewers_mem.clone();
            host.serve(
                metrics_contract.clone(),
                move |params, principal, object| {
                    let params = params.to_serde()?;
                    let session = params["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?
                        .to_owned();
                    let viewer = params["viewer"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing viewer param".into()))?
                        .to_owned();
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "viewers only"))?;
                    if !parsed.can_view(&session) {
                        return Err(ChordError::new("forbidden", "viewers only").into());
                    }
                    let status_store =
                        object.get(store_for_metrics.clone(), JsonValue::from_serde(json!({})))?;
                    let count = {
                        let mut guard = viewers.lock().unwrap_or_else(|p| p.into_inner());
                        let set = guard.entry(session.clone()).or_default();
                        set.insert(viewer.clone());
                        set.len() as u64
                    };
                    let clients = object.writable(JsonValue::from_serde(json!(count)))?;
                    let v_close = viewers.clone();
                    let s_close = session.clone();
                    let v_id = viewer.clone();
                    let _guard = object.open(
                        |_| Box::pin(async { Ok(()) }),
                        move |_, _| {
                            Box::pin(async move {
                                let mut guard = v_close.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(set) = guard.get_mut(&s_close) {
                                    set.remove(&v_id);
                                    if set.is_empty() {
                                        guard.remove(&s_close);
                                    }
                                }
                                Ok(())
                            })
                        },
                    );
                    let session_for_usage = session.clone();
                    let usage = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let src = status_store.member("statuses")?;
                            let extract = {
                                let session = session_for_usage.clone();
                                move |val: &Value| {
                                    val.get(&session)
                                        .and_then(|s| s.get("usage"))
                                        .cloned()
                                        .unwrap_or(Value::Null)
                                }
                            };
                            let init = extract(&src.value()?.to_json().to_serde()?);
                            target.set(JsonValue::from_serde(init), &ctx.context)?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let sub = src.subscribe(
                                move |call, context| match call {
                                    WatchCall::Value { value, .. } => {
                                        let next = extract(&value.to_serde()?);
                                        changed.set(JsonValue::from_serde(next), context)
                                    }
                                    WatchCall::Failed { error } => changed.fail(error, context),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;
                    let mut members = Members::variant("reader");
                    members.state("clients", clients)?;
                    members.state("usage", usage)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            host.serve(
                status_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "owners and guests only"))?;
                    let store =
                        object.get(store_for_status.clone(), JsonValue::from_serde(json!({})))?;
                    match parsed {
                        Principal::Worker {
                            session,
                            generation,
                        } => {
                            let mut members = Members::variant("reporter");
                            members.method("report", move |args, ctx| {
                                let store = store.clone();
                                let session = session.clone();
                                Box::pin(async move {
                                    let mut payload = args.to_serde()?;
                                    if let Some(obj) = payload.as_object_mut() {
                                        obj.insert("session".into(), Value::String(session));
                                        obj.insert("generation".into(), json!(generation));
                                    }
                                    store
                                        .call(
                                            "report",
                                            JsonValue::from_serde(payload),
                                            &ctx.context,
                                        )
                                        .await
                                })
                            })?;
                            Ok(members)
                        }
                        Principal::Guest { sessions, .. } => {
                            let allowed: BTreeSet<String> = sessions.keys().cloned().collect();
                            let redacted = object.follow(move |target, ctx| {
                                Box::pin(async move {
                                    let src = store.member("statuses")?;
                                    let init = redact_statuses_for_guest(
                                        &src.value()?.to_json().to_serde()?,
                                        &allowed,
                                    );
                                    target.set(JsonValue::from_serde(init), &ctx.context)?;
                                    let changed = target.clone();
                                    let ended = target.clone();
                                    let allowed_cb = allowed.clone();
                                    let sub = src.subscribe(
                                        move |call, context| match call {
                                            WatchCall::Value { value, .. } => {
                                                let next = redact_statuses_for_guest(
                                                    &value.to_serde()?,
                                                    &allowed_cb,
                                                );
                                                changed.set(JsonValue::from_serde(next), context)
                                            }
                                            WatchCall::Failed { error } => {
                                                changed.fail(error, context)
                                            }
                                        },
                                        move |_| ended.detach("ended"),
                                    );
                                    Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                                })
                            })?;
                            let mut members = Members::variant("reader");
                            members.state("statuses", redacted)?;
                            Ok(members)
                        }
                        Principal::Owner { .. } => {
                            let mut members = Members::variant("reader");
                            members.state(
                                "statuses",
                                object.follow_source(store.member("statuses")?)?,
                            )?;
                            Ok(members)
                        }
                        _ => Err(ChordError::new("forbidden", "owners and guests only").into()),
                    }
                },
                ServeOptions::default(),
            )
        }),
    )
}
