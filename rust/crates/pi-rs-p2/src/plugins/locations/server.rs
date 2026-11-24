//! Server facet for `locations`: serves `p2.agent-locations`.

use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Facet, Held, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::v1::{Principal, p2_contracts::lookup, server::service_of},
    plugins::locations::{
        ENV_MANAGERS_SERVICE, EnvInfo, EnvManagers, is_sandbox, list_local_directory,
        tested_summary,
    },
};

fn read_state_json(held: &Held, member: &str) -> Result<Value> {
    held.member(member)?.value()?.to_json().to_serde()
}

fn check_current(hosts: &Held, session: &str, generation: u64) -> Result<()> {
    let workers = read_state_json(hosts, "workers")?;
    let entry = workers.get(session);
    let running = entry
        .and_then(|w| w.get("running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let current_gen = entry
        .and_then(|w| w.get("generation"))
        .and_then(Value::as_u64);
    if !running || current_gen != Some(generation) {
        return Err(ChordError::new("forbidden", "stale worker").into());
    }
    Ok(())
}

pub fn server_facet() -> Result<Facet> {
    let agent_locations = Arc::new(lookup("p2.agent-locations")?);
    let hosts_contract = Arc::new(lookup("p2.hosts")?);
    let envs_contract = Arc::new(lookup("p2.env-service")?);
    let moves_contract = Arc::new(lookup("p2.session-moves")?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);

    Ok(define_facet(move |facet_host| {
        let hosts_contract = hosts_contract.clone();
        let envs_contract = envs_contract.clone();
        let moves_contract = moves_contract.clone();
        let catalog_contract = catalog_contract.clone();

        facet_host.serve(
            agent_locations.clone(),
            move |_, principal, object| {
                let parsed = principal
                    .and_then(|v| Principal::deserialize(v).ok())
                    .ok_or_else(|| {
                        ChordError::new("forbidden", "location tools are for session workers")
                    })?;
                let Principal::Worker {
                    session,
                    generation,
                } = parsed
                else {
                    return Err(ChordError::new(
                        "forbidden",
                        "location tools are for session workers",
                    )
                    .into());
                };

                let hosts = object.get(hosts_contract.clone(), JsonValue::Object(Default::default()))?;
                let envs = object.get(envs_contract.clone(), JsonValue::Object(Default::default()))?;
                let moves = object.get(moves_contract.clone(), JsonValue::Object(Default::default()))?;
                let catalog =
                    object.get(catalog_contract.clone(), JsonValue::Object(Default::default()))?;
                let host = object.host();

                let mut members = Members::variant("worker");

                {
                    let hosts = hosts.clone();
                    let envs = envs.clone();
                    let session = session.clone();
                    members.method("envs", move |_, _| {
                        let hosts = hosts.clone();
                        let envs = envs.clone();
                        let session = session.clone();
                        Box::pin(async move {
                            check_current(&hosts, &session, generation)?;
                            let configs = read_state_json(&envs, "configs")?;
                            let tests = read_state_json(&envs, "tests")?;
                            let mut names: Vec<String> = configs
                                .as_object()
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                            names.sort();
                            let mut list = Vec::with_capacity(names.len());
                            for name in names {
                                let Some(cfg) = configs.get(&name) else {
                                    continue;
                                };
                                let env_type = cfg
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("ssh")
                                    .to_owned();
                                let host_str = cfg
                                    .get("host")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                let user = if env_type == "ssh" {
                                    cfg.get("user").and_then(Value::as_str).map(str::to_owned)
                                } else {
                                    None
                                };
                                let port = if env_type == "ssh" {
                                    cfg.get("port").and_then(Value::as_u64)
                                } else {
                                    None
                                };
                                let tested = tested_summary(cfg, tests.get(&name));
                                list.push(EnvInfo {
                                    name,
                                    env_type,
                                    host: host_str,
                                    user,
                                    port,
                                    tested,
                                });
                            }
                            Ok(JsonValue::from_serde(json!({ "envs": list })))
                        })
                    })?;
                }

                {
                    let hosts = hosts.clone();
                    let host = host.clone();
                    let session = session.clone();
                    members.method("directories", move |args, _| {
                        let hosts = hosts.clone();
                        let host = host.clone();
                        let session = session.clone();
                        Box::pin(async move {
                            check_current(&hosts, &session, generation)?;
                            let args = args.to_serde()?;
                            let env = args.get("env").and_then(Value::as_str).map(str::to_owned);
                            let path = args
                                .get("path")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("missing path".into()))?
                                .to_owned();
                            let listing = if let Some(env_name) = env {
                                let managers =
                                    service_of::<EnvManagers>(&host, ENV_MANAGERS_SERVICE)?;
                                managers.manager.list_directory(&env_name, &path).await?
                            } else {
                                list_local_directory(&path).await?
                            };
                            Ok(JsonValue::from_serde(serde_json::to_value(listing)?))
                        })
                    })?;
                }

                {
                    let hosts = hosts.clone();
                    let envs = envs.clone();
                    let moves = moves.clone();
                    let catalog = catalog.clone();
                    let session = session.clone();
                    members.method("move", move |args, ctx| {
                        let hosts = hosts.clone();
                        let envs = envs.clone();
                        let moves = moves.clone();
                        let catalog = catalog.clone();
                        let session = session.clone();
                        Box::pin(async move {
                            check_current(&hosts, &session, generation)?;
                            let args = args.to_serde()?;
                            let env = args.get("env").and_then(Value::as_str).map(str::to_owned);
                            let cwd = args
                                .get("cwd")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("missing cwd".into()))?
                                .to_owned();

                            let sessions = read_state_json(&catalog, "sessions")?;
                            let from = sessions
                                .as_array()
                                .and_then(|rows| {
                                    rows.iter().find(|row| {
                                        row.get("id").and_then(Value::as_str) == Some(&session)
                                    })
                                })
                                .and_then(|row| row.get("env"))
                                .and_then(Value::as_str);

                            if let Some(from_env) = from
                                && Some(from_env) != env.as_deref()
                            {
                                let configs = read_state_json(&envs, "configs")?;
                                if is_sandbox(&configs, from_env) {
                                    return Err(ChordError::new(
                                        "forbidden",
                                        format!(
                                            "this session works in sandbox {from_env} and cannot leave it"
                                        ),
                                    )
                                    .into());
                                }
                            }

                            moves
                                .call(
                                    "relocate",
                                    JsonValue::from_serde(json!({
                                        "id": session,
                                        "env": env,
                                        "cwd": cwd,
                                    })),
                                    &ctx.context,
                                )
                                .await
                        })
                    })?;
                }

                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
