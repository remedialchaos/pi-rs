//! Rollups server side: `p2.rollup-store` and `p2.sidebar-rollups`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::{ChordError, Validator},
    delta::Segment,
    documents::Definition,
    host::{Facet, Held, Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use pi_rs_codemode::{CodemodeSandbox, CodemodeTool, HostError, SandboxOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use uuid::Uuid;

use crate::api::v1::{Principal, p2_contracts::lookup};

use super::runner::{
    MENTIONS_GLOBAL, PreviewResult, RUN_MEMORY_BYTES, RollupFacts, fingerprint_of, input_of,
    mention_of, mention_read, preview_to_json, run_rollup,
};

const TICK_MS: u64 = 30_000;
const EVERYTHING: &[&str] = &["sessions.*", "projects"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateMode {
    Force,
    Skip,
    Check,
}

struct ActiveRun {
    search: Held,
    ctx: Context,
    mentions: Mutex<HashMap<String, Value>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn saved_definition() -> Result<Arc<Definition>> {
    let schema = JsonValue::from_serde(json!({
        "type": "object",
        "properties": {
            "rollups": { "type": "object" }
        },
        "required": ["rollups"],
        "additionalProperties": false
    }));
    let def = Definition::new("p2.rollups", schema, 1, || {
        Ok(JsonValue::from_serde(json!({ "rollups": {} })))
    })?
    .persisted(true, |_, _, deltas_since_base| deltas_since_base >= 20);
    Ok(Arc::new(def))
}

fn rollup_output_validator() -> Result<Validator> {
    let member = json!({
        "anyOf": [
            { "type": "string", "minLength": 1, "maxLength": 200 },
            {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "minLength": 1, "maxLength": 200 },
                    "at": { "type": "number" },
                    "keep": { "type": "boolean" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        ]
    });
    let members = json!({ "type": "array", "items": member, "maxItems": 5000 });
    let target = json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "folder" },
                    "env": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                    "cwd": { "type": "string", "minLength": 1 }
                },
                "required": ["kind", "env", "cwd"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "project" },
                    "project": { "type": "string", "minLength": 1 }
                },
                "required": ["kind", "project"],
                "additionalProperties": false
            }
        ]
    });
    let group = json!({
        "type": "object",
        "properties": {
            "key": { "type": "string", "minLength": 1, "maxLength": 200 },
            "name": { "type": "string", "minLength": 1, "maxLength": 120 },
            "title": { "type": "string", "maxLength": 500 },
            "keep": { "type": "boolean" },
            "target": target,
            "members": members
        },
        "required": ["key", "name", "members"],
        "additionalProperties": false
    });
    let output_schema = json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "layout": { "type": "string", "const": "sessions" },
                    "members": members
                },
                "required": ["layout", "members"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "layout": { "type": "string", "const": "groups" },
                    "groups": { "type": "array", "items": group, "maxItems": 500 }
                },
                "required": ["layout", "groups"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "layout": { "type": "string", "const": "link" },
                    "members": members
                },
                "required": ["layout", "members"],
                "additionalProperties": false
            }
        ]
    });
    Validator::compile(&output_schema)
}

fn fresh(definition: &Value) -> Value {
    json!({
        "definition": definition,
        "output": null,
        "checkedAt": null,
        "error": null,
        "reads": [],
        "fingerprint": null
    })
}

async fn read_facts(
    catalog: &Held,
    statuses: &Held,
    search: &Held,
    projects: &Held,
    familiar: &Held,
    ctx: &Context,
) -> Result<RollupFacts> {
    catalog.wait(ctx).await?;
    statuses.wait(ctx).await?;
    search.wait(ctx).await?;
    projects.wait(ctx).await?;
    familiar.wait(ctx).await?;

    let sessions_val = catalog.member("sessions")?.value()?.to_json().to_serde()?;
    let statuses_val = statuses.member("statuses")?.value()?.to_json().to_serde()?;
    let answers_val = search.member("answers")?.value()?.to_json().to_serde()?;
    let reads_val = search.member("reads")?.value()?.to_json().to_serde()?;
    let projects_val = projects.member("projects")?.value()?.to_json().to_serde()?;
    let memberships_val = projects.member("sessions")?.value()?.to_json().to_serde()?;
    let familiar_val = familiar.member("session")?.value()?.to_json().to_serde()?;

    Ok(RollupFacts {
        sessions: sessions_val.as_array().cloned().unwrap_or_default(),
        statuses: statuses_val,
        answers: answers_val,
        reads: reads_val,
        projects: projects_val.as_array().cloned().unwrap_or_default(),
        memberships: memberships_val,
        familiar: familiar_val.as_str().map(str::to_owned),
    })
}

async fn call_mentions(
    search: &Held,
    query: &str,
    since: Option<u64>,
    ctx: &Context,
) -> Result<Value> {
    let args = match since {
        Some(since) => json!({ "query": query, "since": since }),
        None => json!({ "query": query }),
    };
    search
        .call("mentions", JsonValue::from_serde(args), ctx)
        .await?
        .to_serde()
}

pub fn server_facet() -> Facet {
    define_facet(|host| {
        let store_contract = Arc::new(lookup("p2.rollup-store")?);
        let sidebar_contract = Arc::new(lookup("p2.sidebar-rollups")?);
        let catalog_contract = Arc::new(lookup("p2.catalog")?);
        let status_contract = Arc::new(lookup("p2.status-store")?);
        let search_contract = Arc::new(lookup("p2.search-index")?);
        let project_contract = Arc::new(lookup("p2.project-service")?);
        let familiar_contract = Arc::new(lookup("p2.familiar-store")?);
        let hosts_contract = Arc::new(lookup("p2.hosts")?);
        let saved_def = saved_definition()?;
        let output_validator = Arc::new(rollup_output_validator()?);

        let fam_for_store = familiar_contract.clone();
        host.serve(
            store_contract.clone(),
            move |_, _, object| {
                let saved = object.state(saved_def.clone())?;
                let catalog = object.get(catalog_contract.clone(), JsonValue::Object(Default::default()))?;
                let statuses = object.get(status_contract.clone(), JsonValue::Object(Default::default()))?;
                let search = object.get(search_contract.clone(), JsonValue::Object(Default::default()))?;
                let projects = object.get(project_contract.clone(), JsonValue::Object(Default::default()))?;
                let familiar = object.get(fam_for_store.clone(), JsonValue::Object(Default::default()))?;

                let runs: Arc<Mutex<HashMap<String, Arc<ActiveRun>>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                let runs_for_global = runs.clone();
                let mentions_global = CodemodeTool::new(MENTIONS_GLOBAL, move |args, _| {
                    let runs = runs_for_global.clone();
                    async move {
                        #[derive(Deserialize)]
                        struct Call {
                            run: String,
                            query: String,
                            since: Option<u64>,
                        }
                        let Some(val) = args else {
                            return Err(HostError(
                                "mentions(query, { since }) takes a query of 1 to 500 characters"
                                    .into(),
                            ));
                        };
                        let Ok(call) = serde_json::from_value::<Call>(val) else {
                            return Err(HostError(
                                "mentions(query, { since }) takes a query of 1 to 500 characters"
                                    .into(),
                            ));
                        };
                        let qlen = call.query.chars().count();
                        if !(1..=500).contains(&qlen) {
                            return Err(HostError(
                                "mentions(query, { since }) takes a query of 1 to 500 characters"
                                    .into(),
                            ));
                        }
                        let active = runs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&call.run)
                            .cloned()
                            .ok_or_else(|| {
                                HostError("mentions() works only while its rollup runs".into())
                            })?;
                        let found =
                            call_mentions(&active.search, &call.query, call.since, &active.ctx)
                                .await
                                .map_err(|e| HostError(e.to_string()))?;
                        active
                            .mentions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(mention_read(&call.query, call.since), found.clone());
                        Ok(Some(found))
                    }
                });

                let sandbox = Arc::new(
                    CodemodeSandbox::new(SandboxOptions {
                        memory_limit_bytes: Some(RUN_MEMORY_BYTES),
                        globals: vec![mentions_global],
                        ..Default::default()
                    })
                    .map_err(|e| Error::Invalid(e.to_string()))?,
                );

                let (facts_tx, facts_rx) = watch::channel(0u64);
                let queues: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
                    Arc::new(Mutex::new(HashMap::new()));

                let entry_of = {
                    let saved = saved.clone();
                    move |key: &str| -> Result<Value> {
                        let doc = saved.value()?.to_serde()?;
                        doc.get("rollups")
                            .and_then(|r| r.get(key))
                            .cloned()
                            .ok_or_else(|| Error::Invalid(format!("No rollup \"{key}\"")))
                    }
                };

                let run_script = {
                    let sandbox = sandbox.clone();
                    let runs = runs.clone();
                    let search = search.clone();
                    let output_validator = output_validator.clone();
                    move |code: String, input: super::runner::RollupInput, ctx: Context| {
                        let sandbox = sandbox.clone();
                        let runs = runs.clone();
                        let search = search.clone();
                        let output_validator = output_validator.clone();
                        async move {
                            let token = Uuid::new_v4().to_string();
                            let active = Arc::new(ActiveRun {
                                search,
                                ctx,
                                mentions: Mutex::new(HashMap::new()),
                            });
                            runs.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(token.clone(), active.clone());
                            let result = run_rollup(
                                &sandbox,
                                &code,
                                &input,
                                &token,
                                &output_validator,
                            )
                            .await;
                            runs.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&token);
                            let answered = active
                                .mentions
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone();
                            (result, answered)
                        }
                    }
                };

                let update_now = {
                    let saved = saved.clone();
                    let catalog = catalog.clone();
                    let statuses = statuses.clone();
                    let search = search.clone();
                    let projects = projects.clone();
                    let familiar = familiar.clone();
                    let queues = queues.clone();
                    let entry_of = entry_of.clone();
                    let run_script = run_script.clone();
                    move |key: String, mode: UpdateMode, ctx: Context| {
                        let saved = saved.clone();
                        let catalog = catalog.clone();
                        let statuses = statuses.clone();
                        let search = search.clone();
                        let projects = projects.clone();
                        let familiar = familiar.clone();
                        let queues = queues.clone();
                        let entry_of = entry_of.clone();
                        let run_script = run_script.clone();
                        async move {
                            let key_lock = queues
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .entry(key.clone())
                                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                                .clone();
                            let _guard = key_lock.lock().await;
                            let entry = entry_of(&key)?;
                            let now = now_ms();
                            let facts = read_facts(
                                &catalog, &statuses, &search, &projects, &familiar, &ctx,
                            )
                            .await?;
                            let input = input_of(&facts, now);
                            let old_fp = entry.get("fingerprint").and_then(Value::as_str);
                            let old_reads: Vec<String> = entry
                                .get("reads")
                                .and_then(Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default();
                            if mode != UpdateMode::Force
                                && let Some(fp) = old_fp
                            {
                                let mut mentions_now = HashMap::new();
                                for path in &old_reads {
                                    if let Some(m) = mention_of(path)
                                        && let Ok(ans) =
                                            call_mentions(&search, &m.query, m.since, &ctx).await
                                    {
                                        mentions_now.insert(path.clone(), ans);
                                    }
                                }
                                if fp == fingerprint_of(&input, &old_reads, &mentions_now) {
                                    if mode == UpdateMode::Check {
                                        let key_c = key.clone();
                                        saved
                                            .change(&ctx, move |draft| {
                                                Box::pin(async move {
                                                    let cur = draft.value()?.to_json().to_serde()?;
                                                    if cur
                                                        .get("rollups")
                                                        .and_then(|r| r.get(&key_c))
                                                        .is_some()
                                                    {
                                                        draft.set(
                                                            &[
                                                                Segment::Key("rollups".into()),
                                                                Segment::Key(key_c.into()),
                                                                Segment::Key("checkedAt".into()),
                                                            ],
                                                            JsonValue::from_serde(json!(now)),
                                                        )?;
                                                    }
                                                    Ok(())
                                                })
                                            })
                                            .await?;
                                    }
                                    return entry_of(&key);
                                }
                            }

                            let code = entry["definition"]["code"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned();
                            let (result, answered) =
                                run_script(code.clone(), input.clone(), ctx.clone()).await;

                            let key_w = key.clone();
                            saved
                                .change(&ctx, move |draft| {
                                    Box::pin(async move {
                                        let doc = draft.value()?.to_json().to_serde()?;
                                        let Some(mut current) = doc
                                            .get("rollups")
                                            .and_then(|r| r.get(&key_w))
                                            .cloned()
                                        else {
                                            return Ok(());
                                        };
                                        if current["definition"]["code"].as_str() != Some(&code) {
                                            return Ok(());
                                        }
                                        current["checkedAt"] = json!(now);
                                        let reads: Vec<String> = match &result {
                                            PreviewResult::Ok { reads, .. } => reads.clone(),
                                            PreviewResult::Err { .. } => {
                                                EVERYTHING.iter().map(|s| (*s).to_owned()).collect()
                                            }
                                        };
                                        let fp = fingerprint_of(&input, &reads, &answered);
                                        current["reads"] = json!(reads);
                                        current["fingerprint"] = json!(fp);
                                        match &result {
                                            PreviewResult::Ok { output, .. } => {
                                                current["output"] = output.clone();
                                                current["error"] = Value::Null;
                                            }
                                            PreviewResult::Err { error } => {
                                                current["error"] = json!(error);
                                            }
                                        }
                                        draft.set(
                                            &[
                                                Segment::Key("rollups".into()),
                                                Segment::Key(key_w.into()),
                                            ],
                                            JsonValue::from_serde(current),
                                        )?;
                                        Ok(())
                                    })
                                })
                                .await?;
                            entry_of(&key)
                        }
                    }
                };

                // Watch upstream facts and run periodic tick.
                let live_catalog = catalog.clone();
                let live_statuses = statuses.clone();
                let live_search = search.clone();
                let live_projects = projects.clone();
                let live_familiar = familiar.clone();
                let live_saved = saved.clone();
                let live_update = update_now.clone();
                let live_sandbox = sandbox.clone();
                object.on_live(move |ctx| {
                    let catalog = live_catalog.clone();
                    let statuses = live_statuses.clone();
                    let search = live_search.clone();
                    let projects = live_projects.clone();
                    let familiar = live_familiar.clone();
                    let saved = live_saved.clone();
                    let update = live_update.clone();
                    let sandbox = live_sandbox.clone();
                    let facts_tx = facts_tx.clone();
                    let mut facts_rx = facts_rx.clone();
                    Box::pin(async move {
                        let mut subs = Vec::new();
                        let bump = move || {
                            facts_tx.send_modify(|n| *n = n.wrapping_add(1));
                        };
                        for (held, member_name) in [
                            (&catalog, "sessions"),
                            (&statuses, "statuses"),
                            (&search, "answers"),
                            (&search, "reads"),
                            (&projects, "projects"),
                            (&projects, "sessions"),
                            (&familiar, "session"),
                        ] {
                            if held.wait(&ctx.context).await.is_ok()
                                && let Ok(member) = held.member(member_name)
                            {
                                let bump = bump.clone();
                                subs.push(member.subscribe(
                                    move |call, _| {
                                        if matches!(call, WatchCall::Value { .. }) {
                                            bump();
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                ));
                            }
                        }

                        let mut interval =
                            tokio::time::interval(Duration::from_millis(TICK_MS));
                        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        loop {
                            tokio::select! {
                                _ = async {
                                    if let Some(sig) = ctx.context.abort_signal() {
                                        sig.cancelled().await;
                                    } else {
                                        std::future::pending::<()>().await;
                                    }
                                } => {
                                    break;
                                }
                                changed = facts_rx.changed() => {
                                    if changed.is_err() {
                                        break;
                                    }
                                    if let Ok(doc) = saved.value().and_then(|v| v.to_serde())
                                        && let Some(map) = doc.get("rollups").and_then(Value::as_object)
                                    {
                                        for (key, entry) in map {
                                            if entry["definition"]["refresh"]["kind"].as_str() == Some("auto") {
                                                let _ = update(key.clone(), UpdateMode::Skip, ctx.context.clone()).await;
                                            }
                                        }
                                    }
                                }
                                _ = interval.tick() => {
                                    let now = now_ms();
                                    if let Ok(doc) = saved.value().and_then(|v| v.to_serde())
                                        && let Some(map) = doc.get("rollups").and_then(Value::as_object)
                                    {
                                        for (key, entry) in map {
                                            let refresh = &entry["definition"]["refresh"];
                                            let kind = refresh["kind"].as_str();
                                            if kind == Some("every") {
                                                let minutes = refresh["minutes"].as_u64().unwrap_or(1);
                                                let checked_at = entry["checkedAt"].as_u64().unwrap_or(0);
                                                if now.saturating_sub(checked_at) >= minutes * 60_000 {
                                                    let _ = update(key.clone(), UpdateMode::Check, ctx.context.clone()).await;
                                                }
                                            } else if kind == Some("auto")
                                                && entry["reads"]
                                                    .as_array()
                                                    .is_some_and(|r| r.iter().any(|v| v == "now"))
                                            {
                                                let _ = update(key.clone(), UpdateMode::Skip, ctx.context.clone()).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        drop(subs);
                        sandbox.close();
                        Ok(())
                    })
                });

                let saved_source = saved.source()?;
                let rollups_state = object.follow(move |target, ctx| {
                    let saved_source = saved_source.clone();
                    Box::pin(async move {
                        let initial = saved_source.value()?.to_json().to_serde()?;
                        let arr: Vec<Value> = initial["rollups"]
                            .as_object()
                            .map(|m| m.values().cloned().collect())
                            .unwrap_or_default();
                        target.set(JsonValue::from_serde(Value::Array(arr)), &ctx.context)?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let subscription = saved_source.subscribe(
                            move |call, context| match call {
                                WatchCall::Value { value, .. } => {
                                    let val = value.to_serde()?;
                                    let arr: Vec<Value> = val["rollups"]
                                        .as_object()
                                        .map(|m| m.values().cloned().collect())
                                        .unwrap_or_default();
                                    changed.set(JsonValue::from_serde(Value::Array(arr)), context)
                                }
                                WatchCall::Failed { error } => changed.fail(error, context),
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let mut members = Members::new();
                members.state("rollups", rollups_state)?;

                let save_saved = saved.clone();
                let save_update = update_now.clone();
                members.method("save", move |args, ctx| {
                    let saved = save_saved.clone();
                    let update = save_update.clone();
                    Box::pin(async move {
                        let def = args.to_serde()?;
                        let key = def["key"]
                            .as_str()
                            .ok_or_else(|| Error::Invalid("missing rollup key".into()))?
                            .to_owned();
                        let key_s = key.clone();
                        let fresh_entry = fresh(&def);
                        saved
                            .change(&ctx.context, move |draft| {
                                Box::pin(async move {
                                    draft.set(
                                        &[
                                            Segment::Key("rollups".into()),
                                            Segment::Key(key_s.into()),
                                        ],
                                        JsonValue::from_serde(fresh_entry),
                                    )?;
                                    Ok(())
                                })
                            })
                            .await?;
                        let updated = update(key, UpdateMode::Force, ctx.context).await?;
                        Ok(JsonValue::from_serde(updated))
                    })
                })?;

                let remove_saved = saved.clone();
                let remove_entry_of = entry_of.clone();
                members.method("remove", move |args, ctx| {
                    let saved = remove_saved.clone();
                    let entry_of = remove_entry_of.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let key = v["key"]
                            .as_str()
                            .ok_or_else(|| Error::Invalid("missing rollup key".into()))?
                            .to_owned();
                        entry_of(&key)?;
                        saved
                            .change(&ctx.context, move |draft| {
                                Box::pin(async move {
                                    draft.delete(&[
                                        Segment::Key("rollups".into()),
                                        Segment::Key(key.into()),
                                    ])?;
                                    Ok(())
                                })
                            })
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let refresh_update = update_now;
                members.method("refresh", move |args, ctx| {
                    let update = refresh_update.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let key = v["key"]
                            .as_str()
                            .ok_or_else(|| Error::Invalid("missing rollup key".into()))?
                            .to_owned();
                        let updated = update(key, UpdateMode::Force, ctx.context).await?;
                        Ok(JsonValue::from_serde(updated))
                    })
                })?;

                let prev_catalog = catalog;
                let prev_statuses = statuses;
                let prev_search = search;
                let prev_projects = projects;
                let prev_familiar = familiar;
                let prev_run = run_script;
                members.method("preview", move |args, ctx| {
                    let catalog = prev_catalog.clone();
                    let statuses = prev_statuses.clone();
                    let search = prev_search.clone();
                    let projects = prev_projects.clone();
                    let familiar = prev_familiar.clone();
                    let run_script = prev_run.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let code = v["code"]
                            .as_str()
                            .ok_or_else(|| Error::Invalid("missing rollup code".into()))?
                            .to_owned();
                        let facts = read_facts(
                            &catalog,
                            &statuses,
                            &search,
                            &projects,
                            &familiar,
                            &ctx.context,
                        )
                        .await?;
                        let input = input_of(&facts, now_ms());
                        let (result, _) = run_script(code, input, ctx.context).await;
                        Ok(JsonValue::from_serde(preview_to_json(&result)))
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        host.serve(
            sidebar_contract,
            move |_, principal, object| {
                let parsed: Option<Principal> = principal
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| {
                        Error::from(ChordError::new(
                            "forbidden",
                            "rollups are the owner's and the Familiar's",
                        ))
                    })?;
                let is_owner = parsed.as_ref().is_some_and(Principal::is_owner);
                let worker_principal = match parsed {
                    Some(Principal::Worker {
                        session,
                        generation,
                    }) => Some((session, generation)),
                    _ => None,
                };
                if !is_owner && worker_principal.is_none() {
                    return Err(ChordError::new(
                        "forbidden",
                        "rollups are the owner's and the Familiar's",
                    )
                    .into());
                }
                let store = object.get(
                    store_contract.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                if is_owner {
                    let follow_store = store.clone();
                    let rollups_state = object.follow(move |target, ctx| {
                        let store = follow_store.clone();
                        Box::pin(async move {
                            store.wait(&ctx.context).await?;
                            let member = store.member("rollups")?;
                            target.set(member.value()?.to_json(), &ctx.context)?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let subscription = member.subscribe(
                                move |call, context| match call {
                                    WatchCall::Value { value, .. } => changed.set(value, context),
                                    WatchCall::Failed { error } => changed.fail(error, context),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;
                    let mut members = Members::variant("owner");
                    members.state("rollups", rollups_state)?;
                    for method_name in ["save", "remove", "refresh", "preview"] {
                        let store = store.clone();
                        members.method(method_name, move |args, ctx| {
                            let store = store.clone();
                            Box::pin(async move {
                                store.wait(&ctx.context).await?;
                                store.call(method_name, args, &ctx.context).await
                            })
                        })?;
                    }
                    return Ok(members);
                }

                let Some((session, generation)) = worker_principal else {
                    return Err(ChordError::new(
                        "forbidden",
                        "rollups are the owner's and the Familiar's",
                    )
                    .into());
                };

                let familiar = object.get(
                    familiar_contract.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                let hosts = object.get(
                    hosts_contract.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                let admit = move |hosts: &Held, familiar: &Held| -> Result<()> {
                    let workers = hosts.member("workers")?.value()?.to_json().to_serde()?;
                    let current = workers.get(&session);
                    let running = current
                        .and_then(|c| c.get("running"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let current_gen = current
                        .and_then(|c| c.get("generation"))
                        .and_then(Value::as_u64);
                    if !running || current_gen != Some(generation) {
                        return Err(ChordError::new("forbidden", "stale worker").into());
                    }
                    let fam_sess = familiar.member("session")?.value()?.to_json().to_serde()?;
                    if fam_sess.as_str() != Some(session.as_str()) {
                        return Err(ChordError::new(
                            "forbidden",
                            "only the Familiar manages rollups",
                        )
                        .into());
                    }
                    Ok(())
                };

                let mut members = Members::variant("familiar");
                let list_store = store.clone();
                let list_hosts = hosts.clone();
                let list_familiar = familiar.clone();
                let list_admit = admit.clone();
                members.method("list", move |_, ctx| {
                    let store = list_store.clone();
                    let hosts = list_hosts.clone();
                    let familiar = list_familiar.clone();
                    let admit = list_admit.clone();
                    Box::pin(async move {
                        hosts.wait(&ctx.context).await?;
                        familiar.wait(&ctx.context).await?;
                        store.wait(&ctx.context).await?;
                        admit(&hosts, &familiar)?;
                        Ok(store.member("rollups")?.value()?.to_json())
                    })
                })?;

                for method_name in ["save", "remove", "refresh", "preview"] {
                    let store = store.clone();
                    let hosts = hosts.clone();
                    let familiar = familiar.clone();
                    let admit = admit.clone();
                    members.method(method_name, move |args, ctx| {
                        let store = store.clone();
                        let hosts = hosts.clone();
                        let familiar = familiar.clone();
                        let admit = admit.clone();
                        Box::pin(async move {
                            hosts.wait(&ctx.context).await?;
                            familiar.wait(&ctx.context).await?;
                            store.wait(&ctx.context).await?;
                            admit(&hosts, &familiar)?;
                            store.call(method_name, args, &ctx.context).await
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    })
}
