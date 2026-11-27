//! `powershell` built-in plugin: offers Durable's deferred `powershell` tool on Windows
//! environments when `bash` is enabled (`apps/p2/plugins/powershell/worker.ts`).

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use pi_rs_ai::Tool;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
};
use pi_rs_durable::{
    ConversationId, ROOT_CONVERSATION_ID,
    documents::resolve_address,
    harness::{AGENT_DOC, Extension, Harness, ToolRegistration, configure, define_extension},
    session::{Error as DurableError, Observer},
    tools::{PowerShellToolInput, PowerShellToolOptions, TailShellOutput, create_powershell_tool},
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::{
    api::v1::{FacetKind, PluginRegistry, worker::worker_of},
    plugins::subagents::contract_worker::subagents_doc,
    runtime::{location::Location, worker_environment::locate_conversation},
};

const RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub const POWERSHELL_EXTENSION: &str = "p2-powershell";
pub const POWERSHELL_TOOL: &str = "powershell";

/// Whether `agent_state` enables `tool_name`.
pub fn enables_tool(agent_state: Option<&Value>, tool_name: &str) -> bool {
    let Some(tools) = agent_state.and_then(|s| s.get("tools")) else {
        return true;
    };
    if let Some(arr) = tools.as_array() {
        return arr.iter().any(|v| v.as_str() == Some(tool_name));
    }
    if let Some(removed) = tools.get("remove").and_then(Value::as_array) {
        return !removed.iter().any(|v| v.as_str() == Some(tool_name));
    }
    true
}

/// Whether `agent_state` currently offers the deferred `powershell` tool to the model.
pub fn offers_powershell(agent_state: Option<&Value>) -> bool {
    if !enables_tool(agent_state, POWERSHELL_TOOL) {
        return false;
    }
    let Some(model_tools) = agent_state.and_then(|s| s.get("modelTools")) else {
        return false;
    };
    if let Some(arr) = model_tools.as_array() {
        return arr.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL));
    }
    model_tools
        .get("add")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL)))
}

/// Enables or disables `powershell` in `agent_state` while preserving all other tool choices.
pub fn set_powershell(agent_state: &mut Value, on: bool) {
    let Some(obj) = agent_state.as_object_mut() else {
        return;
    };
    let without = |arr: &[Value]| -> Vec<Value> {
        arr.iter()
            .filter(|v| v.as_str() != Some(POWERSHELL_TOOL))
            .cloned()
            .collect()
    };

    if let Some(tools) = obj.get("tools").cloned() {
        if let Some(arr) = tools.as_array() {
            let mut next = without(arr);
            if on {
                next.push(Value::String(POWERSHELL_TOOL.into()));
            }
            obj.insert("tools".into(), Value::Array(next));
        } else if on
            && let Some(removed) = tools.get("remove").and_then(Value::as_array)
            && removed.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL))
        {
            obj.insert("tools".into(), json!({ "remove": without(removed) }));
        }
    }

    if let Some(model_tools) = obj.get("modelTools").cloned()
        && let Some(arr) = model_tools.as_array()
    {
        let mut next = without(arr);
        if on {
            next.push(Value::String(POWERSHELL_TOOL.into()));
        }
        obj.insert("modelTools".into(), Value::Array(next));
        return;
    }

    let mut add = obj
        .get("modelTools")
        .and_then(|mt| mt.get("add"))
        .and_then(Value::as_array)
        .map(|arr| without(arr))
        .unwrap_or_default();
    if on {
        add.push(Value::String(POWERSHELL_TOOL.into()));
    }
    let remove = obj
        .get("modelTools")
        .and_then(|mt| mt.get("remove"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut next_mt = serde_json::Map::new();
    if !remove.is_empty() {
        next_mt.insert("remove".into(), Value::Array(remove));
    }
    if !add.is_empty() {
        next_mt.insert("add".into(), Value::Array(add));
    }
    obj.insert("modelTools".into(), Value::Object(next_mt));
}

/// Builds the Durable `p2-powershell` extension wrapping Durable's deferred `powershell` tool.
pub fn create_powershell_extension() -> Extension {
    let tool_impl = Arc::new(create_powershell_tool(PowerShellToolOptions::default()));
    let spec = tool_impl.spec();
    let registration = ToolRegistration {
        declaration: Tool {
            name: spec.name.to_owned(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: spec.structured_output_schema.clone(),
        output_limits: spec.output_limits,
        replay_safe: false,
        callers: None,
        deferred: true,
        execute: Arc::new(move |args, invocation, context| {
            let tool_impl = tool_impl.clone();
            Box::pin(async move {
                let parsed: PowerShellToolInput = serde_json::from_value(args)?;
                let env = invocation.env(context).await?;
                let mut shell_out = TailShellOutput::new(None);
                let result = tool_impl
                    .execute(parsed, env.as_deref(), &mut shell_out, context)
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(pi_rs_durable::tools::ToolResult {
                    output: result.output,
                    structured_output: result.structured_output,
                    is_error: result.is_error,
                    details: result.details.map(serde_json::to_value).transpose()?,
                    diagnostics: result.diagnostics,
                    control: result.control,
                    usage: result.usage,
                }
                .into())
            })
        }),
    };

    define_extension(Extension {
        name: POWERSHELL_EXTENSION.into(),
        tools: vec![registration],
        ..Default::default()
    })
}

/// Evaluates Windows platform + `bash` tool availability across all conversations in `durable`,
/// commits `set_powershell` updates when needed, and publishes the per-conversation map to `offered_target`.
/// Returns `Ok(true)` if any conversation's environment platform could not be determined (`None`).
pub async fn sync_powershell_offered<F, Fut>(
    durable: &Harness,
    mut probe_windows: F,
    offered_target: &Source,
    ctx: &Context,
) -> Result<bool>
where
    F: FnMut(ConversationId) -> Fut,
    Fut: Future<Output = Option<bool>>,
{
    let mut ids = vec![ROOT_CONVERSATION_ID];
    let sub_doc = subagents_doc();
    let sub_addr = resolve_address(&sub_doc, Some(ROOT_CONVERSATION_ID), None)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if let Ok(Some(sub_snap)) = durable.session.snapshot(&sub_doc, &sub_addr, ctx).await {
        for section in ["agents", "oneShots"] {
            if let Some(map) = sub_snap.get(section).and_then(Value::as_object) {
                for entry in map.values() {
                    if let Some(conv_id) = entry.get("conversationId").and_then(Value::as_u64)
                        && !ids.contains(&conv_id)
                    {
                        ids.push(conv_id);
                    }
                }
            }
        }
    }

    let mut offered = Map::new();
    let mut changes: Vec<(ConversationId, Value, bool)> = Vec::new();
    let mut unknown = false;

    for id in ids {
        let addr = resolve_address(&AGENT_DOC, Some(id), None)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let state = durable
            .session
            .snapshot(&AGENT_DOC, &addr, ctx)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map(|obj| Value::Object((*obj).clone()));
        let windows = probe_windows(id).await;
        if windows.is_none() {
            unknown = true;
        }
        let currently = offers_powershell(state.as_ref());
        let on = match windows {
            None => currently,
            Some(is_win) => is_win && enables_tool(state.as_ref(), "bash"),
        };
        offered.insert(id.to_string(), Value::Bool(on));
        if on != currently {
            let mut next = state.unwrap_or_else(|| json!({}));
            set_powershell(&mut next, on);
            changes.push((id, next, on));
        }
    }

    if !changes.is_empty() {
        durable
            .commit(
                async move |tx| {
                    for (id, next, _) in &changes {
                        let mut delta = Map::new();
                        if let Some(tools) = next.get("tools") {
                            delta.insert("tools".into(), as_named_registrations(tools));
                        }
                        if let Some(model_tools) = next.get("modelTools") {
                            delta.insert("modelTools".into(), as_named_registrations(model_tools));
                        }
                        configure(tx, *id, &delta).await?;
                    }
                    Ok(())
                },
                ctx,
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
    }

    offered_target.set(JsonValue::from_serde(Value::Object(offered)), ctx)?;
    Ok(unknown)
}

fn as_named_registrations(value: &Value) -> Value {
    let wrap = |arr: &[Value]| -> Value {
        Value::Array(
            arr.iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(|name| json!({ "name": name }))
                        .or_else(|| {
                            item.get("name")
                                .and_then(Value::as_str)
                                .map(|name| json!({ "name": name }))
                        })
                })
                .collect(),
        )
    };
    if let Some(arr) = value.as_array() {
        return wrap(arr);
    }
    if let Some(obj) = value.as_object() {
        let mut out = Map::new();
        for field in ["add", "remove"] {
            if let Some(arr) = obj.get(field).and_then(Value::as_array) {
                out.insert(field.into(), wrap(arr));
            }
        }
        return Value::Object(out);
    }
    value.clone()
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let mut member_defs = indexmap::IndexMap::new();
    member_defs.insert(
        "offered".into(),
        pi_rs_chord::contracts::Member::State(JsonValue::from_serde(json!({ "type": "object" }))),
    );
    let contract = Arc::new(Contract::local(
        "p2.powershell",
        1,
        Vec::new(),
        member_defs,
    )?);
    registry.register(
        "powershell",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                move |_, _, object| {
                    let offered = object.writable(JsonValue::from_serde(json!({})))?;
                    if let Ok(worker) = worker_of(&object.host()) {
                        let extension = create_powershell_extension();
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;

                        let probe_worker = worker.clone();
                        let offered_live = offered.clone();
                        object.on_live(move |ctx| {
                            let worker = probe_worker.clone();
                            let offered_live = offered_live.clone();
                            Box::pin(async move {
                                let ready = async {
                                    loop {
                                        if let Ok(parts) = worker.harness() {
                                            return parts;
                                        }
                                        tokio::time::sleep(Duration::from_millis(25)).await;
                                    }
                                };
                                let Ok(parts) = ctx.context.await_with(ready).await else {
                                    return Ok(());
                                };
                                let systems_cache = Arc::new(Mutex::new(HashMap::<String, bool>::new()));
                                let notify = Arc::new(tokio::sync::Notify::new());
                                let notify_commit = notify.clone();
                                let sub_commit = parts
                                    .durable
                                    .session
                                    .observe_commits(Observer::new(
                                        move |publication: &pi_rs_durable::CommitPublication, _| {
                                            if publication.changes.iter().any(|change| {
                                                matches!(
                                                    change,
                                                    pi_rs_durable::CommitChange::Document(
                                                        pi_rs_durable::DocumentCommitChange::Change {
                                                            record,
                                                            ..
                                                        } | pi_rs_durable::DocumentCommitChange::Retire {
                                                            record,
                                                            ..
                                                        }
                                                    ) if record.kind == "pi.agent"
                                                        || record.kind == "p2.subagents"
                                                )
                                            }) {
                                                notify_commit.notify_one();
                                            }
                                            Ok(())
                                        },
                                        |_| {},
                                    ))
                                    .map_err(|e| Error::Invalid(e.to_string()))?;
                                let mut loc_rx = worker.location.subscribe();

                                loop {
                                    if ctx.context.is_aborted() {
                                        break;
                                    }
                                    let systems = systems_cache.clone();
                                    let envs = parts.environments.clone();
                                    let durable = parts.durable.clone();
                                    let c = ctx.context.clone();
                                    let unknown = sync_powershell_offered(
                                        &durable,
                                        move |conv_id| {
                                            let systems = systems.clone();
                                            let envs = envs.clone();
                                            let c = c.clone();
                                            async move {
                                                let location: Location = {
                                                    let guard = envs.lock().await;
                                                    locate_conversation(
                                                        Some(&guard),
                                                        conv_id,
                                                        &c,
                                                        None,
                                                    )
                                                    .await
                                                    .ok()?
                                                };
                                                let Some(ref env_name) = location.env else {
                                                    return Some(cfg!(windows));
                                                };
                                                if let Some(known) =
                                                    systems.lock().await.get(env_name).copied()
                                                {
                                                    return Some(known);
                                                }
                                                let win = envs
                                                    .lock()
                                                    .await
                                                    .platform_is_windows(&location)?;
                                                systems.lock().await.insert(env_name.clone(), win);
                                                Some(win)
                                            }
                                        },
                                        &offered_live,
                                        &ctx.context,
                                    )
                                    .await
                                    .unwrap_or(true);

                                    let wait = async {
                                        if unknown {
                                            tokio::select! {
                                                _ = notify.notified() => {}
                                                _ = loc_rx.recv() => {}
                                                _ = tokio::time::sleep(RETRY_INTERVAL) => {}
                                            }
                                        } else {
                                            tokio::select! {
                                                _ = notify.notified() => {}
                                                _ = loc_rx.recv() => {}
                                            }
                                        }
                                    };
                                    if ctx.context.await_with(wait).await.is_err() {
                                        break;
                                    }
                                }
                                drop(sub_commit);
                                Ok(())
                            })
                        });

                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    let mut members = Members::new();
                    members.state("offered", offered)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
