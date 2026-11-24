//! Server facet for `models`: serves `p2.model-config`, `p2.models`, and `p2.model-resources`.

use std::{path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::{
    api::v1::{
        Principal,
        p2_contracts::lookup,
        server::{install_service, server_of},
    },
    plugins::models::{
        resources::{MODEL_RESOURCES_SERVICE, ModelResources},
        scope::resolve_model_scope,
    },
};

fn resources_state_json(res: &ModelResources) -> Result<(Value, Value)> {
    let summary = serde_json::to_value(res.summary())?;
    let snapshot = serde_json::to_value(res.snapshot())?;
    Ok((summary, snapshot))
}

fn enabled_models_from_snapshot(snapshot: &Value) -> Vec<String> {
    snapshot
        .get("settings")
        .and_then(|s| s.get("enabledModels"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub fn server_facet() -> Result<Facet> {
    let model_config = Arc::new(lookup("p2.model-config")?);
    let models_contract = Arc::new(lookup("p2.models")?);
    let server_contract = Arc::new(lookup("p2.server")?);

    Ok(define_facet(move |facet_host| {
        let host = facet_host.host();
        let (data_dir, fake) = match server_of(&host) {
            Ok(natives) => (PathBuf::from(&natives.info.data_dir), natives.info.fake),
            Err(_) => (PathBuf::from("."), false),
        };
        let shared = Arc::new(OnceCell::<Arc<ModelResources>>::new());

        let config_shared = shared.clone();
        let config_host = host.clone();
        let server_contract_cfg = server_contract.clone();
        facet_host.serve(
            model_config.clone(),
            move |_, _, object| {
                let shared = config_shared.clone();
                let host = config_host.clone();
                let server_contract = server_contract_cfg.clone();
                let fallback_dir = data_dir.clone();

                let summary_holder = object.writable(JsonValue::from_serde(json!({
                    "models": [],
                    "defaultModel": null,
                    "errors": []
                })))?;
                let snapshot_holder = object.writable(JsonValue::from_serde(json!({
                    "version": 0,
                    "settings": {},
                    "credentials": [],
                    "providers": [],
                    "models": [],
                    "errors": []
                })))?;

                let open_shared = shared.clone();
                let open_host = host.clone();
                let sum_w = summary_holder.clone();
                let snap_w = snapshot_holder.clone();
                let dir_open = fallback_dir.clone();
                let server_ref =
                    object.get(server_contract, JsonValue::Object(Default::default()))?;

                object.open(
                    move |ctx| {
                        let open_shared = open_shared.clone();
                        let open_host = open_host.clone();
                        let sum_w = sum_w.clone();
                        let snap_w = snap_w.clone();
                        let server_ref = server_ref.clone();
                        let dir_open = dir_open.clone();
                        Box::pin(async move {
                            let (dir, is_fake) = match server_ref
                                .member("info")
                                .and_then(|m| m.value())
                                .and_then(|v| v.to_json().to_serde())
                            {
                                Ok(info) => (
                                    info.get("dataDir")
                                        .and_then(Value::as_str)
                                        .map(PathBuf::from)
                                        .unwrap_or(dir_open),
                                    info.get("fake").and_then(Value::as_bool).unwrap_or(fake),
                                ),
                                Err(_) => (dir_open, fake),
                            };

                            let res = open_shared
                                .get_or_try_init(|| ModelResources::open(dir, is_fake))
                                .await?
                                .clone();
                            install_service(&open_host, MODEL_RESOURCES_SERVICE, &res)?;

                            let (sum_val, snap_val) = resources_state_json(&res)?;
                            sum_w.set(JsonValue::from_serde(sum_val), &ctx.context)?;
                            snap_w.set(JsonValue::from_serde(snap_val), &ctx.context)?;

                            let mut rx = res.subscribe();
                            let res_watch = res.clone();
                            let sum_watch = sum_w.clone();
                            let snap_watch = snap_w.clone();
                            let task = tokio::spawn(async move {
                                let bg = pi_rs_chord::context::Context::background();
                                while rx.changed().await.is_ok() {
                                    if let Ok((s, sn)) = resources_state_json(&res_watch) {
                                        let _ = sum_watch.set(JsonValue::from_serde(s), &bg);
                                        let _ = snap_watch.set(JsonValue::from_serde(sn), &bg);
                                    }
                                }
                            });
                            Ok(task.abort_handle())
                        })
                    },
                    |handle, _| {
                        Box::pin(async move {
                            handle.abort();
                            Ok(())
                        })
                    },
                );

                let mut members = Members::new();
                members.state("summary", summary_holder.clone())?;
                members.state("snapshot", snapshot_holder.clone())?;

                let default_shared = shared.clone();
                let default_sum = summary_holder.clone();
                let default_snap = snapshot_holder.clone();
                members.method("setDefault", move |args, ctx| {
                    let shared = default_shared.clone();
                    let sum_w = default_sum.clone();
                    let snap_w = default_snap.clone();
                    Box::pin(async move {
                        let res = shared
                            .get()
                            .ok_or_else(|| Error::Invalid("ModelResources not open".into()))?;
                        let args = args.to_serde()?;
                        let provider = args
                            .get("provider")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing provider".into()))?;
                        let model_id = args
                            .get("modelId")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing modelId".into()))?;
                        let thinking_level = args
                            .get("thinkingLevel")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing thinkingLevel".into()))?;

                        if res.models.get_model(provider, model_id).is_none() {
                            return Err(Error::Invalid("Unknown model".into()));
                        }

                        let mut settings = res.settings();
                        let Some(obj) = settings.as_object_mut() else {
                            return Err(Error::Invalid("settings must be an object".into()));
                        };
                        obj.insert(
                            "defaultModel".into(),
                            json!({ "provider": provider, "modelId": model_id }),
                        );
                        obj.insert("thinkingLevel".into(), json!(thinking_level));
                        res.save_settings(settings).await?;
                        let (s, sn) = resources_state_json(res)?;
                        sum_w.set(JsonValue::from_serde(s), &ctx.context)?;
                        snap_w.set(JsonValue::from_serde(sn), &ctx.context)?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let enabled_shared = shared.clone();
                let enabled_sum = summary_holder.clone();
                let enabled_snap = snapshot_holder.clone();
                members.method("setEnabledModels", move |args, ctx| {
                    let shared = enabled_shared.clone();
                    let sum_w = enabled_sum.clone();
                    let snap_w = enabled_snap.clone();
                    Box::pin(async move {
                        let res = shared
                            .get()
                            .ok_or_else(|| Error::Invalid("ModelResources not open".into()))?;
                        let args = args.to_serde()?;
                        let patterns = args
                            .get("patterns")
                            .and_then(Value::as_array)
                            .ok_or_else(|| Error::Invalid("missing patterns".into()))?;
                        let cleaned: Vec<String> = patterns
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .collect();

                        let mut settings = res.settings();
                        let Some(obj) = settings.as_object_mut() else {
                            return Err(Error::Invalid("settings must be an object".into()));
                        };
                        if cleaned.is_empty() {
                            obj.remove("enabledModels");
                        } else {
                            obj.insert("enabledModels".into(), json!(cleaned));
                        }
                        res.save_settings(settings).await?;
                        let (s, sn) = resources_state_json(res)?;
                        sum_w.set(JsonValue::from_serde(s), &ctx.context)?;
                        snap_w.set(JsonValue::from_serde(sn), &ctx.context)?;
                        Ok(JsonValue::Null)
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let cfg_contract = model_config.clone();
        facet_host.serve(
            models_contract.clone(),
            move |_, principal, object| {
                let allowed = principal
                    .and_then(|v| Principal::deserialize(v).ok())
                    .is_some_and(|p| p.is_owner());
                if !allowed {
                    return Err(ChordError::new("forbidden", "models are owner-only").into());
                }

                let config =
                    object.get(cfg_contract.clone(), JsonValue::Object(Default::default()))?;
                let mut members = Members::variant("owner");

                let summary_src = config.member("summary")?;
                let snapshot_src = config.member("snapshot")?;

                {
                    let src = summary_src.clone();
                    members.state(
                        "models",
                        object.follow(move |target, ctx| {
                            let src = src.clone();
                            Box::pin(async move {
                                let initial = src
                                    .value()?
                                    .to_json()
                                    .to_serde()?
                                    .get("models")
                                    .cloned()
                                    .unwrap_or_else(|| json!([]));
                                target.set(JsonValue::from_serde(initial), &ctx.context)?;
                                let target_sub = target.clone();
                                let sub = src.subscribe(
                                    move |call, c| {
                                        if let pi_rs_chord::watch::WatchCall::Value {
                                            value, ..
                                        } = call
                                        {
                                            let val = value
                                                .to_serde()?
                                                .get("models")
                                                .cloned()
                                                .unwrap_or_else(|| json!([]));
                                            target_sub.set(JsonValue::from_serde(val), c)?;
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                }

                {
                    let src = summary_src.clone();
                    members.state(
                        "defaultModel",
                        object.follow(move |target, ctx| {
                            let src = src.clone();
                            Box::pin(async move {
                                let initial = src
                                    .value()?
                                    .to_json()
                                    .to_serde()?
                                    .get("defaultModel")
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                target.set(JsonValue::from_serde(initial), &ctx.context)?;
                                let target_sub = target.clone();
                                let sub = src.subscribe(
                                    move |call, c| {
                                        if let pi_rs_chord::watch::WatchCall::Value {
                                            value, ..
                                        } = call
                                        {
                                            let val = value
                                                .to_serde()?
                                                .get("defaultModel")
                                                .cloned()
                                                .unwrap_or(Value::Null);
                                            target_sub.set(JsonValue::from_serde(val), c)?;
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                }

                {
                    let src = summary_src.clone();
                    members.state(
                        "errors",
                        object.follow(move |target, ctx| {
                            let src = src.clone();
                            Box::pin(async move {
                                let initial = src
                                    .value()?
                                    .to_json()
                                    .to_serde()?
                                    .get("errors")
                                    .cloned()
                                    .unwrap_or_else(|| json!([]));
                                target.set(JsonValue::from_serde(initial), &ctx.context)?;
                                let target_sub = target.clone();
                                let sub = src.subscribe(
                                    move |call, c| {
                                        if let pi_rs_chord::watch::WatchCall::Value {
                                            value, ..
                                        } = call
                                        {
                                            let val = value
                                                .to_serde()?
                                                .get("errors")
                                                .cloned()
                                                .unwrap_or_else(|| json!([]));
                                            target_sub.set(JsonValue::from_serde(val), c)?;
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                }

                {
                    let src = snapshot_src.clone();
                    members.state(
                        "enabledModels",
                        object.follow(move |target, ctx| {
                            let src = src.clone();
                            Box::pin(async move {
                                let initial = enabled_models_from_snapshot(
                                    &src.value()?.to_json().to_serde()?,
                                );
                                target.set(JsonValue::from_serde(json!(initial)), &ctx.context)?;
                                let target_sub = target.clone();
                                let sub = src.subscribe(
                                    move |call, c| {
                                        if let pi_rs_chord::watch::WatchCall::Value {
                                            value, ..
                                        } = call
                                        {
                                            let val =
                                                enabled_models_from_snapshot(&value.to_serde()?);
                                            target_sub.set(JsonValue::from_serde(json!(val)), c)?;
                                        }
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                }

                {
                    let sum = summary_src.clone();
                    let snap = snapshot_src.clone();
                    members.state(
                        "scopedModels",
                        object.follow(move |target, ctx| {
                            let sum = sum.clone();
                            let snap = snap.clone();
                            Box::pin(async move {
                                let compute = |sum_val: &Value, snap_val: &Value| -> Value {
                                    let models: Vec<crate::plugins::models::resources::ModelInfo> =
                                        sum_val
                                            .get("models")
                                            .cloned()
                                            .and_then(|v| serde_json::from_value(v).ok())
                                            .unwrap_or_default();
                                    let patterns = enabled_models_from_snapshot(snap_val);
                                    let scoped = resolve_model_scope(&patterns, &models).models;
                                    json!(scoped)
                                };
                                let s0 = sum.value()?.to_json().to_serde()?;
                                let sn0 = snap.value()?.to_json().to_serde()?;
                                target
                                    .set(JsonValue::from_serde(compute(&s0, &sn0)), &ctx.context)?;

                                let sum_sub = sum.clone();
                                let snap_sub = snap.clone();
                                let t1 = target.clone();
                                let t2 = target.clone();
                                let s_a = sum.clone();
                                let sn_a = snap.clone();
                                let sub1 = sum_sub.subscribe(
                                    move |_, c| {
                                        let s = s_a.value()?.to_json().to_serde()?;
                                        let sn = sn_a.value()?.to_json().to_serde()?;
                                        t1.set(JsonValue::from_serde(compute(&s, &sn)), c)?;
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                let sub2 = snap_sub.subscribe(
                                    move |_, c| {
                                        let s = sum.value()?.to_json().to_serde()?;
                                        let sn = snap.value()?.to_json().to_serde()?;
                                        t2.set(JsonValue::from_serde(compute(&s, &sn)), c)?;
                                        Ok(())
                                    },
                                    |_| {},
                                );
                                Ok(Box::new(move || {
                                    drop(sub1);
                                    drop(sub2);
                                })
                                    as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                }

                members.state("resources", snapshot_src)?;

                let cfg_default = config.clone();
                members.method("setDefault", move |args, ctx| {
                    let cfg = cfg_default.clone();
                    Box::pin(async move { cfg.call("setDefault", args, &ctx.context).await })
                })?;

                let cfg_enabled = config;
                members.method("setEnabledModels", move |args, ctx| {
                    let cfg = cfg_enabled.clone();
                    Box::pin(async move { cfg.call("setEnabledModels", args, &ctx.context).await })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
