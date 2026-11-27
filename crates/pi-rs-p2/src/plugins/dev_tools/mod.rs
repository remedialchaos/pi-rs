//! `dev-tools` built-in plugin: exposes `p2.plugin-reload` to session workers and
//! contributes the `p2-dev-tools` extension (`reload_plugins` + `p2-development` section).

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    remote::RemotePeer,
    sync::SyncFuture,
};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID,
    harness::{Extension, ToolRegistration, define_extension, section},
    session::Error as DurableError,
    tools::ToolResult,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    api::v1::{
        FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::service_of,
        worker::worker_of,
    },
    runtime::main_conversation::is_top_level,
};

/// Native reload callback service key registered by the development host when reload is available.
pub const PLUGIN_RELOAD_SERVICE: &str = "p2.plugin-reload-handler";

pub type ReloadHandler = dyn Fn(Option<Vec<String>>) -> SyncFuture<JsonValue> + Send + Sync;

#[derive(Deserialize)]
struct ReloadArgs {
    plugins: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PluginOutcomeEntry {
    pub plugin: String,
    pub outcome: String,
    #[serde(default)]
    pub errors: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PluginReloadResult {
    #[serde(default)]
    pub refused: Option<String>,
    #[serde(default)]
    pub plugins: Vec<PluginOutcomeEntry>,
    #[serde(default)]
    pub logs: Vec<String>,
}

pub type ReloadFn =
    Arc<dyn Fn(Option<Vec<String>>, Context) -> SyncFuture<PluginReloadResult> + Send + Sync>;
pub type OnCheckoutFn = Arc<dyn Fn() -> bool + Send + Sync>;

fn result_text(result: &PluginReloadResult) -> String {
    if let Some(refused) = &result.refused {
        return format!("Nothing reloaded: {refused}");
    }
    let mut lines: Vec<String> = if result.plugins.is_empty() {
        vec!["No plugin changed since the last reload.".into()]
    } else {
        result
            .plugins
            .iter()
            .flat_map(|entry| {
                let mut rows = vec![format!("{}: {}", entry.plugin, entry.outcome)];
                if let Some(errors) = &entry.errors {
                    for err in errors {
                        rows.push(format!("  {err}"));
                    }
                }
                rows
            })
            .collect()
    };
    if !result.logs.is_empty() {
        lines.push("Errors logged since:".into());
        for line in &result.logs {
            lines.push(format!("  {line}"));
        }
    }
    lines.join("\n")
}

fn path_inside(parent: &Path, child: &Path) -> bool {
    if let Ok(rel) = child.strip_prefix(parent) {
        return !rel.components().any(|c| matches!(c, Component::ParentDir));
    }
    false
}

/// Builds the worker `p2-dev-tools` extension (`reload_plugins` tool + `p2-development` section).
pub fn create_reload_extension(
    reload: ReloadFn,
    on_checkout: OnCheckoutFn,
    worker: std::sync::Weak<crate::api::v1::worker::WorkerProcess>,
) -> Extension {
    let hint_section = section(
        "p2-development",
        Arc::new(move |input, ctx| {
            let on_checkout = on_checkout.clone();
            let worker = worker.clone();
            Box::pin(async move {
                if !on_checkout() {
                    return Ok(None);
                }
                let conv_id = input["conversationId"]
                    .as_u64()
                    .unwrap_or(ROOT_CONVERSATION_ID);
                let Some(worker) = worker.upgrade() else {
                    return Ok(None);
                };
                let Ok(parts) = worker.harness() else {
                    return Ok(None);
                };
                if !is_top_level(&parts.durable.session, conv_id, ctx)
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?
                {
                    return Ok(None);
                }
                Ok(Some(
                    "p2 backend saves only mark dirty. After editing p2 plugins, use reload_plugins to apply dirty plugins (or name plugins). Web/UI edits are plugin edits too: reload_plugins rebuilds the web bundle and open pages reload onto it. Runtime edits need an explicit `npm run reload -- --restart` from apps/p2 with this server's P2_DATA_DIR.".into(),
                ))
            })
        }),
        None,
    );

    let tool = ToolRegistration {
        declaration: Tool {
            name: "reload_plugins".into(),
            description: "Builds p2's changed plugins (the user plugin root's and the built-in ones) and rolls them out to the server, every session's worker and the page; optional plugins names them. Reports per plugin: applied, removed, failed with the errors (file:line:column: message), or refused with the reason; and errors logged meanwhile. A failed build applies nothing: the old code keeps running. Runtime code needs npm run reload -- --restart instead.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugins": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1, "maxLength": 100 },
                        "maxItems": 100
                    }
                },
                "additionalProperties": true
            }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: false,
        callers: None,
        deferred: false,
        execute: Arc::new(move |args, _, ctx| {
            let reload = reload.clone();
            Box::pin(async move {
                let parsed: ReloadArgs = serde_json::from_value(args)?;
                match reload(parsed.plugins, ctx.clone()).await {
                    Ok(result) => {
                        let ok = result.refused.is_none()
                            && result
                                .plugins
                                .iter()
                                .all(|e| e.outcome == "applied" || e.outcome == "removed");
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(result_text(&result).into())]),
                            is_error: Some(!ok),
                            details: Some(json!({ "ok": ok })),
                            ..Default::default()
                        }
                        .into())
                    }
                    Err(err) => {
                        if ctx.is_aborted() {
                            return Err(DurableError::invalid(err.to_string()));
                        }
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(err.to_string().into())]),
                            is_error: Some(true),
                            details: Some(json!({ "ok": false })),
                            ..Default::default()
                        }
                        .into())
                    }
                }
            })
        }),
    };

    define_extension(Extension {
        name: "p2-dev-tools".into(),
        sections: vec![hint_section],
        tools: vec![tool],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(lookup("p2.plugin-reload")?);
    let server_contract = contract.clone();
    registry.register(
        "dev-tools",
        FacetKind::Server,
        define_facet(move |host| {
            host.serve(
                server_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "session workers only"))?;
                    if !matches!(parsed, Principal::Worker { .. }) {
                        return Err(ChordError::new("forbidden", "session workers only").into());
                    }
                    let host_handle = object.host();
                    let mut members = Members::variant("worker");
                    members.method("reload", move |args, _| {
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let parsed: ReloadArgs = serde_json::from_value(args.to_serde()?)?;
                            match service_of::<Arc<ReloadHandler>>(
                                &host_handle,
                                PLUGIN_RELOAD_SERVICE,
                            ) {
                                Ok(handler) => handler(parsed.plugins).await,
                                Err(_) => Ok(JsonValue::from_serde(json!({
                                    "refused": "plugin reload is not active on this server",
                                    "plugins": [],
                                    "logs": []
                                }))),
                            }
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;

    let worker_tools_contract = Arc::new(Contract::local(
        "p2.dev-tools",
        1,
        Vec::new(),
        indexmap::IndexMap::new(),
    )?);
    let worker_reload_contract = contract;
    registry.register(
        "dev-tools",
        FacetKind::Worker,
        define_facet(move |host| {
            let reload_contract = worker_reload_contract.clone();
            host.serve(
                worker_tools_contract.clone(),
                move |_, _, object| {
                    let host_handle = object.host();
                    if let Ok(worker) = worker_of(&host_handle) {
                        let call_host = host_handle.clone();
                        let call_contract = reload_contract.clone();
                        let reload_fn: ReloadFn = Arc::new(move |plugins, ctx| {
                            let call_host = call_host.clone();
                            let call_contract = call_contract.clone();
                            Box::pin(async move {
                                let args = match plugins {
                                    Some(plugins) => json!({ "plugins": plugins }),
                                    None => json!({}),
                                };
                                let params = JsonValue::from_serde(json!({}));
                                let call_args = JsonValue::from_serde(args);
                                let raw = if let Ok(peer) =
                                    service_of::<RemotePeer>(&call_host, "p2.server-peer")
                                {
                                    let held = peer.hold(call_contract, params, &ctx)?;
                                    let res = match held.wait(&ctx).await {
                                        Ok(()) => held.call("reload", call_args, &ctx).await,
                                        Err(err) => Err(err),
                                    };
                                    held.release();
                                    res?.to_serde()?
                                } else if call_host.served_contract(call_contract.name()).is_some()
                                {
                                    let held = call_host.hold(call_contract, params, &ctx)?;
                                    let res = match held.wait(&ctx).await {
                                        Ok(()) => held.call("reload", call_args, &ctx).await,
                                        Err(err) => Err(err),
                                    };
                                    held.release();
                                    res?.to_serde()?
                                } else {
                                    return Err(Error::Invalid(
                                        "p2.plugin-reload server view is unavailable".into(),
                                    ));
                                };
                                Ok(serde_json::from_value(raw)?)
                            })
                        });
                        let loc_worker = worker.clone();
                        let on_checkout: OnCheckoutFn = Arc::new(move || {
                            let location = loc_worker.location.value();
                            if location.env.is_some() {
                                return false;
                            }
                            let cwd = std::fs::canonicalize(&location.cwd)
                                .unwrap_or_else(|_| PathBuf::from(&location.cwd));
                            let project = std::fs::canonicalize(&loc_worker.init.app)
                                .unwrap_or_else(|_| loc_worker.init.app.clone());
                            !project.as_os_str().is_empty()
                                && (path_inside(&project, &cwd) || path_inside(&cwd, &project))
                        });
                        let ext = create_reload_extension(
                            reload_fn,
                            on_checkout,
                            Arc::downgrade(&worker),
                        );
                        let ext_name = ext.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(ext)?;
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
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
