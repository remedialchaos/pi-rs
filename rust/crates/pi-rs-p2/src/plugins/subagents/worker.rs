//! Worker facet for the `subagents` plugin (`p2.subagent-control`, tools, reporting, and locations).

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use pi_rs_durable::{
    ConversationId, EntryId, EntryQuery, ROOT_CONVERSATION_ID,
    documents::resolve_address,
    harness::{Harness, LIVE_DOC, TaskGraph},
    session::{Observer, Session},
    storage_types::{CommitChange, CommitPublication, DocumentCommitChange},
};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    api::v1::Principal,
    hosts::worker::{
        ConversationLocations, PlacedConversations,
        durable::{LockedTool, ToolAccess},
        worker_of,
    },
    runtime::{
        location::{Location, LocationInput},
        worker_environment::{
            ConversationPlacement, PlacementFuture, PlacementReader, resolve_directory,
        },
    },
};

fn durable_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

use super::{
    contract::{Child, ChildFailure, subagent_control_contract, subagents_contract},
    contract_worker::{SubagentState, ToolScope, choosable_tools, subagents_doc},
    tools::{
        create_subagent_extension, enable_script_tools, own_location, place_subagents,
        set_archived, spawn_environments, team_extension,
    },
};

const READ_ONLY: &str = "One-shot agents are read-only";
const NEWEST_ENTRIES: usize = 20;

#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub last_entry_id: Option<EntryId>,
    pub updated_at: Option<i64>,
    pub failure: Option<ChildFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureSource {
    Read,
    Cached(Option<ChildFailure>),
}

/// Decides whether to read `LiveDoc` or reuse the cached failure from the previous report.
pub fn failure_source(known: Option<&Seen>, newest: Option<EntryId>, ended: bool) -> FailureSource {
    match known {
        Some(prev) if !ended && newest == prev.last_entry_id => {
            FailureSource::Cached(prev.failure.clone())
        }
        _ => FailureSource::Read,
    }
}

fn failure_of(live: Option<&serde_json::Map<String, Value>>) -> Option<ChildFailure> {
    let failure = live?.get("runFailure")?.as_object()?;
    let reason = failure.get("reason")?.as_str()?.to_owned();
    let detail = failure
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(ChildFailure { reason, detail })
}

async fn newest_of(
    session: &Session,
    conversation_id: ConversationId,
    known: Option<&Seen>,
    ctx: &Context,
) -> Result<(Option<EntryId>, Option<i64>)> {
    if session
        .conversation(conversation_id, ctx)
        .await
        .map_err(durable_error)?
        .is_none()
    {
        return Ok((None, None));
    }
    let since = known.and_then(|k| k.last_entry_id);
    let filter = EntryQuery {
        conversation_id,
        min_entry_id: since,
        ..Default::default()
    };
    let page = session
        .scan_entries(&filter, NEWEST_ENTRIES, None, ctx)
        .await
        .map_err(durable_error)?;
    let Some(newest) = page.items.first() else {
        return Ok((since, known.and_then(|k| k.updated_at)));
    };
    if Some(newest.id) == since {
        return Ok((since, known.and_then(|k| k.updated_at)));
    }
    let updated_at = page
        .items
        .iter()
        .find_map(|entry| {
            entry
                .model
                .as_deref()
                .and_then(|m| m.last())
                .and_then(|msg| msg.get("timestamp").and_then(Value::as_i64))
        })
        .or_else(|| known.and_then(|k| k.updated_at));
    Ok((Some(newest.id), updated_at))
}

async fn details_of(
    harness: &Harness,
    id: ConversationId,
    seen: &mut BTreeMap<ConversationId, Seen>,
    endings: &mut BTreeMap<ConversationId, usize>,
    ctx: &Context,
) -> Result<(u64, Option<i64>, Option<ChildFailure>)> {
    if harness
        .session
        .conversation(id, ctx)
        .await
        .map_err(durable_error)?
        .is_none()
    {
        return Ok((0, None, None));
    }
    let known = seen.get(&id).cloned();
    let (last_entry_id, updated_at) = newest_of(&harness.session, id, known.as_ref(), ctx).await?;
    let ended = endings.get(&id).copied();
    let source = failure_source(known.as_ref(), last_entry_id, ended.is_some());
    let failure = match source {
        FailureSource::Cached(f) => f,
        FailureSource::Read => {
            let live_addr = resolve_address(&LIVE_DOC, Some(id), None).map_err(durable_error)?;
            let live = harness
                .session
                .snapshot(&LIVE_DOC, &live_addr, ctx)
                .await
                .map_err(durable_error)?;
            let f = failure_of(live.as_deref());
            if endings.get(&id).copied() == ended {
                endings.remove(&id);
            }
            f
        }
    };
    seen.insert(
        id,
        Seen {
            last_entry_id,
            updated_at,
            failure: failure.clone(),
        },
    );
    Ok((last_entry_id.unwrap_or(0), updated_at, failure))
}

pub async fn children_of(
    harness: &Harness,
    seen: &mut BTreeMap<ConversationId, Seen>,
    endings: &mut BTreeMap<ConversationId, usize>,
    ctx: &Context,
) -> Result<Vec<Child>> {
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None).map_err(durable_error)?;
    let state: SubagentState = match harness
        .session
        .snapshot(&doc, &addr, ctx)
        .await
        .map_err(durable_error)?
    {
        Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
        None => SubagentState::default(),
    };
    let listed: BTreeSet<ConversationId> =
        state.agents.values().map(|a| a.conversation_id).collect();
    seen.retain(|id, _| listed.contains(id));
    endings.retain(|id, _| listed.contains(id));

    let mut children = Vec::with_capacity(state.agents.len());
    for (name, agent) in &state.agents {
        let (last_entry_id, updated_at, failure) =
            details_of(harness, agent.conversation_id, seen, endings, ctx).await?;
        children.push(Child {
            name: name.clone(),
            kind: None,
            call_id: None,
            archived: agent.archived,
            conversation_id: agent.conversation_id,
            model: agent.model.clone(),
            thinking_level: agent.thinking_level.clone(),
            tools: agent.tools.clone(),
            location: agent.location.clone(),
            last_entry_id,
            updated_at,
            failure,
        });
    }
    if let Some(one_shots) = &state.one_shots {
        for (call_id, shot) in one_shots {
            children.push(Child {
                name: shot.label.clone(),
                kind: Some("agent".into()),
                call_id: Some(call_id.clone()),
                archived: None,
                conversation_id: shot.conversation_id,
                model: shot.model.clone(),
                thinking_level: shot.thinking_level.clone(),
                tools: shot.tools.clone(),
                location: None,
                last_entry_id: 0,
                updated_at: None,
                failure: None,
            });
        }
    }
    Ok(children)
}

pub fn working_children(
    graph: &TaskGraph,
    seen: &BTreeMap<ConversationId, Seen>,
) -> BTreeSet<ConversationId> {
    graph
        .tasks
        .values()
        .filter(|node| !node.background && seen.contains_key(&node.conversation_id))
        .map(|node| node.conversation_id)
        .collect()
}

struct SubagentPlacement;

impl ConversationPlacement for SubagentPlacement {
    fn own<'a>(
        &'a self,
        read: &'a dyn PlacementReader,
        id: ConversationId,
        ctx: &'a Context,
    ) -> PlacementFuture<'a, Option<Location>> {
        Box::pin(async move {
            if id == ROOT_CONVERSATION_ID {
                return Ok(None);
            }
            let doc = subagents_doc();
            let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None).map_err(|e| {
                crate::runtime::worker_environment::WorkerEnvironmentError::Placement(Box::new(e))
            })?;
            if let Some(raw) = read.snapshot(&doc, &addr, ctx).await? {
                let state: SubagentState = serde_json::from_value(Value::Object((*raw).clone()))
                    .map_err(|e| {
                        crate::runtime::worker_environment::WorkerEnvironmentError::Placement(
                            Box::new(e),
                        )
                    })?;
                if let Some(loc) = state
                    .agents
                    .values()
                    .find(|a| a.conversation_id == id)
                    .and_then(|a| a.location.clone())
                {
                    return Ok(Some(loc));
                }
            }
            let inherited = super::contract_worker::inherited_location_doc();
            let inh_addr = resolve_address(&inherited, Some(id), None).map_err(|e| {
                crate::runtime::worker_environment::WorkerEnvironmentError::Placement(Box::new(e))
            })?;
            if let Some(raw) = read.snapshot(&inherited, &inh_addr, ctx).await? {
                let loc: Option<Location> = raw
                    .get("location")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| {
                        crate::runtime::worker_environment::WorkerEnvironmentError::Placement(
                            Box::new(e),
                        )
                    })?;
                return Ok(loc);
            }
            Ok(None)
        })
    }
}

/// Builds the worker facet for the `subagents` plugin.
pub fn worker_facet() -> Result<Facet> {
    let subagents = Arc::new(subagents_contract()?);
    let control = Arc::new(subagent_control_contract()?);

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let process = worker_of(&host_handle)?;

        *process
            .registries
            .conversation_locations
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ConversationLocations {
            own: Arc::new(SubagentPlacement),
            place: Arc::new(|harness, session_loc, init, ctx| {
                Box::pin(async move {
                    let recorded = init.values.get("subagentLocations");
                    let placed = place_subagents(
                        &harness,
                        spawn_environments(recorded, session_loc.env),
                        &ctx,
                    )
                    .await
                    .map_err(durable_error)?;
                    Ok(PlacedConversations {
                        followers: placed.followers,
                        own: placed.own,
                    })
                })
            }),
        }));

        let access_map = Arc::new(move || {
            let mut map = IndexMap::new();
            map.insert("subagent".into(), ToolScope::Main);
            map.insert("team".into(), ToolScope::Subagent);
            map
        });

        let loc_proc = process.clone();
        let locator = super::tools::SubagentLocator {
            locate: Arc::new(move |conv_id, ctx| {
                let proc = loc_proc.clone();
                Box::pin(async move {
                    let parts = proc.harness()?;
                    let session = &parts.durable.session;
                    if let Some(own) = own_location(session, conv_id, &ctx)
                        .await
                        .map_err(durable_error)?
                    {
                        return Ok(own);
                    }
                    Ok(proc.location.value())
                })
            }),
            sandbox: Arc::new({
                let proc = process.clone();
                move |loc| {
                    let proc = proc.clone();
                    Box::pin(async move {
                        let parts = proc.harness()?;
                        let envs = parts.environments.lock().await;
                        envs.sandbox(loc.env.as_deref()).map_err(durable_error)
                    })
                }
            }),
            directory: Arc::new({
                let proc = process.clone();
                move |target: LocationInput, from: Location, ctx: Context| {
                    let proc = proc.clone();
                    Box::pin(async move {
                        let Ok(parts) = proc.harness() else {
                            return Ok(Err("The session's Harness is not open".into()));
                        };
                        let mut envs = parts.environments.lock().await;
                        match resolve_directory(&mut envs, &target, &from, &ctx).await {
                            Ok(loc) => Ok(Ok(loc)),
                            Err(err) => Ok(Err(err.to_string())),
                        }
                    })
                }
            }),
        };

        let models_proc = process.clone();
        let subagent_ext = create_subagent_extension(
            Arc::new(move || Ok(models_proc.registries.models.clone())),
            locator,
            access_map.clone(),
        );
        let mut tool_access = IndexMap::new();
        tool_access.insert(
            "subagent".into(),
            ToolAccess {
                main_only: true,
                locked: LockedTool::Pass,
            },
        );
        process
            .registries
            .extensions
            .install_with_access(subagent_ext, &tool_access)?;
        process
            .registries
            .extensions
            .install_with_access(team_extension(), &IndexMap::new())?;

        // Update subagent registry entry when a subagent conversation is configured.
        let cfg_proc = process.clone();
        let cfg_access = access_map.clone();
        process
            .registries
            .conversation_configured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                "subagents".into(),
                Arc::new(move |conversation, ctx| {
                    let proc = cfg_proc.clone();
                    let access = cfg_access.clone();
                    Box::pin(async move {
                        if conversation.id == ROOT_CONVERSATION_ID {
                            return Ok(());
                        }
                        let agent = conversation.agent(&ctx).await.map_err(durable_error)?;
                        let parts = proc.harness()?;
                        let tool_scope = access();
                        let choosable: Vec<String> = choosable_tools(&agent, &tool_scope)
                            .into_iter()
                            .map(|t| t.declaration.name)
                            .collect();
                        let doc = subagents_doc();
                        let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)
                            .map_err(durable_error)?;
                        parts
                            .durable
                            .commit(
                                async |tx| {
                                    let raw = tx.doc(&doc, &addr, None).await?;
                                    let mut state: SubagentState =
                                        serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                                    if let Some(child) = state
                                        .agents
                                        .values_mut()
                                        .find(|e| e.conversation_id == conversation.id)
                                    {
                                        if let Some(m) = agent.state.get("model")
                                            && let (Some(prov), Some(mid)) = (
                                                m.get("provider").and_then(Value::as_str),
                                                m.get("modelId").and_then(Value::as_str),
                                            )
                                        {
                                            child.model = format!("{prov}/{mid}");
                                        }
                                        if let Some(tl) =
                                            agent.state.get("thinkingLevel").and_then(Value::as_str)
                                        {
                                            child.thinking_level = tl.to_owned();
                                        }
                                        child.tools = choosable;
                                        raw.set(
                                            &[],
                                            JsonValue::from_serde(serde_json::to_value(&state)?),
                                        )?;
                                    }
                                    Ok(())
                                },
                                &ctx,
                            )
                            .await
                            .map_err(durable_error)?;
                        Ok(())
                    })
                }),
            );

        // Mark one-shot agent conversations read-only.
        let ro_proc = process.clone();
        process
            .registries
            .read_only
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                "subagents".into(),
                Arc::new(move |conversation_id, ctx| {
                    let proc = ro_proc.clone();
                    Box::pin(async move {
                        let parts = proc.harness()?;
                        let doc = subagents_doc();
                        let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)
                            .map_err(durable_error)?;
                        let Some(raw) = parts
                            .durable
                            .session
                            .snapshot(&doc, &addr, &ctx)
                            .await
                            .map_err(durable_error)?
                        else {
                            return Ok(None);
                        };
                        let state: SubagentState =
                            serde_json::from_value(Value::Object((*raw).clone()))?;
                        let is_one_shot = state.one_shots.as_ref().is_some_and(|m| {
                            m.values().any(|a| a.conversation_id == conversation_id)
                        });
                        Ok(is_one_shot.then(|| READ_ONLY.to_owned()))
                    })
                }),
            );

        let report_lock = Arc::new(AsyncMutex::new(()));
        let seen_state = Arc::new(AsyncMutex::new(BTreeMap::<ConversationId, Seen>::new()));
        let endings_state = Arc::new(AsyncMutex::new(BTreeMap::<ConversationId, usize>::new()));
        let one_shots_cache = Arc::new(AsyncMutex::new(HashSet::<ConversationId>::new()));

        let root_hold = host_handle.root();
        let held_feed = root_hold.hold(subagents.clone(), JsonValue::Object(IndexMap::new()))?;

        // Serve SubagentControl (`p2.subagent-control`).
        let ctrl_proc = process.clone();
        let ctrl_feed = held_feed.clone();
        let ctrl_lock = report_lock.clone();
        let ctrl_seen = seen_state.clone();
        let ctrl_endings = endings_state.clone();
        host.serve(
            control.clone(),
            move |params, principal_raw, _self_| {
                let session = params
                    .to_serde()?
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("missing session".into()))?
                    .to_owned();
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => {
                        return Err(ChordError::new("forbidden", "participants only").into());
                    }
                };
                if !principal.can_send(&session) {
                    return Err(ChordError::new("forbidden", "participants only").into());
                }

                let proc = ctrl_proc.clone();
                let feed = ctrl_feed.clone();
                let lock = ctrl_lock.clone();
                let seen = ctrl_seen.clone();
                let endings = ctrl_endings.clone();

                let mut members = Members::variant("participant");
                members.method("archive", move |args, ctx| {
                    let proc = proc.clone();
                    let feed = feed.clone();
                    let lock = lock.clone();
                    let seen = seen.clone();
                    let endings = endings.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let conversation = val
                            .get("conversation")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| Error::Invalid("missing conversation".into()))?;
                        let archived = val
                            .get("archived")
                            .and_then(Value::as_bool)
                            .ok_or_else(|| Error::Invalid("missing archived".into()))?;

                        proc.call(async {
                            let parts = proc.harness()?;
                            let doc = subagents_doc();
                            let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)
                                .map_err(durable_error)?;
                            let state: SubagentState = match parts
                                .durable
                                .session
                                .snapshot(&doc, &addr, &ctx.context)
                                .await
                                .map_err(durable_error)?
                            {
                                Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
                                None => SubagentState::default(),
                            };
                            if state.one_shots.as_ref().is_some_and(|m| {
                                m.values().any(|a| a.conversation_id == conversation)
                            }) {
                                return Err(ChordError::new("forbidden", READ_ONLY).into());
                            }
                            if !state
                                .agents
                                .values()
                                .any(|a| a.conversation_id == conversation)
                            {
                                return Err(Error::Invalid("Unknown conversation".into()));
                            }
                            set_archived(
                                &parts.durable,
                                ROOT_CONVERSATION_ID,
                                conversation,
                                archived,
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?;

                            let _guard = lock.lock().await;
                            let mut s = seen.lock().await;
                            let mut e = endings.lock().await;
                            let children =
                                children_of(&parts.durable, &mut s, &mut e, &ctx.context).await?;
                            feed.call(
                                "report",
                                JsonValue::from_serde(json!({ "children": children })),
                                &ctx.context,
                            )
                            .await?;
                            Ok(JsonValue::Null)
                        })
                        .await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        // Reporter & script-tools sync loop.
        let live_proc = process;
        let live_feed = held_feed;
        let lock = report_lock;
        let seen = seen_state;
        let endings = endings_state;
        let one_shots = one_shots_cache;
        tokio::spawn(async move {
            let _keep_root = root_hold;
            let ctx = Context::background();
            let parts = loop {
                if live_proc.stopping() {
                    return;
                }
                if let Ok(parts) = live_proc.harness() {
                    break parts;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };

            let snap = live_proc.registries.extensions.snapshot();
            let script_names: Vec<String> = snap
                .tools()
                .filter(|(_, t)| {
                    t.callers
                        .as_ref()
                        .is_some_and(|c| !c.iter().any(|x| x == "model"))
                })
                .map(|(_, t)| t.declaration.name.clone())
                .collect();
            if !script_names.is_empty() {
                let _ = enable_script_tools(&parts.durable, &script_names, &ctx).await;
            }

            let notify = Arc::new(tokio::sync::Notify::new());
            let commit_notify = notify.clone();
            let commit_one_shots = one_shots.clone();
            let Ok(_commit_sub) = parts.durable.session.observe_commits(Observer::new(
                move |commit: &CommitPublication, _| {
                    let one_shots_guard = commit_one_shots.try_lock();
                    let relevant = commit.changes.iter().any(|change| match change {
                        CommitChange::Document(doc) => match doc {
                            DocumentCommitChange::Change { record, .. }
                            | DocumentCommitChange::Retire { record, .. } => {
                                record.kind == "p2.subagents"
                            }
                        },
                        CommitChange::Entry { value } => {
                            value.conversation_id != ROOT_CONVERSATION_ID
                                && one_shots_guard
                                    .as_ref()
                                    .is_ok_and(|s| !s.contains(&value.conversation_id))
                        }
                        _ => false,
                    });
                    if relevant {
                        commit_notify.notify_one();
                    }
                    Ok(())
                },
                |_| {},
            )) else {
                return;
            };

            let working = Arc::new(std::sync::Mutex::new(BTreeSet::<ConversationId>::new()));
            let task_notify = notify.clone();
            let task_seen = seen.clone();
            let task_endings = endings.clone();
            let task_working = working.clone();
            let Ok(_task_sub) = parts
                .durable
                .observe_task_graph(
                    Observer::new(
                        move |call: &WatchCall<Arc<TaskGraph>>, _| {
                            if let WatchCall::Value { value, .. } = call
                                && let Ok(s) = task_seen.try_lock()
                            {
                                let current = working_children(value, &s);
                                let mut prev =
                                    task_working.lock().unwrap_or_else(|e| e.into_inner());
                                let ended: Vec<_> = prev.difference(&current).copied().collect();
                                *prev = current;
                                if !ended.is_empty()
                                    && let Ok(mut e) = task_endings.try_lock()
                                {
                                    for id in ended {
                                        *e.entry(id).or_insert(0) += 1;
                                    }
                                    task_notify.notify_one();
                                }
                            }
                            Ok(())
                        },
                        |_| {},
                    ),
                    &ctx,
                )
                .await
            else {
                return;
            };

            while !live_proc.stopping() {
                let res = async {
                    let _guard = lock.lock().await;
                    let mut s = seen.lock().await;
                    let mut e = endings.lock().await;
                    let children = children_of(&parts.durable, &mut s, &mut e, &ctx).await?;
                    {
                        let mut os = one_shots.lock().await;
                        for child in &children {
                            if child.kind.as_deref() == Some("agent") {
                                os.insert(child.conversation_id);
                            }
                        }
                    }
                    live_feed
                        .call(
                            "report",
                            JsonValue::from_serde(json!({ "children": children })),
                            &ctx,
                        )
                        .await?;
                    Ok::<_, Error>(())
                }
                .await;
                if let Err(err) = res {
                    if live_proc.stopping() {
                        return;
                    }
                    eprintln!("p2 subagents: report failed, retrying: {err}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                notify.notified().await;
            }
        });

        Ok(())
    }))
}
