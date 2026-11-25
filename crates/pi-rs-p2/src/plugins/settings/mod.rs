//! `settings` built-in plugin: owner-only `p2.settings` view & partial update over `p2.model-config`.

use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::api::v1::{
    FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::service_of,
};

/// Native callback key published by `models` to reload + save `settings.json` atomically.
pub const SETTINGS_SAVER_SERVICE: &str = "p2.settings-saver";

pub type SettingsSaver =
    dyn Fn(Map<String, Value>, Vec<String>) -> pi_rs_chord::sync::SyncFuture<()> + Send + Sync;

#[derive(Deserialize)]
struct UpdateArgs {
    set: Map<String, Value>,
    unset: Vec<String>,
}

fn extract_settings(snapshot: &Value) -> Value {
    snapshot
        .get("settings")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn extract_settings_error(snapshot: &Value) -> Value {
    snapshot
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| {
            errors.iter().find_map(|entry| {
                (entry.get("resource").and_then(Value::as_str) == Some("settings"))
                    .then(|| entry.get("message").cloned())
                    .flatten()
            })
        })
        .unwrap_or(Value::Null)
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let settings_contract = Arc::new(lookup("p2.settings")?);
    let model_config_contract = Arc::new(lookup("p2.model-config")?);

    registry.register(
        "settings",
        FacetKind::Server,
        define_facet(move |host| {
            let model_config_ref = model_config_contract.clone();
            host.serve(
                settings_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "settings are owner-only"))?;
                    if !parsed.is_owner() {
                        return Err(ChordError::new("forbidden", "settings are owner-only").into());
                    }
                    let config =
                        object.get(model_config_ref.clone(), JsonValue::from_serde(json!({})))?;
                    let config_for_settings = config.clone();
                    let settings_src = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let snap = config_for_settings.member("snapshot")?;
                            let init = extract_settings(&snap.value()?.to_json().to_serde()?);
                            target.set(JsonValue::from_serde(init), &ctx.context)?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let sub = snap.subscribe(
                                move |call, context| match call {
                                    WatchCall::Value { value, .. } => {
                                        let next = extract_settings(&value.to_serde()?);
                                        changed.set(JsonValue::from_serde(next), context)
                                    }
                                    WatchCall::Failed { error } => changed.fail(error, context),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let config_for_error = config.clone();
                    let error_src = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let snap = config_for_error.member("snapshot")?;
                            let init = extract_settings_error(&snap.value()?.to_json().to_serde()?);
                            target.set(JsonValue::from_serde(init), &ctx.context)?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let sub = snap.subscribe(
                                move |call, context| match call {
                                    WatchCall::Value { value, .. } => {
                                        let next = extract_settings_error(&value.to_serde()?);
                                        changed.set(JsonValue::from_serde(next), context)
                                    }
                                    WatchCall::Failed { error } => changed.fail(error, context),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let host_handle = object.host();
                    let mut members = Members::variant("owner");
                    members.state("settings", settings_src)?;
                    members.state("error", error_src)?;
                    members.method("update", move |args, _| {
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let parsed: UpdateArgs = serde_json::from_value(args.to_serde()?)?;
                            let saver = service_of::<Arc<SettingsSaver>>(
                                &host_handle,
                                SETTINGS_SAVER_SERVICE,
                            )
                            .map_err(|_| {
                                Error::Invalid("The model configuration is not active".into())
                            })?;
                            saver(parsed.set, parsed.unset).await?;
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
