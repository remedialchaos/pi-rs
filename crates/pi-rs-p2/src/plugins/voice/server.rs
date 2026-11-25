//! Server facet for the `voice` plugin (`p2.voice-service-store`, `p2.voice-service`, HTTP & WS routes).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::extract::ws::{Message as AxumWsMessage, WebSocket as AxumWebSocket};
use futures_util::{SinkExt, StreamExt};
use indexmap::IndexMap;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Members, ServeOptions},
    json::JsonValue,
    node::http::{
        HttpError, HttpRequest, HttpResponse, RouteOptions, SocketOptions, json_response,
    },
    state::Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio_tungstenite::tungstenite::{
    Message as TungWsMessage, client::IntoClientRequest, http::HeaderValue,
};

use crate::{
    api::v1::{GrantMode, Principal, server::server_of},
    runtime::{
        logs::{LogLevel, process_logs},
        server::ServerServices,
    },
};

use super::{
    contract::{voice_service_contract, voice_service_store_contract},
    protocol::{
        MAX_LOG_EVENTS, MAX_LOG_MESSAGE, SAMPLE_TEXT, Voices, is_valid_id, is_valid_room_command,
        is_valid_room_personal, room_name, validate_log_batch, validate_sample, validate_voices,
    },
};

const DEFAULT_OWNER_NAME: &str = "Owner";
const MAX_SOCKETS: usize = 4;
const GUEST_RECHECK_SECS: u64 = 30;
const CONFIG_POLL_MS: u64 = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceConfig {
    pub url: String,
    pub token: String,
}

#[derive(Deserialize)]
struct RawConfig {
    url: String,
    token: String,
}

/// Strips `//` line comments and `/* */` block comments outside JSON string literals.
pub fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i] as char);
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let ch = input[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Reads and validates `<data_dir>/voice.json`; returns `None` when missing or invalid.
pub async fn read_config(data_dir: &Path) -> Option<VoiceConfig> {
    let path = data_dir.join("voice.json");
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let stripped = strip_json_comments(&text);
    let parsed: RawConfig = serde_json::from_str(&stripped).ok()?;
    if !(parsed.url.starts_with("http://") || parsed.url.starts_with("https://")) {
        return None;
    }
    if parsed.token.is_empty() {
        return None;
    }
    Some(VoiceConfig {
        url: parsed.url.trim_end_matches('/').to_owned(),
        token: parsed.token,
    })
}

async fn fetch_voices(
    client: &reqwest::Client,
    config: &VoiceConfig,
) -> std::result::Result<Voices, String> {
    let reply = client
        .get(format!("{}/voices", config.url))
        .bearer_auth(&config.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !reply.status().is_success() {
        return Err(format!("HTTP {}", reply.status().as_u16()));
    }
    let raw: Value = reply.json().await.map_err(|e| e.to_string())?;
    validate_voices(&raw).ok_or_else(|| "unexpected /voices answer".to_owned())
}

#[derive(Clone, Default)]
struct ServiceSnapshot {
    voices: Option<Voices>,
    failure: Option<String>,
}

fn may_talk(principal: &Principal, session: Option<&str>) -> bool {
    if principal.is_owner() {
        return true;
    }
    match principal {
        Principal::Guest { home, .. } => {
            let target = session.unwrap_or(home.as_str());
            principal.can_send(target)
        }
        _ => false,
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn format_utc_hms_ms(epoch_ms: f64) -> String {
    let total_ms = epoch_ms.max(0.0) as u64;
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = (total_secs / 3600) % 24;
    format!("{hours:02}:{mins:02}:{secs:02}.{ms:03}")
}

fn json_error_response(
    status: u16,
    message: &str,
) -> pi_rs_chord::node::http::HttpResult<HttpResponse> {
    let mut resp = json_response(&json!({ "error": message }))?;
    *resp.status_mut() =
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST);
    Ok(resp)
}

fn parse_principal(req: &HttpRequest) -> Option<Principal> {
    req.principal
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Builds the server facet for the `voice` plugin.
pub fn server_facet() -> Result<pi_rs_chord::host::Facet> {
    let store_contract = Arc::new(voice_service_store_contract()?);
    let service_contract = Arc::new(voice_service_contract()?);
    let client = reqwest::Client::new();
    let open_sockets: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
    let data_dir_slot: Arc<Mutex<Option<PathBuf>>> = Arc::default();

    let mut services = ServerServices::default();

    // POST /voice/log
    services
        .route(
            "POST /voice/log",
            RouteOptions::new(move |req| {
                Box::pin(async move {
                    let Some(principal) = parse_principal(&req) else {
                        return json_error_response(403, "Forbidden");
                    };
                    if !may_talk(&principal, None) {
                        return json_error_response(403, "Forbidden");
                    }
                    let who = match &principal {
                        Principal::Guest { id, .. } => format!("guest:{id} "),
                        _ => String::new(),
                    };
                    let limit = MAX_LOG_EVENTS * (4 * MAX_LOG_MESSAGE + 64) + 1024;
                    let Ok(body) = req.json(limit).await else {
                        return json_error_response(
                            400,
                            "Expected { client, events: [{ t, message }] }",
                        );
                    };
                    let Some(batch) = validate_log_batch(&body) else {
                        return json_error_response(
                            400,
                            "Expected { client, events: [{ t, message }] }",
                        );
                    };
                    for ev in batch.events {
                        let sanitized: String = ev
                            .message
                            .chars()
                            .map(|c| if c.is_control() { ' ' } else { c })
                            .collect();
                        let ts = format_utc_hms_ms(ev.t);
                        let line = format!("[voice {who}{}] {ts} {sanitized}", batch.client);
                        process_logs().append("voice", LogLevel::Event, &line);
                    }
                    axum::response::Response::builder()
                        .status(204)
                        .body(axum::body::Body::empty())
                        .map_err(|e| HttpError::new(500, e.to_string()))
                })
            }),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;

    // POST /voice/sample
    let sample_client = client.clone();
    let sample_dir_slot = data_dir_slot.clone();
    services
        .route(
            "POST /voice/sample",
            RouteOptions::new(move |req| {
                let client = sample_client.clone();
                let dir_slot = sample_dir_slot.clone();
                Box::pin(async move {
                    let Some(principal) = parse_principal(&req) else {
                        return json_error_response(403, "Forbidden");
                    };
                    if !principal.is_owner() {
                        return json_error_response(403, "Forbidden");
                    }
                    let Some(data_dir) = dir_slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
                    else {
                        return json_error_response(503, "Server not ready");
                    };
                    let Some(config) = read_config(&data_dir).await else {
                        return json_error_response(503, "Voice is not configured (voice.json)");
                    };
                    let Ok(body) = req.json(4096).await else {
                        return json_error_response(400, "Expected { engine?, voice?, speed }");
                    };
                    let Some(sample) = validate_sample(&body) else {
                        return json_error_response(400, "Expected { engine?, voice?, speed }");
                    };
                    let mut payload = serde_json::Map::new();
                    payload.insert("text".into(), json!(SAMPLE_TEXT));
                    payload.insert("speed".into(), json!(sample.speed));
                    if let Some(engine) = sample.engine {
                        payload.insert("engine".into(), json!(engine));
                    }
                    if let Some(voice) = sample.voice {
                        payload.insert("voice".into(), json!(voice));
                    }
                    if let Some(lang) = sample.language {
                        payload.insert("language".into(), json!(lang));
                    }
                    let reply = match client
                        .post(format!("{}/speak", config.url))
                        .bearer_auth(&config.token)
                        .json(&Value::Object(payload))
                        .timeout(Duration::from_secs(30))
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(err) => {
                            return json_error_response(
                                502,
                                &format!("Speech service failed: {err}"),
                            );
                        }
                    };
                    let status = reply.status().as_u16();
                    let content_type = reply
                        .headers()
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/octet-stream")
                        .to_owned();
                    let bytes = match reply.bytes().await {
                        Ok(b) => b,
                        Err(err) => {
                            return json_error_response(
                                502,
                                &format!("Speech service failed: {err}"),
                            );
                        }
                    };
                    axum::response::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, content_type)
                        .header(http::header::CONTENT_LENGTH, bytes.len())
                        .header(http::header::CACHE_CONTROL, "no-store")
                        .body(axum::body::Body::from(bytes))
                        .map_err(|e| HttpError::new(500, e.to_string()))
                })
            }),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;

    // WS /voice/room
    let ws_sockets = open_sockets.clone();
    let ws_dir_slot = data_dir_slot.clone();
    services
        .upgrade(
            "/voice/room",
            SocketOptions {
                public: false,
                max_payload: 256 * 1024,
                handler: Arc::new(move |browser_ws, req| {
                    let open = ws_sockets.clone();
                    let dir_slot = ws_dir_slot.clone();
                    Box::pin(
                        async move { handle_room_socket(browser_ws, req, open, dir_slot).await },
                    )
                }),
            },
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;

    let facet_dir_slot = data_dir_slot;
    Ok(services.with_facet(move |host| {
        let host_handle = host.host();
        *facet_dir_slot.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(PathBuf::from(&server_of(&host_handle)?.info.data_dir));
        let store_client = client.clone();

        host.serve(
            store_contract.clone(),
            move |_params, _principal, self_| {
                let data_dir = PathBuf::from(&server_of(&host_handle)?.info.data_dir);
                let client = store_client.clone();
                let (tx, _rx) = watch::channel(ServiceSnapshot::default());
                let refresh_notify = Arc::new(Notify::new());
                let voices_rx = tx.subscribe();
                let failure_rx = tx.subscribe();

                let voices_source: Source = self_.follow(move |target, ctx| {
                    let mut rx = voices_rx.clone();
                    Box::pin(async move {
                        let current = rx.borrow_and_update().voices.clone();
                        target.set(
                            JsonValue::from_serde(serde_json::to_value(&current)?),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let task = tokio::spawn(async move {
                            let bg = Context::background();
                            while rx.changed().await.is_ok() {
                                let v = rx.borrow_and_update().voices.clone();
                                if let Ok(json_val) = serde_json::to_value(&v) {
                                    let _ = changed.set(JsonValue::from_serde(json_val), &bg);
                                }
                            }
                        });
                        Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let failure_source: Source = self_.follow(move |target, ctx| {
                    let mut rx = failure_rx.clone();
                    Box::pin(async move {
                        let current = rx.borrow_and_update().failure.clone();
                        target.set(
                            JsonValue::from_serde(serde_json::to_value(&current)?),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let task = tokio::spawn(async move {
                            let bg = Context::background();
                            while rx.changed().await.is_ok() {
                                let f = rx.borrow_and_update().failure.clone();
                                if let Ok(json_val) = serde_json::to_value(&f) {
                                    let _ = changed.set(JsonValue::from_serde(json_val), &bg);
                                }
                            }
                        });
                        Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let watch_notify = refresh_notify.clone();
                self_.on_live(move |ctx| {
                    let data_dir = data_dir.clone();
                    let client = client.clone();
                    let tx = tx.clone();
                    let notify = watch_notify.clone();
                    Box::pin(async move {
                        let mut last_raw: Option<String> = None;
                        let mut first = true;
                        while !ctx.context.is_aborted() {
                            let current_raw =
                                tokio::fs::read_to_string(data_dir.join("voice.json"))
                                    .await
                                    .ok();
                            if first || current_raw != last_raw {
                                first = false;
                                last_raw = current_raw;
                                let next = match read_config(&data_dir).await {
                                    None => ServiceSnapshot {
                                        voices: None,
                                        failure: None,
                                    },
                                    Some(cfg) => match fetch_voices(&client, &cfg).await {
                                        Ok(v) => ServiceSnapshot {
                                            voices: Some(v),
                                            failure: None,
                                        },
                                        Err(err) => ServiceSnapshot {
                                            voices: None,
                                            failure: Some(format!(
                                                "Speech service unreachable: {err}"
                                            )),
                                        },
                                    },
                                };
                                tx.send_replace(next);
                            }
                            let sleep = tokio::time::sleep(Duration::from_millis(CONFIG_POLL_MS));
                            let notified = notify.notified();
                            let woken = ctx
                                .context
                                .await_with(async {
                                    tokio::select! {
                                        _ = sleep => false,
                                        _ = notified => {
                                            last_raw = None;
                                            true
                                        }
                                    }
                                })
                                .await;
                            if woken.is_err() {
                                break;
                            }
                        }
                        Ok(())
                    })
                });

                let mut members = Members::new();
                members.state("voices", voices_source)?;
                members.state("failure", failure_source)?;
                members.method("refresh", move |_, _| {
                    let notify = refresh_notify.clone();
                    Box::pin(async move {
                        notify.notify_one();
                        Ok(JsonValue::Null)
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let store_ref_contract = store_contract.clone();
        host.serve(
            service_contract.clone(),
            move |_params, principal_raw, self_| {
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => {
                        return Err(ChordError::new(
                            "forbidden",
                            "owners and read/write guests only",
                        )
                        .into());
                    }
                };
                let can_talk = principal.is_owner()
                    || matches!(
                        &principal,
                        Principal::Guest { sessions, .. }
                            if sessions.values().any(|m| *m == GrantMode::Full)
                    );
                if !can_talk {
                    return Err(
                        ChordError::new("forbidden", "owners and read/write guests only").into(),
                    );
                }

                let store = self_.get(
                    store_ref_contract.clone(),
                    JsonValue::Object(IndexMap::new()),
                )?;
                let voices_src = store.member("voices")?;
                let failure_src = store.member("failure")?;

                let refresh_store = store.clone();
                let refresh_failure = failure_src.clone();
                self_.on_live(move |ctx| {
                    let store = refresh_store.clone();
                    let failure = refresh_failure.clone();
                    Box::pin(async move {
                        if let Ok(val) = failure.value()
                            && val.to_json() != JsonValue::Null
                        {
                            let _ = store.call("refresh", JsonValue::Null, &ctx.context).await;
                        }
                        Ok(())
                    })
                });

                let mut members = Members::variant("talker");
                members.state("voices", self_.follow_source(voices_src)?)?;
                members.state("failure", self_.follow_source(failure_src)?)?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

async fn handle_room_socket(
    mut browser_ws: AxumWebSocket,
    req: HttpRequest,
    open: Arc<Mutex<BTreeMap<String, usize>>>,
    dir_slot: Arc<Mutex<Option<PathBuf>>>,
) -> pi_rs_chord::node::http::HttpResult<()> {
    let session = query_param(&req.query, "session").unwrap_or_default();
    let conversation: u64 = query_param(&req.query, "conversation")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let Some(room) = (conversation >= 1)
        .then(|| room_name(&session, conversation))
        .flatten()
    else {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(400, "Bad Request"));
    };
    let Some(principal) = parse_principal(&req) else {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(403, "Forbidden"));
    };
    if !may_talk(&principal, Some(&session)) {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(403, "Forbidden"));
    }

    let (participant_id, participant_name, participant_role, is_owner) = match &principal {
        Principal::Owner { .. } => (
            "owner".to_owned(),
            DEFAULT_OWNER_NAME.to_owned(),
            "owner",
            true,
        ),
        Principal::Guest { id, name, .. } => (id.clone(), name.clone(), "guest", false),
        _ => {
            let _ = browser_ws.close().await;
            return Err(HttpError::new(403, "Forbidden"));
        }
    };
    if !is_valid_id(&participant_id) {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(403, "Forbidden"));
    }

    let Some(data_dir) = dir_slot.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(503, "Service Unavailable"));
    };
    let Some(config) = read_config(&data_dir).await else {
        let _ = browser_ws.close().await;
        return Err(HttpError::new(503, "Service Unavailable"));
    };

    let socket_key = format!("room {}", principal.key());
    let too_many = {
        let mut counts = open.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.get(&socket_key).copied().unwrap_or(0);
        if count >= MAX_SOCKETS {
            true
        } else {
            counts.insert(socket_key.clone(), count + 1);
            false
        }
    };
    if too_many {
        let _ = browser_ws.close().await;
        return Ok(());
    }
    struct SocketGuard {
        open: Arc<Mutex<BTreeMap<String, usize>>>,
        key: String,
    }
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let mut counts = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(left) = counts.get(&self.key).copied() {
                if left <= 1 {
                    counts.remove(&self.key);
                } else {
                    counts.insert(self.key.clone(), left - 1);
                }
            }
        }
    }
    let _guard = SocketGuard {
        open,
        key: socket_key,
    };

    let ws_url = format!(
        "{}/room",
        config
            .url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let mut upstream_req = ws_url
        .into_client_request()
        .map_err(|e| HttpError::new(502, e.to_string()))?;
    let headers = upstream_req.headers_mut();
    if let Ok(auth) = HeaderValue::from_str(&format!("Bearer {}", config.token)) {
        headers.insert("Authorization", auth);
    }
    if let Ok(r) = HeaderValue::from_str(&room) {
        headers.insert("X-Room", r);
    }
    if let Ok(pid) = HeaderValue::from_str(&participant_id) {
        headers.insert("X-Participant-Id", pid);
    }
    let truncated_name: String = participant_name.chars().take(64).collect();
    let encoded_name = utf8_percent_encode(&truncated_name, NON_ALPHANUMERIC).to_string();
    if let Ok(pname) = HeaderValue::from_str(&encoded_name) {
        headers.insert("X-Participant-Name", pname);
    }
    if let Ok(prole) = HeaderValue::from_str(participant_role) {
        headers.insert("X-Participant-Role", prole);
    }

    let upstream_ws = match tokio_tungstenite::connect_async(upstream_req).await {
        Ok((ws, _)) => ws,
        Err(_) => {
            let _ = browser_ws
                .send(AxumWsMessage::Text(
                    json!({
                        "type": "error",
                        "code": "unavailable",
                        "message": "Speech service unreachable"
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            let _ = browser_ws.close().await;
            return Ok(());
        }
    };

    let (mut browser_tx, mut browser_rx) = browser_ws.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_ws.split();

    let mut recheck = tokio::time::interval(Duration::from_secs(GUEST_RECHECK_SECS));
    recheck.tick().await;

    loop {
        tokio::select! {
            _ = recheck.tick(), if !is_owner => {
                if !may_talk(&principal, Some(&session)) {
                    let _ = browser_tx.close().await;
                    let _ = upstream_tx.close().await;
                    break;
                }
            }
            msg = browser_rx.next() => {
                let Some(Ok(msg)) = msg else {
                    let _ = upstream_tx.close().await;
                    break;
                };
                match msg {
                    AxumWsMessage::Binary(bytes) => {
                        if upstream_tx.send(TungWsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    AxumWsMessage::Text(text) => {
                        let parsed: Option<Value> = serde_json::from_str(&text).ok();
                        let allowed = parsed.as_ref().is_some_and(|v| {
                            if is_owner {
                                is_valid_room_command(v)
                            } else {
                                is_valid_room_personal(v)
                            }
                        });
                        if !allowed {
                            let err_msg = json!({
                                "type": "error",
                                "code": if is_owner { "invalid_message" } else { "forbidden" },
                                "message": if is_owner { "Invalid room command" } else { "Only the owner may do that" }
                            });
                            let _ = browser_tx.send(AxumWsMessage::Text(err_msg.to_string().into())).await;
                            continue;
                        }
                        if upstream_tx.send(TungWsMessage::Text(text.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    AxumWsMessage::Close(_) => {
                        let _ = upstream_tx.close().await;
                        break;
                    }
                    _ => {}
                }
            }
            msg = upstream_rx.next() => {
                let Some(Ok(msg)) = msg else {
                    let _ = browser_tx.close().await;
                    break;
                };
                match msg {
                    TungWsMessage::Binary(bytes) => {
                        if browser_tx.send(AxumWsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    TungWsMessage::Text(text) => {
                        if browser_tx.send(AxumWsMessage::Text(text.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    TungWsMessage::Close(_) => {
                        let _ = browser_tx.close().await;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
