//! Server facet for `envs`: serves `p2.env-service`, `p2.envs`, `p2.sandbox-secrets`, and `p2.env-managers`.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Facet, Held, Members, ServeOptions, define_facet},
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
    plugins::{
        envs::{
            config::{EnvConfig, env_revision},
            manager::EnvManager,
            store::EnvStore,
        },
        locations::{ENV_MANAGERS_SERVICE, EnvManagers, list_local_directory},
    },
};

fn read_state_json(held: &Held, member: &str) -> Result<Value> {
    held.member(member)?.value()?.to_json().to_serde()
}

fn store_state_json(store: &EnvStore) -> Result<(Value, Value)> {
    let snap = store.snapshot();
    let configs = serde_json::to_value(&snap.configs)?;
    let failure = match snap.error {
        Some(err) => Value::String(err),
        None => Value::Null,
    };
    Ok((configs, failure))
}

fn sessions_using_map(sessions: &Value) -> BTreeMap<String, Vec<Value>> {
    let mut map: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let Some(rows) = sessions.as_array() else {
        return map;
    };
    for row in rows {
        if let Some(env_name) = row.get("env").and_then(Value::as_str) {
            map.entry(env_name.to_owned())
                .or_default()
                .push(row.clone());
        }
    }
    map
}

pub fn server_facet() -> Result<Facet> {
    let env_service_contract = Arc::new(lookup("p2.env-service")?);
    let envs_contract = Arc::new(lookup("p2.envs")?);
    let secrets_contract = Arc::new(lookup("p2.sandbox-secrets")?);
    let server_contract = Arc::new(lookup("p2.server")?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);

    Ok(define_facet(move |facet_host| {
        let host = facet_host.host();
        let default_dir = match server_of(&host) {
            Ok(natives) => PathBuf::from(&natives.info.data_dir),
            Err(_) => PathBuf::from("."),
        };

        let shared_store = Arc::new(OnceCell::<Arc<EnvStore>>::new());
        let shared_mgr = Arc::new(OnceCell::<Arc<EnvManager>>::new());

        {
            let shared_store = shared_store.clone();
            let shared_mgr = shared_mgr.clone();
            let host = host.clone();
            let server_contract = server_contract.clone();
            let catalog_contract = catalog_contract.clone();
            let default_dir = default_dir.clone();

            facet_host.serve(
                env_service_contract.clone(),
                move |_, _, object| {
                    let configs_holder = object.writable(JsonValue::from_serde(json!({})))?;
                    let tests_holder = object.writable(JsonValue::from_serde(json!({})))?;
                    let failure_holder = object.writable(JsonValue::Null)?;

                    let server_ref = object.get(
                        server_contract.clone(),
                        JsonValue::Object(Default::default()),
                    )?;
                    let catalog_ref = object.get(
                        catalog_contract.clone(),
                        JsonValue::Object(Default::default()),
                    )?;

                    let open_store = shared_store.clone();
                    let open_mgr = shared_mgr.clone();
                    let open_host = host.clone();
                    let cfg_w = configs_holder.clone();
                    let fail_w = failure_holder.clone();
                    let dir_open = default_dir.clone();
                    let cat_open = catalog_ref.clone();

                    object.open(
                        move |ctx| {
                            let open_store = open_store.clone();
                            let open_mgr = open_mgr.clone();
                            let open_host = open_host.clone();
                            let cfg_w = cfg_w.clone();
                            let fail_w = fail_w.clone();
                            let server_ref = server_ref.clone();
                            let cat_open = cat_open.clone();
                            let dir_open = dir_open.clone();
                            Box::pin(async move {
                                let dir = server_ref
                                    .member("info")
                                    .and_then(|m| m.value())
                                    .and_then(|v| v.to_json().to_serde())
                                    .ok()
                                    .and_then(|info| {
                                        info.get("dataDir")
                                            .and_then(Value::as_str)
                                            .map(PathBuf::from)
                                    })
                                    .unwrap_or(dir_open);

                                let store = open_store
                                    .get_or_try_init(|| EnvStore::open(dir.clone()))
                                    .await?
                                    .clone();
                                let dir_for_provider = dir.clone();
                                let mgr = open_mgr
                                    .get_or_init(|| async {
                                        let cat = cat_open.clone();
                                        let uses: crate::plugins::envs::manager::UsesFn =
                                            Arc::new(move |name: &str| {
                                                read_state_json(&cat, "sessions")
                                                    .ok()
                                                    .and_then(|s| {
                                                        sessions_using_map(&s)
                                                            .get(name)
                                                            .map(Vec::len)
                                                    })
                                                    .unwrap_or(0)
                                            });
                                        Arc::new(EnvManager::new(dir, uses))
                                    })
                                    .await
                                    .clone();

                                let wrapper = Arc::new(EnvManagers::new(mgr));
                                install_service(&open_host, ENV_MANAGERS_SERVICE, &wrapper)?;
                                let term_provider = Arc::new(
                                    crate::plugins::envs::worker::env_provider(dir_for_provider),
                                );
                                install_service(
                                    &open_host,
                                    crate::plugins::terminals::ENVIRONMENT_PROVIDER_SERVICE,
                                    &term_provider,
                                )?;

                                let (cfgs, fail) = store_state_json(&store)?;
                                cfg_w.set(JsonValue::from_serde(cfgs), &ctx.context)?;
                                fail_w.set(JsonValue::from_serde(fail), &ctx.context)?;

                                let mut rx = store.subscribe();
                                let store_watch = store.clone();
                                let cfg_watch = cfg_w.clone();
                                let fail_watch = fail_w.clone();
                                let task = tokio::spawn(async move {
                                    let bg = pi_rs_chord::context::Context::background();
                                    while rx.changed().await.is_ok() {
                                        if let Ok((c, f)) = store_state_json(&store_watch) {
                                            let _ = cfg_watch.set(JsonValue::from_serde(c), &bg);
                                            let _ = fail_watch.set(JsonValue::from_serde(f), &bg);
                                        }
                                    }
                                });
                                Ok((task.abort_handle(), wrapper, term_provider))
                            })
                        },
                        |state, _| {
                            Box::pin(async move {
                                state.0.abort();
                                Ok(())
                            })
                        },
                    );

                    let mut members = Members::new();
                    members.state("configs", configs_holder)?;
                    members.state("tests", tests_holder.clone())?;
                    members.state("failure", failure_holder)?;

                    let test_mgr = shared_mgr.clone();
                    let test_holder = tests_holder;
                    let test_cache = Arc::new(std::sync::Mutex::new(serde_json::Map::new()));
                    members.method("test", move |args, ctx| {
                        let mgr_cell = test_mgr.clone();
                        let holder = test_holder.clone();
                        let cache = test_cache.clone();
                        Box::pin(async move {
                            let mgr = mgr_cell
                                .get()
                                .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                            let args = args.to_serde()?;
                            let name = args
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("missing name".into()))?;
                            let cfg = mgr.config(name).await?;
                            let test = mgr.test(name).await?;
                            let next = {
                                let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
                                map.insert(
                                    name.to_owned(),
                                    json!({ "revision": env_revision(&cfg), "test": test }),
                                );
                                Value::Object(map.clone())
                            };
                            holder.set(JsonValue::from_serde(next), &ctx.context)?;
                            Ok(JsonValue::from_serde(test))
                        })
                    })?;

                    let reload_store = shared_store.clone();
                    members.method("reload", move |_, _| {
                        let store_cell = reload_store.clone();
                        Box::pin(async move {
                            let store = store_cell
                                .get()
                                .ok_or_else(|| Error::Invalid("EnvStore not open".into()))?;
                            store.reload().await;
                            Ok(JsonValue::Null)
                        })
                    })?;

                    Ok(members)
                },
                ServeOptions::default(),
            )?;
        }

        {
            let svc_contract = env_service_contract.clone();
            let cat_contract = catalog_contract.clone();
            let mgr_cell = shared_mgr.clone();

            facet_host.serve(
                envs_contract.clone(),
                move |_, principal, object| {
                    let allowed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .is_some_and(|p| p.is_owner());
                    if !allowed {
                        return Err(
                            ChordError::new("forbidden", "environments are owner-only").into()
                        );
                    }

                    let service =
                        object.get(svc_contract.clone(), JsonValue::Object(Default::default()))?;
                    let catalog =
                        object.get(cat_contract.clone(), JsonValue::Object(Default::default()))?;

                    let mut members = Members::variant("owner");

                    let configs_src = service.member("configs")?;
                    let tests_src = service.member("tests")?;
                    let sessions_src = catalog.member("sessions")?;

                    {
                        let cfgs = configs_src.clone();
                        let tsts = tests_src.clone();
                        let sess = sessions_src.clone();
                        members.state(
                            "envs",
                            object.follow(move |target, ctx| {
                                let cfgs = cfgs.clone();
                                let tsts = tsts.clone();
                                let sess = sess.clone();
                                Box::pin(async move {
                                    let compute = |c_val: &Value, t_val: &Value, s_val: &Value| {
                                        let usage = sessions_using_map(s_val);
                                        let mut out = Vec::new();
                                        if let Some(map) = c_val.as_object() {
                                            let mut names: Vec<&String> = map.keys().collect();
                                            names.sort();
                                            for name in names {
                                                let cfg_val = &map[name];
                                                let Ok(cfg) =
                                                    serde_json::from_value::<EnvConfig>(cfg_val.clone())
                                                else {
                                                    continue;
                                                };
                                                let users_rows =
                                                    usage.get(name).map(Vec::as_slice).unwrap_or(&[]);
                                                let users: Vec<Value> = users_rows
                                                    .iter()
                                                    .map(|u| {
                                                        json!({
                                                            "id": u["id"],
                                                            "title": u["title"],
                                                            "main": u.get("main").and_then(Value::as_u64).unwrap_or(1),
                                                            "archived": !u["archivedAt"].is_null(),
                                                        })
                                                    })
                                                    .collect();
                                                let mut row = json!({
                                                    "name": name,
                                                    "config": cfg_val,
                                                    "sessions": users.len(),
                                                    "users": users,
                                                });
                                                if let Some(t) = t_val.get(name)
                                                    && t.get("revision").and_then(Value::as_str)
                                                        == Some(env_revision(&cfg).as_str())
                                                {
                                                    row["test"] = t["test"].clone();
                                                }
                                                out.push(row);
                                            }
                                        }
                                        Value::Array(out)
                                    };

                                    let c0 = cfgs.value()?.to_json().to_serde()?;
                                    let t0 = tsts.value()?.to_json().to_serde()?;
                                    let s0 = sess.value()?.to_json().to_serde()?;
                                    target.set(
                                        JsonValue::from_serde(compute(&c0, &t0, &s0)),
                                        &ctx.context,
                                    )?;

                                    let (tx, mut rx) = tokio::sync::watch::channel(());
                                    let tx1 = tx.clone();
                                    let sub1 = cfgs.subscribe(
                                        move |_, _| {
                                            tx1.send_replace(());
                                            Ok(())
                                        },
                                        |_| {},
                                    );
                                    let tx2 = tx.clone();
                                    let sub2 = tsts.subscribe(
                                        move |_, _| {
                                            tx2.send_replace(());
                                            Ok(())
                                        },
                                        |_| {},
                                    );
                                    let sub3 = sess.subscribe(
                                        move |_, _| {
                                            tx.send_replace(());
                                            Ok(())
                                        },
                                        |_| {},
                                    );
                                    let target_w = target.clone();
                                    let c_w = cfgs.clone();
                                    let t_w = tsts.clone();
                                    let s_w = sess.clone();
                                    let task = tokio::spawn(async move {
                                        let bg = pi_rs_chord::context::Context::background();
                                        while rx.changed().await.is_ok() {
                                            if let (Ok(c), Ok(t), Ok(s)) = (
                                                c_w.value().and_then(|v| v.to_json().to_serde()),
                                                t_w.value().and_then(|v| v.to_json().to_serde()),
                                                s_w.value().and_then(|v| v.to_json().to_serde()),
                                            ) {
                                                let _ = target_w.set(
                                                    JsonValue::from_serde(compute(&c, &t, &s)),
                                                    &bg,
                                                );
                                            }
                                        }
                                        drop((sub1, sub2, sub3));
                                    });
                                    Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                                })
                            })?,
                        )?;
                    }

                    members.state("failure", service.member("failure")?)?;

                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("list", move |_, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                Ok(JsonValue::from_serde(serde_json::to_value(mgr.list().await?)?))
                            })
                        })?;
                    }

                    members.method("warm", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;

                    {
                        let mgr_cell = mgr_cell.clone();
                        let svc = service.clone();
                        members.method("save", move |args, ctx| {
                            let mgr_cell = mgr_cell.clone();
                            let svc = svc.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let cfg: EnvConfig = serde_json::from_value(
                                    args.get("config")
                                        .cloned()
                                        .ok_or_else(|| Error::Invalid("missing config".into()))?,
                                )?;
                                mgr.save(name, cfg).await?;
                                svc.call("reload", JsonValue::Null, &ctx.context).await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        let svc = service.clone();
                        members.method("delete", move |args, ctx| {
                            let mgr_cell = mgr_cell.clone();
                            let svc = svc.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let force = args.get("force").and_then(Value::as_bool) == Some(true);
                                mgr.delete(name, force).await?;
                                svc.call("reload", JsonValue::Null, &ctx.context).await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let svc = service.clone();
                        members.method("test", move |args, ctx| {
                            let svc = svc.clone();
                            Box::pin(async move { svc.call("test", args, &ctx.context).await })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("acceptHostKey", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let lines: Vec<String> = serde_json::from_value(
                                    args.get("lines")
                                        .cloned()
                                        .ok_or_else(|| Error::Invalid("missing lines".into()))?,
                                )?;
                                mgr.accept_host_key(name, &lines).await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("forgetHostKey", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                mgr.forget_host_key(name).await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        let svc = service.clone();
                        members.method("generateKey", move |args, ctx| {
                            let mgr_cell = mgr_cell.clone();
                            let svc = svc.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let public_key = mgr.generate_key(name).await?;
                                svc.call("reload", JsonValue::Null, &ctx.context).await?;
                                Ok(JsonValue::from_serde(json!({
                                    "publicKey": public_key,
                                    "identityFile": mgr.key_path(name),
                                })))
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        let svc = service;
                        members.method("uploadKey", move |args, ctx| {
                            let mgr_cell = mgr_cell.clone();
                            let svc = svc.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let key = args
                                    .get("key")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing key".into()))?;
                                let public_key = mgr.upload_key(name, key).await?;
                                svc.call("reload", JsonValue::Null, &ctx.context).await?;
                                Ok(JsonValue::from_serde(json!({
                                    "publicKey": public_key,
                                    "identityFile": mgr.key_path(name),
                                })))
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("publicKey", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                Ok(JsonValue::from_serde(json!(mgr.public_key(name).await?)))
                            })
                        })?;
                    }

                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("listDirectory", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let env = args.get("env").and_then(Value::as_str);
                                let path = args
                                    .get("path")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing path".into()))?;
                                let listing = match env {
                                    Some(name) => mgr.list_remote_directory(name, path).await?,
                                    None => list_local_directory(path).await?,
                                };
                                Ok(JsonValue::from_serde(serde_json::to_value(listing)?))
                            })
                        })?;
                    }

                    for method_name in [
                        "sandboxStatus",
                        "sandboxStart",
                        "sandboxStop",
                        "sandboxResetDisk",
                    ] {
                        let mgr_cell = mgr_cell.clone();
                        members.method(method_name, move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let status = match method_name {
                                    "sandboxStatus" => mgr.sandbox_status(name).await?,
                                    "sandboxStart" => mgr.sandbox_start(name).await?,
                                    "sandboxStop" => mgr.sandbox_stop(name).await?,
                                    _ => mgr.sandbox_reset_disk(name).await?,
                                };
                                Ok(JsonValue::from_serde(serde_json::to_value(status)?))
                            })
                        })?;
                    }

                    Ok(members)
                },
                ServeOptions::default(),
            )?;
        }

        {
            let mgr_cell = shared_mgr.clone();
            facet_host.serve(
                secrets_contract.clone(),
                move |_, principal, _| {
                    let allowed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .is_some_and(|p| p.is_owner());
                    if !allowed {
                        return Err(
                            ChordError::new("forbidden", "sandbox secrets are owner-only").into(),
                        );
                    }
                    let mut members = Members::variant("owner");
                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("list", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                Ok(JsonValue::from_serde(serde_json::to_value(
                                    mgr.secrets(name).await?,
                                )?))
                            })
                        })?;
                    }
                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("set", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let sandbox = args
                                    .get("sandbox")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing sandbox".into()))?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                let hosts: Vec<String> = serde_json::from_value(
                                    args.get("hosts")
                                        .cloned()
                                        .ok_or_else(|| Error::Invalid("missing hosts".into()))?,
                                )?;
                                let preset = args
                                    .get("preset")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                let value =
                                    args.get("value").and_then(Value::as_str).map(str::to_owned);
                                let add_hosts =
                                    args.get("addHosts").and_then(Value::as_bool) == Some(true);
                                mgr.set_secret(sandbox, name, hosts, preset, value, add_hosts)
                                    .await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }
                    {
                        let mgr_cell = mgr_cell.clone();
                        members.method("delete", move |args, _| {
                            let mgr_cell = mgr_cell.clone();
                            Box::pin(async move {
                                let mgr = mgr_cell
                                    .get()
                                    .ok_or_else(|| Error::Invalid("EnvManager not open".into()))?;
                                let args = args.to_serde()?;
                                let sandbox = args
                                    .get("sandbox")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing sandbox".into()))?;
                                let name = args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| Error::Invalid("missing name".into()))?;
                                mgr.delete_secret(sandbox, name).await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
        }

        Ok(())
    }))
}
