//! Server facet for the `discord` plugin (`p2.discord-store`, `p2.discord`, `GET /discord/avatar`).

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{AcceptsTable, VersionMappings, schema},
    data::ChordError,
    documents::Definition,
    host::{Facet, Members, ServeOptions},
    json::JsonValue,
    node::http::{HttpError, HttpRequest, HttpResponse, RouteOptions},
    watch::WatchCall,
};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::{
    api::v1::{Principal, p2_contracts::lookup, server::server_of},
    plugins::voice::server::strip_json_comments,
    runtime::server::ServerServices,
};

use super::{
    bot::DiscordPort,
    contract::{
        Binding, BotStatus, DiscordFileWire, Link, LinkUpdate, Pending, TurnState,
        discord_contract, discord_ingress_contract, discord_store_contract,
    },
    fleet::{FIRST_BOT, Fleet},
    relay::{DeliverInput, DeliverResult, Relay, RelayState, SessionPort, StatePort},
};

const TYPING_SECS: u64 = 8;
const CHECK_EVERY: u64 = 2;

fn bindings_definition() -> Result<Arc<Definition>> {
    let def = Definition::new(
        "p2.discord-bindings",
        schema::obj([
            (
                "bindings",
                schema::required(JsonValue::from_serde(json!({ "type": "object" }))),
            ),
            (
                "pending",
                schema::required(JsonValue::from_serde(json!({ "type": "object" }))),
            ),
        ]),
        2,
        || {
            Ok(JsonValue::from_serde(
                json!({ "bindings": {}, "pending": {} }),
            ))
        },
    )?
    .persisted(true, |_, _, _| true)
    .migrating(|old, from| {
        if from != 1 {
            return Err(Error::Invalid(format!(
                "Unknown p2.discord-bindings version {from}"
            )));
        }
        let mut val = old.to_serde()?;
        for section in ["bindings", "pending"] {
            if let Some(obj) = val.get_mut(section).and_then(Value::as_object_mut) {
                for entry in obj.values_mut() {
                    if let Some(eobj) = entry.as_object_mut() {
                        eobj.entry("botId").or_insert_with(|| json!(FIRST_BOT));
                    }
                }
            }
        }
        Ok(JsonValue::from_serde(val))
    });
    Ok(Arc::new(def))
}

fn read_relay_state(val: &Value) -> RelayState {
    let bindings = val
        .get("bindings")
        .and_then(|v| serde_json::from_value::<BTreeMap<String, Binding>>(v.clone()).ok())
        .unwrap_or_default();
    let pending = val
        .get("pending")
        .and_then(|v| serde_json::from_value::<BTreeMap<String, Pending>>(v.clone()).ok())
        .unwrap_or_default();
    RelayState { bindings, pending }
}

fn write_relay_state(state: &RelayState) -> Result<JsonValue> {
    Ok(JsonValue::from_serde(json!({
        "bindings": state.bindings,
        "pending": state.pending,
    })))
}

struct DocStatePort {
    doc: pi_rs_chord::host::ObjectDocument,
}

impl StatePort for DocStatePort {
    fn value(&self) -> RelayState {
        self.doc
            .source()
            .and_then(|s| s.value())
            .and_then(|v| v.to_json().to_serde())
            .map(|v| read_relay_state(&v))
            .unwrap_or_default()
    }

    fn change(
        &self,
        update: Box<dyn FnOnce(&mut RelayState) + Send>,
    ) -> pi_rs_chord::sync::SyncFuture<()> {
        let doc = self.doc.clone();
        Box::pin(async move {
            let current = doc.source()?.value()?.to_json().to_serde()?;
            let mut next = read_relay_state(&current);
            update(&mut next);
            let next_json = write_relay_state(&next)?;
            doc.change(&Context::background(), move |draft| {
                Box::pin(async move { draft.set(&[], next_json) })
            })
            .await
        })
    }
}

struct ServerSessionPort {
    host: pi_rs_chord::host::Host,
    catalog: pi_rs_chord::host::Held,
    envs: pi_rs_chord::host::Held,
    locks: pi_rs_chord::host::Held,
    ingress_contract: Arc<pi_rs_chord::contracts::Contract>,
}

impl SessionPort for ServerSessionPort {
    fn usable(&self, session: String) -> pi_rs_chord::sync::SyncFuture<Option<String>> {
        let catalog = self.catalog.clone();
        let envs = self.envs.clone();
        Box::pin(async move {
            let sessions_val = catalog.member("sessions")?.value()?.to_json().to_serde()?;
            let row = sessions_val.as_array().and_then(|arr| {
                arr.iter()
                    .find(|r| r.get("id").and_then(Value::as_str) == Some(session.as_str()))
            });
            let Some(row) = row else {
                return Ok(Some("its p2 session is gone.".into()));
            };
            if !row.get("archivedAt").is_none_or(Value::is_null) {
                return Ok(Some("its p2 session is archived.".into()));
            }
            let env_name = row.get("env").and_then(Value::as_str);
            let configs = envs.member("configs")?.value()?.to_json().to_serde()?;
            let in_sandbox = env_name.is_some_and(|e| {
                configs
                    .get(e)
                    .and_then(|c| c.get("type"))
                    .and_then(Value::as_str)
                    == Some("gondolin")
            });
            if !in_sandbox {
                return Ok(Some("its p2 session does not work in a sandbox.".into()));
            }
            Ok(None)
        })
    }

    fn lock(&self, session: String) -> Option<crate::runtime::session_lock::SessionLock> {
        let locks_val = self
            .locks
            .member("locks")
            .ok()?
            .value()
            .ok()?
            .to_json()
            .to_serde()
            .ok()?;
        let lock_val = locks_val.get(&session)?.clone();
        serde_json::from_value(lock_val).ok()
    }

    fn deliver(
        &self,
        session: String,
        input: DeliverInput,
    ) -> pi_rs_chord::sync::SyncFuture<DeliverResult> {
        let host = self.host.clone();
        let contract = self.ingress_contract.clone();
        Box::pin(async move {
            let root = host.root();
            let worker = root.hold(
                contract,
                JsonValue::from_serde(json!({ "session": session })),
            )?;
            let ctx = Context::background();
            worker.wait(&ctx).await?;
            let files: Vec<DiscordFileWire> = input
                .files
                .into_iter()
                .map(|f| DiscordFileWire {
                    url: f.url,
                    name: f.name,
                    size: f.size,
                    key: f.key,
                    ref_id: f.ref_id,
                })
                .collect();
            let mut payload = Map::new();
            if let Some(conv) = input.conversation {
                payload.insert("conversation".into(), json!(conv));
            }
            payload.insert("requestId".into(), json!(input.request_id));
            payload.insert(
                "author".into(),
                json!({ "id": input.author_id, "name": input.author_name }),
            );
            payload.insert("text".into(), json!(input.text));
            let mut ctx_obj = Map::new();
            ctx_obj.insert("channel".into(), json!(input.context.channel));
            ctx_obj.insert("guild".into(), json!(input.context.guild));
            ctx_obj.insert("messages".into(), json!(input.context.messages));
            if let Some(th) = input.context.thread {
                ctx_obj.insert("thread".into(), json!(th));
            }
            payload.insert("context".into(), Value::Object(ctx_obj));
            payload.insert("files".into(), serde_json::to_value(files)?);
            payload.insert("lock".into(), serde_json::to_value(input.lock)?);

            let res = worker
                .call(
                    "deliver",
                    JsonValue::from_serde(Value::Object(payload)),
                    &ctx,
                )
                .await?
                .to_serde()?;
            Ok(DeliverResult {
                conversation: res["conversation"].as_u64().unwrap_or(1),
                submission_id: res["submissionId"].as_u64().unwrap_or(0),
            })
        })
    }

    fn turn(
        &self,
        session: String,
        submission_id: u64,
    ) -> pi_rs_chord::sync::SyncFuture<TurnState> {
        let host = self.host.clone();
        let contract = self.ingress_contract.clone();
        Box::pin(async move {
            let root = host.root();
            let worker = root.hold(
                contract,
                JsonValue::from_serde(json!({ "session": session })),
            )?;
            let ctx = Context::background();
            worker.wait(&ctx).await?;
            let res = worker
                .call(
                    "turn",
                    JsonValue::from_serde(json!({ "submissionId": submission_id })),
                    &ctx,
                )
                .await?
                .to_serde()?;
            Ok(serde_json::from_value(res)?)
        })
    }

    fn thread(
        &self,
        session: String,
        name: String,
        before: u64,
    ) -> pi_rs_chord::sync::SyncFuture<u64> {
        let host = self.host.clone();
        let contract = self.ingress_contract.clone();
        Box::pin(async move {
            let root = host.root();
            let worker = root.hold(
                contract,
                JsonValue::from_serde(json!({ "session": session })),
            )?;
            let ctx = Context::background();
            worker.wait(&ctx).await?;
            let res = worker
                .call(
                    "thread",
                    JsonValue::from_serde(json!({ "name": name, "before": before })),
                    &ctx,
                )
                .await?
                .to_serde()?;
            res.get("conversation")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::Invalid("missing conversation".into()))
        })
    }

    fn read(
        &self,
        session: String,
        id: String,
        offset: u64,
        length: usize,
    ) -> pi_rs_chord::sync::SyncFuture<Vec<u8>> {
        let host = self.host.clone();
        let contract = self.ingress_contract.clone();
        Box::pin(async move {
            let root = host.root();
            let worker = root.hold(
                contract,
                JsonValue::from_serde(json!({ "session": session })),
            )?;
            let ctx = Context::background();
            worker.wait(&ctx).await?;
            let res = worker
                .call(
                    "read",
                    JsonValue::from_serde(json!({ "id": id, "offset": offset, "length": length })),
                    &ctx,
                )
                .await?
                .to_serde()?;
            if let Some(arr) = res.as_array() {
                Ok(arr
                    .iter()
                    .filter_map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect())
            } else {
                Ok(Vec::new())
            }
        })
    }

    fn command(
        &self,
        session: String,
        conversation: Option<u64>,
        command: super::contract::Command,
    ) -> pi_rs_chord::sync::SyncFuture<String> {
        let host = self.host.clone();
        let contract = self.ingress_contract.clone();
        Box::pin(async move {
            let root = host.root();
            let worker = root.hold(
                contract,
                JsonValue::from_serde(json!({ "session": session })),
            )?;
            let ctx = Context::background();
            worker.wait(&ctx).await?;
            let mut payload = Map::new();
            if let Some(conv) = conversation {
                payload.insert("conversation".into(), json!(conv));
            }
            payload.insert("command".into(), serde_json::to_value(command)?);
            let res = worker
                .call(
                    "command",
                    JsonValue::from_serde(Value::Object(payload)),
                    &ctx,
                )
                .await?
                .to_serde()?;
            Ok(res
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned())
        })
    }
}

/// Extracts sorted MCP server names from `mcp.json` (with comments and optional UTF-8 BOM).
pub fn mcp_server_names(text: &str) -> Vec<String> {
    let cleaned = strip_json_comments(text.trim_start_matches('\u{feff}'));
    let Ok(val) = serde_json::from_str::<Value>(&cleaned) else {
        return Vec::new();
    };
    let Some(servers) = val.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut names: Vec<String> = servers.keys().cloned().collect();
    names.sort();
    names
}

async fn fetch_avatar(
    client: &reqwest::Client,
    url: &str,
) -> std::result::Result<(String, Vec<u8>), String> {
    if !url.starts_with("https://cdn.discordapp.com/") {
        return Err("Not a Discord avatar".into());
    }
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let ct = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let valid_image = matches!(
        ct.split(';').next().unwrap_or("").trim(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    );
    if !resp.status().is_success() || !valid_image {
        return Err("No avatar".into());
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
    Ok((ct, bytes))
}

fn empty_response(status: u16) -> pi_rs_chord::node::http::HttpResult<HttpResponse> {
    axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .map_err(|e| HttpError::new(500, e.to_string()))
}

fn discord_accepts() -> AcceptsTable {
    let mut table = IndexMap::new();
    table.insert(2, VersionMappings::default());
    table
}

fn parse_principal(req: &HttpRequest) -> Option<Principal> {
    req.principal
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Builds the server facet for the `discord` plugin.
pub fn server_facet() -> Result<Facet> {
    let store_contract = Arc::new(discord_store_contract()?);
    let discord_contract_arc = Arc::new(discord_contract()?);
    let ingress_contract = Arc::new(discord_ingress_contract()?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);
    let envs_contract = Arc::new(lookup("p2.env-service")?);
    let locks_contract = Arc::new(lookup("p2.session-locks")?);
    let moves_contract = Arc::new(lookup("p2.session-moves")?);
    let status_contract = Arc::new(lookup("p2.status-store")?);
    let state_def = bindings_definition()?;

    let fleet_slot: Arc<Mutex<Option<Arc<Fleet>>>> = Arc::default();
    type AvatarCache = Arc<Mutex<BTreeMap<String, (String, Vec<u8>)>>>;
    let avatar_cache: AvatarCache = Arc::default();
    let http_client = reqwest::Client::new();

    let mut services = ServerServices::default();

    // GET /discord/avatar?bot=...
    let av_fleet = fleet_slot.clone();
    let av_cache = avatar_cache.clone();
    let av_client = http_client.clone();
    services
        .route(
            "GET /discord/avatar",
            RouteOptions::new(move |req| {
                let fleet_slot = av_fleet.clone();
                let cache = av_cache.clone();
                let client = av_client.clone();
                Box::pin(async move {
                    let Some(principal) = parse_principal(&req) else {
                        return empty_response(404);
                    };
                    if !principal.is_owner() {
                        return empty_response(404);
                    }
                    let bot_id = form_urlencoded::parse(req.query.as_bytes())
                        .find(|(k, _)| k == "bot")
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    let url = {
                        let guard = fleet_slot.lock().unwrap_or_else(|e| e.into_inner());
                        guard.as_ref().and_then(|f| {
                            f.views()
                                .into_iter()
                                .find(|v| v.id == bot_id)
                                .and_then(|v| match v.status {
                                    BotStatus::Connected { avatar_url, .. } => avatar_url,
                                    _ => None,
                                })
                        })
                    };
                    let Some(url) = url else {
                        return empty_response(404);
                    };
                    let cached = cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&url)
                        .cloned();
                    let (content_type, bytes) = match cached {
                        Some(hit) => hit,
                        None => match fetch_avatar(&client, &url).await {
                            Ok(fetched) => {
                                cache
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(url, fetched.clone());
                                fetched
                            }
                            Err(_) => return empty_response(502),
                        },
                    };
                    axum::response::Response::builder()
                        .status(200)
                        .header(http::header::CONTENT_TYPE, content_type)
                        .header(http::header::CONTENT_LENGTH, bytes.len())
                        .header(http::header::CACHE_CONTROL, "private, max-age=86400")
                        .header("Cross-Origin-Resource-Policy", "same-origin")
                        .body(axum::body::Body::from(bytes))
                        .map_err(|e| HttpError::new(500, e.to_string()))
                })
            }),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;

    Ok(services.with_facet(move |host| {
        let host_handle = host.host();
        let store_fleet_slot = fleet_slot.clone();
        let sc = store_contract.clone();
        let ic = ingress_contract.clone();
        let cc = catalog_contract.clone();
        let ec = envs_contract.clone();
        let lc = locks_contract.clone();
        let mc = moves_contract.clone();
        let stc = status_contract.clone();
        let def = state_def.clone();

        host.serve(
            sc,
            move |_params, _principal, self_| {
                let data_dir = PathBuf::from(&server_of(&host_handle)?.info.data_dir);
                let catalog = self_.get(cc.clone(), JsonValue::Object(IndexMap::new()))?;
                let envs = self_.get(ec.clone(), JsonValue::Object(IndexMap::new()))?;
                let locks = self_.get(lc.clone(), JsonValue::Object(IndexMap::new()))?;
                let moves = self_.get(mc.clone(), JsonValue::Object(IndexMap::new()))?;
                let statuses = self_.get(stc.clone(), JsonValue::Object(IndexMap::new()))?;
                let state_doc = self_.state(def.clone())?;

                let open_dir = data_dir.clone();
                let open_slot = store_fleet_slot.clone();
                let fleet_handle = self_.open(
                    move |_| {
                        let dir = open_dir.clone();
                        let slot = open_slot.clone();
                        Box::pin(async move {
                            let fleet = Fleet::open(&dir).await;
                            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(fleet.clone());
                            Ok(fleet)
                        })
                    },
                    move |fleet, _| {
                        Box::pin(async move {
                            fleet.close().await;
                            Ok(())
                        })
                    },
                );

                let relay_fleet = fleet_handle.clone();
                let relay = Arc::new(Relay::new(
                    move |bot_id| {
                        relay_fleet
                            .value()
                            .ok()
                            .and_then(|f| f.connected_port(bot_id))
                    },
                    Arc::new(ServerSessionPort {
                        host: host_handle.clone(),
                        catalog: catalog.clone(),
                        envs: envs.clone(),
                        locks: locks.clone(),
                        ingress_contract: ic.clone(),
                    }),
                    Arc::new(DocStatePort {
                        doc: state_doc.clone(),
                    }),
                ));

                // Bots live view source
                let bots_fleet = fleet_handle.clone();
                let bots_source = self_.follow(move |target, ctx| {
                    let fh = bots_fleet.clone();
                    Box::pin(async move {
                        let fleet = fh.value()?;
                        let (tx, mut rx) = watch::channel(fleet.views());
                        let notify_tx = tx.clone();
                        let fleet_cb = fleet.clone();
                        fleet.subscribe(move || {
                            let _ = notify_tx.send(fleet_cb.views());
                        });
                        let initial = rx.borrow_and_update().clone();
                        target.set(
                            JsonValue::from_serde(serde_json::to_value(&initial)?),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let task = tokio::spawn(async move {
                            let bg = Context::background();
                            while rx.changed().await.is_ok() {
                                let v = rx.borrow_and_update().clone();
                                if let Ok(json_val) = serde_json::to_value(&v) {
                                    let _ = changed.set(JsonValue::from_serde(json_val), &bg);
                                }
                            }
                        });
                        Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                // Bindings sorted by createdAt
                let bindings_doc_src = state_doc.source()?;
                let bindings_source = self_.follow(move |target, ctx| {
                    let src = bindings_doc_src.clone();
                    Box::pin(async move {
                        let initial = src.value()?.to_json().to_serde()?;
                        let mut list: Vec<Binding> =
                            read_relay_state(&initial).bindings.into_values().collect();
                        list.sort_by_key(|b| b.created_at);
                        target.set(
                            JsonValue::from_serde(serde_json::to_value(&list)?),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let sub = src.subscribe(
                            move |call, c| match call {
                                WatchCall::Value { value, .. } => {
                                    let v = value.to_serde()?;
                                    let mut list: Vec<Binding> =
                                        read_relay_state(&v).bindings.into_values().collect();
                                    list.sort_by_key(|b| b.created_at);
                                    Ok(changed.set(
                                        JsonValue::from_serde(serde_json::to_value(&list)?),
                                        c,
                                    )?)
                                }
                                WatchCall::Failed { error } => {
                                    let _ = changed.fail(error, c);
                                    Ok(())
                                }
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                // Event loop from Fleet -> Relay
                let loop_fleet = fleet_handle.clone();
                let loop_relay = relay.clone();
                self_.on_live(move |ctx| {
                    let fh = loop_fleet.clone();
                    let relay = loop_relay.clone();
                    Box::pin(async move {
                        let fleet = fh.value()?;
                        while !ctx.context.is_aborted() {
                            let Ok(received) = ctx.context.await_with(fleet.next()).await else {
                                break;
                            };
                            let res = match received.event {
                                super::bot::BotEvent::Message(msg) => {
                                    relay.on_message(&received.bot, msg).await
                                }
                                super::bot::BotEvent::Ready => relay.catch_up(&received.bot).await,
                            };
                            if let Err(err) = res {
                                eprintln!("p2 discord: {err}");
                            }
                        }
                        Ok(())
                    })
                });

                // Typing & pending-reply timer loop
                let tick_relay = relay.clone();
                let tick_doc = state_doc.clone();
                self_.on_live(move |ctx| {
                    let relay = tick_relay.clone();
                    let doc = tick_doc.clone();
                    Box::pin(async move {
                        let mut tick = 1u64;
                        while !ctx.context.is_aborted() {
                            if ctx
                                .context
                                .await_with(tokio::time::sleep(Duration::from_secs(TYPING_SECS)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            relay.typing().await;
                            if tick.is_multiple_of(CHECK_EVERY) {
                                let has_pending = doc
                                    .source()
                                    .and_then(|s| s.value())
                                    .and_then(|v| v.to_json().to_serde())
                                    .is_ok_and(|v| !read_relay_state(&v).pending.is_empty());
                                if has_pending {
                                    let _ = relay.check_pending(None).await;
                                }
                            }
                            tick += 1;
                        }
                        Ok(())
                    })
                });

                // StatusStore watcher -> checkPending for changed sessions
                let status_held = statuses.clone();
                let status_src = statuses.member("statuses")?;
                let status_relay = relay;
                let status_doc = state_doc.clone();
                let seen_statuses: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
                self_.on_live(move |ctx| {
                    let status_held = status_held.clone();
                    let status_src = status_src.clone();
                    let relay = status_relay.clone();
                    let doc = status_doc.clone();
                    let seen_map = seen_statuses.clone();
                    Box::pin(async move {
                        status_held.wait(&ctx.context).await?;
                        let (updates, mut receive) = tokio::sync::mpsc::unbounded_channel();
                        let _sub = status_src.subscribe(
                            move |_call, _| {
                                let _ = updates.send(());
                                Ok(())
                            },
                            |_| {},
                        );
                        loop {
                            let val = status_src.value()?.to_json();
                            let serde_val = val.to_serde()?;
                            let mut changed = BTreeSet::new();
                            let mut next = BTreeMap::new();
                            if let Some(obj) = serde_val.as_object() {
                                let prev = seen_map.lock().unwrap_or_else(|e| e.into_inner());
                                for (session, st) in obj {
                                    let running =
                                        st.get("running").and_then(Value::as_bool).unwrap_or(false);
                                    let tasks =
                                        st.get("tasks").and_then(Value::as_u64).unwrap_or(0);
                                    let busy = st
                                        .get("busy")
                                        .and_then(Value::as_array)
                                        .map(|a| {
                                            a.iter()
                                                .filter_map(Value::as_u64)
                                                .map(|n| n.to_string())
                                                .collect::<Vec<_>>()
                                                .join(",")
                                        })
                                        .unwrap_or_default();
                                    let sig = format!("{running} {tasks} {busy}");
                                    if prev.get(session) != Some(&sig) {
                                        changed.insert(session.clone());
                                    }
                                    next.insert(session.clone(), sig);
                                }
                            }
                            *seen_map.lock().unwrap_or_else(|e| e.into_inner()) = next;
                            let has_pending = doc
                                .source()
                                .and_then(|s| s.value())
                                .and_then(|v| v.to_json().to_serde())
                                .is_ok_and(|v| !read_relay_state(&v).pending.is_empty());
                            if !changed.is_empty() && has_pending {
                                let _ = relay.check_pending(Some(&changed)).await;
                            }
                            let Some(()) = ctx
                                .context
                                .await_with(receive.recv())
                                .await
                                .map_err(|e| Error::Invalid(e.to_string()))?
                            else {
                                break;
                            };
                        }
                        Ok(())
                    })
                });

                let mut members = Members::new();
                members.state("bots", bots_source)?;
                members.state("bindings", bindings_source)?;

                // addBot
                let add_fleet = fleet_handle.clone();
                members.method("addBot", move |args, _| {
                    let fh = add_fleet.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let token = val
                            .get("token")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing token".into()))?
                            .to_owned();
                        let id = fh.value()?.add(token).await?;
                        Ok(JsonValue::from_serde(json!({ "id": id })))
                    })
                })?;

                // setBotToken
                let token_fleet = fleet_handle.clone();
                members.method("setBotToken", move |args, _| {
                    let fh = token_fleet.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let bot = val
                            .get("bot")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing bot".into()))?
                            .to_owned();
                        let token = val
                            .get("token")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing token".into()))?
                            .to_owned();
                        fh.value()?.set_token(&bot, token).await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                // Helper for unlink
                let unlink_fn = {
                    let doc = state_doc.clone();
                    let locks = locks.clone();
                    let catalog = catalog.clone();
                    let host = host_handle.clone();
                    let ic = ic.clone();
                    Arc::new(move |session: String, ctx: Context| {
                        let doc = doc.clone();
                        let locks = locks.clone();
                        let catalog = catalog.clone();
                        let host = host.clone();
                        let ic = ic.clone();
                        Box::pin(async move {
                            let current = doc.source()?.value()?.to_json().to_serde()?;
                            let mut next = read_relay_state(&current);
                            next.bindings.retain(|_, b| b.session != session);
                            let next_json = write_relay_state(&next)?;
                            doc.change(&ctx, move |draft| {
                                Box::pin(async move { draft.set(&[], next_json) })
                            })
                            .await?;
                            locks
                                .call(
                                    "set",
                                    JsonValue::from_serde(
                                        json!({ "session": session, "lock": null }),
                                    ),
                                    &ctx,
                                )
                                .await?;
                            let sessions_val =
                                catalog.member("sessions")?.value()?.to_json().to_serde()?;
                            let active = sessions_val.as_array().is_some_and(|arr| {
                                arr.iter().any(|r| {
                                    r.get("id").and_then(Value::as_str) == Some(session.as_str())
                                        && r.get("archivedAt").is_none_or(Value::is_null)
                                })
                            });
                            if active {
                                let root = host.root();
                                if let Ok(worker) = root
                                    .hold(ic, JsonValue::from_serde(json!({ "session": session })))
                                    && worker.wait(&ctx).await.is_ok()
                                {
                                    let _ = worker.call("unlinked", JsonValue::Null, &ctx).await;
                                }
                            }
                            Ok::<_, Error>(())
                        })
                    })
                };

                // removeBot
                let rm_fleet = fleet_handle.clone();
                let rm_doc = state_doc.clone();
                let rm_unlink = unlink_fn.clone();
                members.method("removeBot", move |args, ctx| {
                    let fh = rm_fleet.clone();
                    let doc = rm_doc.clone();
                    let unlink = rm_unlink.clone();
                    Box::pin(async move {
                        let bot = args
                            .to_serde()?
                            .get("bot")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing bot".into()))?
                            .to_owned();
                        let current = doc.source()?.value()?.to_json().to_serde()?;
                        let sessions: Vec<String> = read_relay_state(&current)
                            .bindings
                            .into_values()
                            .filter(|b| b.bot_id == bot)
                            .map(|b| b.session)
                            .collect();
                        for session in sessions {
                            unlink(session, ctx.context.clone()).await?;
                        }
                        fh.value()?.remove(&bot).await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                // server
                let srv_fleet = fleet_handle.clone();
                members.method("server", move |args, _| {
                    let fh = srv_fleet.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let bot_id = val
                            .get("bot")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing bot".into()))?;
                        let guild = val
                            .get("guild")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing guild".into()))?;
                        let bot = fh.value()?.connected(bot_id).ok_or_else(|| {
                            Error::Invalid("The Discord bot is not connected".into())
                        })?;
                        let details = bot.server(guild).await?;
                        Ok(JsonValue::from_serde(serde_json::to_value(details)?))
                    })
                })?;

                // mcpServers
                let mcp_dir = data_dir;
                members.method("mcpServers", move |_, _| {
                    let dir = mcp_dir.clone();
                    Box::pin(async move {
                        let text = tokio::fs::read_to_string(dir.join("mcp.json")).await.ok();
                        let names = text.map(|t| mcp_server_names(&t)).unwrap_or_default();
                        Ok(JsonValue::from_serde(serde_json::to_value(names)?))
                    })
                })?;

                // link
                let link_fleet = fleet_handle;
                let link_doc = state_doc.clone();
                let link_envs = envs;
                let link_catalog = catalog;
                let link_moves = moves;
                let link_locks = locks.clone();
                members.method("link", move |args, ctx| {
                    let fh = link_fleet.clone();
                    let doc = link_doc.clone();
                    let envs = link_envs.clone();
                    let catalog = link_catalog.clone();
                    let moves = link_moves.clone();
                    let locks = link_locks.clone();
                    Box::pin(async move {
                        let link: Link = serde_json::from_value(args.to_serde()?)?;
                        let current = doc.source()?.value()?.to_json().to_serde()?;
                        let state = read_relay_state(&current);
                        if state.bindings.values().any(|b| b.session == link.session) {
                            return Err(Error::Invalid(
                                "This session is linked already: unlink it first".into(),
                            ));
                        }
                        if state.bindings.contains_key(&link.channel) {
                            return Err(Error::Invalid(
                                "This channel is linked to another session".into(),
                            ));
                        }
                        let configs = envs.member("configs")?.value()?.to_json().to_serde()?;
                        let Some(env_cfg) = configs.get(&link.sandbox) else {
                            return Err(Error::Invalid(format!("No sandbox {}", link.sandbox)));
                        };
                        if env_cfg.get("type").and_then(Value::as_str) != Some("gondolin") {
                            return Err(Error::Invalid(format!("No sandbox {}", link.sandbox)));
                        }
                        let sessions_val =
                            catalog.member("sessions")?.value()?.to_json().to_serde()?;
                        let row = sessions_val
                            .as_array()
                            .and_then(|arr| {
                                arr.iter().find(|r| {
                                    r.get("id").and_then(Value::as_str)
                                        == Some(link.session.as_str())
                                        && r.get("archivedAt").is_none_or(Value::is_null)
                                })
                            })
                            .ok_or_else(|| Error::Invalid("No such session".into()))?;
                        let bot = fh.value()?.connected(&link.bot).ok_or_else(|| {
                            Error::Invalid("The Discord bot is not connected".into())
                        })?;
                        let channel = bot.channel(link.channel.clone()).await?;
                        if channel.thread.is_some() || channel.guild_id != link.guild {
                            return Err(Error::Invalid("Pick a channel of the server".into()));
                        }
                        if row.get("env").and_then(Value::as_str) != Some(link.sandbox.as_str()) {
                            let cwd = env_cfg
                                .get("mounts")
                                .and_then(Value::as_array)
                                .and_then(|m| m.first())
                                .and_then(|m| m.get("guest"))
                                .and_then(Value::as_str)
                                .unwrap_or("/root");
                            let res = moves
                                .call(
                                    "relocate",
                                    JsonValue::from_serde(json!({
                                        "id": link.session,
                                        "cwd": cwd,
                                        "env": link.sandbox,
                                    })),
                                    &ctx.context,
                                )
                                .await?
                                .to_serde()?;
                            if let Some(refused) = res.get("refused").and_then(Value::as_str) {
                                return Err(Error::Invalid(format!(
                                    "The session could not move into {}: {refused}",
                                    link.sandbox
                                )));
                            }
                        }
                        locks
                            .call(
                                "set",
                                JsonValue::from_serde(json!({
                                    "session": link.session,
                                    "lock": link.lock,
                                })),
                                &ctx.context,
                            )
                            .await?;

                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let latest = doc.source()?.value()?.to_json().to_serde()?;
                        let mut next = read_relay_state(&latest);
                        next.bindings.insert(
                            link.channel.clone(),
                            Binding {
                                bot_id: link.bot,
                                channel_id: link.channel,
                                channel_name: channel.name,
                                guild_id: channel.guild_id,
                                guild_name: channel.guild_name,
                                session: link.session,
                                wake: link.wake,
                                cursor: channel.last_message_id,
                                threads: IndexMap::new(),
                                created_at: now,
                            },
                        );
                        let next_json = write_relay_state(&next)?;
                        doc.change(&ctx.context, move |draft| {
                            Box::pin(async move { draft.set(&[], next_json) })
                        })
                        .await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                // unlink
                let un_unlink = unlink_fn;
                members.method("unlink", move |args, ctx| {
                    let unlink = un_unlink.clone();
                    Box::pin(async move {
                        let session = args
                            .to_serde()?
                            .get("session")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing session".into()))?
                            .to_owned();
                        unlink(session, ctx.context).await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                // updateLink
                let upd_doc = state_doc;
                let upd_locks = locks;
                members.method("updateLink", move |args, ctx| {
                    let doc = upd_doc.clone();
                    let locks = upd_locks.clone();
                    Box::pin(async move {
                        let update: LinkUpdate = serde_json::from_value(args.to_serde()?)?;
                        let current = doc.source()?.value()?.to_json().to_serde()?;
                        let mut next = read_relay_state(&current);
                        if !next.bindings.values().any(|b| b.session == update.session) {
                            return Err(Error::Invalid("This session is not linked".into()));
                        }
                        locks
                            .call(
                                "set",
                                JsonValue::from_serde(json!({
                                    "session": update.session,
                                    "lock": update.lock,
                                })),
                                &ctx.context,
                            )
                            .await?;
                        for binding in next.bindings.values_mut() {
                            if binding.session == update.session {
                                binding.wake = update.wake.clone();
                            }
                        }
                        let next_json = write_relay_state(&next)?;
                        doc.change(&ctx.context, move |draft| {
                            Box::pin(async move { draft.set(&[], next_json) })
                        })
                        .await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let store_ref_contract = store_contract.clone();
        host.serve(
            discord_contract_arc.clone(),
            move |_params, principal_raw, self_| {
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => {
                        return Err(ChordError::new("forbidden", "Discord is the owner's").into());
                    }
                };
                if !principal.is_owner() {
                    return Err(ChordError::new("forbidden", "Discord is the owner's").into());
                }
                let store = self_.get(
                    store_ref_contract.clone(),
                    JsonValue::Object(IndexMap::new()),
                )?;
                let mut members = Members::variant("owner");
                members.state("bots", self_.follow_source(store.member("bots")?)?)?;
                members.state("bindings", self_.follow_source(store.member("bindings")?)?)?;
                for method in [
                    "addBot",
                    "setBotToken",
                    "removeBot",
                    "server",
                    "mcpServers",
                    "link",
                    "unlink",
                    "updateLink",
                ] {
                    let s = store.clone();
                    let name = method.to_owned();
                    members.method(method, move |args, ctx| {
                        let s = s.clone();
                        let name = name.clone();
                        Box::pin(async move {
                            let val = args.to_serde()?;
                            s.call(&name, JsonValue::from_serde(val), &ctx.context)
                                .await
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions {
                accepts: discord_accepts(),
                ..Default::default()
            },
        )?;

        Ok(())
    }))
}
