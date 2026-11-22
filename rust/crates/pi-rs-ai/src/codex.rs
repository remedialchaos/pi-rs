use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Response, StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use url::Url;

use crate::chat_state::ChatState;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, read_partial, write_partial};
use crate::http::{abortable, client_for, header_map, identity_headers, payload};
use crate::model::{CacheRetention, Model};
use crate::proxy::resolve_http_proxy_url_for_target;
use crate::request::{ProviderHeaders, ProviderResponse, StreamOptions, Transport, merge_headers};
use crate::responses::{
    Dialect, ResponsesEvents, ResponsesRequest, build_body, consume_sse, replay_assistant,
};
use crate::simple_options::build_base_options;
use crate::types::{StopReason, TranscriptContext};

const MAX_AGE: Duration = Duration::from_secs(45 * 60);
const IDLE: Duration = Duration::from_secs(5 * 60);
const BETA: &str = "responses_websockets=2026-02-06";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Continuation {
    request: Value,
    response_id: String,
    items: Vec<Value>,
}
struct Connection {
    socket: Socket,
    created: Instant,
    last_used: Instant,
    continuation: Option<Continuation>,
}
type Entry = Arc<AsyncMutex<Option<Connection>>>;
type Pool = Mutex<HashMap<(String, String), Entry>>;
static POOL: LazyLock<Pool> = LazyLock::new(|| Mutex::new(HashMap::new()));
static FALLBACK: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn close_sessions(session: Option<&str>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let mut fallback = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(session) = session {
        pool.retain(|(s, _), _| s != session);
        fallback.remove(session);
    } else {
        pool.clear();
        fallback.clear();
    }
}

pub fn extract_account_id(token: &str) -> Result<String> {
    let mut parts = token.split('.');
    let invalid = || Error::Auth("Failed to extract accountId from token".into());
    let _ = parts.next();
    let body = parts.next().ok_or_else(invalid)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(invalid());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(body)
        .or_else(|_| STANDARD_NO_PAD.decode(body.trim_end_matches('=')))
        .map_err(|_| invalid())?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| invalid())?;
    value["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(invalid)
}

pub fn stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (mut state, stream) = ChatState::new(&model);
    tokio::spawn(async move {
        let result = run(&mut state, &model, context, &options).await;
        state.finish(result);
    });
    stream
}
pub fn stream_simple(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    check_key(&model, &options)?;
    let options = build_base_options(&model, &context, options);
    Ok(stream(model, context, options))
}
fn check_key<'a>(model: &Model, opts: &'a StreamOptions) -> Result<&'a str> {
    opts.request
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth(format!("No API key for provider: {}", model.provider)))
}
fn codex_url(base: &str) -> Result<Url> {
    let raw = if base.trim().is_empty() {
        "https://chatgpt.com/backend-api"
    } else {
        base
    };
    let mut url = Url::parse(raw)?;
    let path = url.path().trim_end_matches('/');
    let final_path = if path.ends_with("/codex/responses") {
        path.to_owned()
    } else if path.ends_with("/codex") {
        format!("{path}/responses")
    } else {
        format!("{path}/codex/responses")
    };
    url.set_path(&final_path);
    Ok(url)
}
fn headers(
    model: &Model,
    opts: &StreamOptions,
    key: &str,
    acct: &str,
    session: Option<&str>,
    ws: bool,
) -> Result<HeaderMap> {
    let mut headers = ProviderHeaders::from([("originator".into(), Some("pi".into()))]);
    headers.extend(identity_headers(&model.base, &opts.request.identity));
    if let Some(model_headers) = &model.headers {
        headers = merge_headers(
            headers,
            model_headers
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect(),
        );
    }
    headers = merge_headers(headers, opts.request.headers.clone());
    for (name, value) in [
        ("authorization", Some(format!("Bearer {key}"))),
        ("chatgpt-account-id", Some(acct.into())),
        (
            "openai-beta",
            Some(if ws { BETA } else { "responses=experimental" }.into()),
        ),
        ("accept", (!ws).then_some("text/event-stream".into())),
        ("content-type", (!ws).then_some("application/json".into())),
    ] {
        headers.retain(|k, _| !k.eq_ignore_ascii_case(name));
        headers.insert(name.into(), value);
    }
    if let Some(session) = session.filter(|s| !s.is_empty()) {
        for name in ["session-id", "x-client-request-id"] {
            headers.retain(|k, _| !k.eq_ignore_ascii_case(name));
            headers.insert(name.into(), Some(session.into()));
        }
    }
    header_map(headers)
}

async fn connect(url: &Url, headers: HeaderMap, opts: &StreamOptions) -> Result<Socket> {
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    req.headers_mut().extend(headers);
    let proxy = resolve_http_proxy_url_for_target(url.as_str(), &opts.request.env)?;
    let connection = async {
        if let Some(proxy) = proxy {
            // Use CONNECT to keep TLS and the WebSocket handshake end-to-end with the provider.
            // An HTTPS proxy cannot be safely treated as a cleartext proxy.
            if proxy.scheme() != "http" {
                return Err(Error::Protocol(
                    "Codex WebSocket HTTPS proxy requires TLS tunneling; retry over HTTP SSE"
                        .into(),
                ));
            }
            let stream = proxy_tunnel(&proxy, url).await?;
            client_async_tls(req, stream)
                .await
                .map(|(socket, _)| socket)
                .map_err(|e| Error::Protocol(format!("Codex WebSocket connect failed: {e}")))
        } else {
            connect_async(req)
                .await
                .map(|(socket, _)| socket)
                .map_err(|e| Error::Protocol(format!("Codex WebSocket connect failed: {e}")))
        }
    };
    let timeout = opts.websocket_connect_timeout_ms.unwrap_or(15_000);
    abortable(&opts.request.signal, async {
        if timeout == 0 {
            return connection.await;
        }
        tokio::time::timeout(Duration::from_millis(timeout), connection)
            .await
            .map_err(|_| {
                Error::Protocol(format!("WebSocket connection timed out after {timeout}ms"))
            })?
    })
    .await
}
async fn proxy_tunnel(proxy: &Url, target: &Url) -> Result<TcpStream> {
    let host = proxy
        .host_str()
        .ok_or_else(|| Error::Protocol("proxy host missing".into()))?;
    let port = proxy
        .port_or_known_default()
        .ok_or_else(|| Error::Protocol("proxy port missing".into()))?;
    let mut stream = TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
    let target_host = target
        .host_str()
        .ok_or_else(|| Error::Protocol("WebSocket target host missing".into()))?;
    let target_port = target
        .port_or_known_default()
        .ok_or_else(|| Error::Protocol("WebSocket target port missing".into()))?;
    let authority = format!("{target_host}:{target_port}");
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() || proxy.password().is_some() {
        let credentials = format!(
            "{}:{}",
            proxy.username(),
            proxy.password().unwrap_or_default()
        );
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            STANDARD.encode(credentials)
        ));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= 16 * 1024 {
            return Err(Error::Protocol("Oversized WebSocket proxy response".into()));
        }
        head.push(stream.read_u8().await?);
    }
    let status = std::str::from_utf8(&head)
        .ok()
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok());
    if status != Some(200) {
        return Err(Error::Protocol(format!(
            "WebSocket proxy tunnel rejected with status {}",
            status.unwrap_or_default()
        )));
    }
    Ok(stream)
}

fn pick_delta(body: &Value, continuation: &Continuation) -> Option<Value> {
    let a = body.as_object()?;
    let b = continuation.request.as_object()?;
    let mut current = a.iter().filter(|(k, _)| k.as_str() != "input");
    let old = b.iter().filter(|(k, _)| k.as_str() != "input");
    if !current.by_ref().eq(old) {
        return None;
    }
    let source = a.get("input")?.as_array()?;
    let before = b.get("input")?.as_array()?;
    let past = source
        .strip_prefix(before.as_slice())?
        .strip_prefix(continuation.items.as_slice())?;
    let mut outgoing = body.clone();
    outgoing["input"] = Value::Array(past.to_vec());
    outgoing["previous_response_id"] = json!(continuation.response_id);
    Some(outgoing)
}
fn pooled(session: &str, acct: &str) -> Entry {
    POOL.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry((session.to_owned(), acct.to_owned()))
        .or_default()
        .clone()
}
struct Fault {
    error: Error,
    kind: FaultKind,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultKind {
    Transport,
    Limit,
    Continuation,
    Provider,
}
impl Fault {
    fn transport(error: Error) -> Self {
        Self {
            error,
            kind: FaultKind::Transport,
        }
    }
    fn provider(error: Error) -> Self {
        Self {
            error,
            kind: FaultKind::Provider,
        }
    }
}
async fn websocket(
    url: &Url,
    headers: HeaderMap,
    model: &Model,
    opts: &StreamOptions,
    state: &mut ChatState,
    req: &ResponsesRequest,
    entry: Option<&Entry>,
) -> Result<StopReason, Fault> {
    let mut guard = entry.and_then(|map| map.try_lock().ok());
    let mut conn = guard
        .as_deref_mut()
        .and_then(Option::take)
        .filter(|conn| conn.created.elapsed() < MAX_AGE && conn.last_used.elapsed() < IDLE);
    if conn.is_none() {
        let socket = connect(url, headers, opts)
            .await
            .map_err(Fault::transport)?;
        conn = Some(Connection {
            socket,
            created: Instant::now(),
            last_used: Instant::now(),
            continuation: None,
        });
    }
    let Some(mut connection) = conn else {
        return Err(Fault::transport(Error::Protocol(
            "WebSocket connection unavailable".into(),
        )));
    };
    let allow_delta = matches!(
        opts.transport,
        Some(Transport::Auto | Transport::WebsocketCached)
    ) && !opts.disable_continuation;
    let maybe_delta = if allow_delta {
        connection
            .continuation
            .as_ref()
            .and_then(|previous| pick_delta(&req.body, previous))
    } else {
        None
    };
    let chosen = maybe_delta.as_ref().unwrap_or(&req.body);
    let mut wire = json!({"type":"response.create"});
    if let Some(w) = wire.as_object_mut()
        && let Some(body) = chosen.as_object()
    {
        w.extend(body.clone());
    }
    let wire = serde_json::to_string(&wire).map_err(|e| Fault::provider(e.into()))?;
    abortable(&opts.request.signal, async {
        connection
            .socket
            .send(Message::Text(wire.into()))
            .await
            .map_err(|e| Error::Protocol(e.to_string()))
    })
    .await
    .map_err(Fault::transport)?;
    let mut decoder = ResponsesEvents::default();
    while !decoder.terminal() {
        let next = abortable(&opts.request.signal, async {
            let next = connection.socket.next();
            let timeout = opts.request.timeout_ms.unwrap_or(0);
            let item = if timeout == 0 {
                next.await
            } else {
                tokio::time::timeout(Duration::from_millis(timeout), next)
                    .await
                    .map_err(|_| {
                        Error::Protocol(format!("WebSocket idle timeout after {timeout}ms"))
                    })?
            };
            item.transpose().map_err(|e| Error::Protocol(e.to_string()))
        })
        .await
        .map_err(Fault::transport)?;
        let Some(message) = next else {
            return Err(Fault::transport(Error::Protocol(
                "Codex WebSocket disconnected".into(),
            )));
        };
        let mut raw = match message {
            Message::Text(data) => serde_json::from_str::<Value>(&data),
            Message::Binary(data) => serde_json::from_slice::<Value>(&data),
            Message::Close(frame) => {
                return Err(Fault::transport(Error::Protocol(format!(
                    "Codex WebSocket closed{}",
                    frame.map_or_else(String::new, |f| format!(
                        " ({}): {}",
                        u16::from(f.code),
                        f.reason
                    ))
                ))));
            }
            _ => continue,
        }
        .map_err(|e| Fault::provider(e.into()))?;
        if let Some(watch) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, watch(&raw, model))
                .await
                .map_err(Fault::provider)?;
        }
        let kind = raw["type"].as_str();
        if kind == Some("error") {
            let code = raw["code"]
                .as_str()
                .or_else(|| raw["error"]["code"].as_str());
            let message = raw["message"]
                .as_str()
                .or_else(|| raw["error"]["message"].as_str())
                .or(code);
            let kind = match code {
                Some("websocket_connection_limit_reached") => FaultKind::Limit,
                Some("previous_response_not_found") => FaultKind::Continuation,
                _ => FaultKind::Provider,
            };
            return Err(Fault {
                kind,
                error: Error::Protocol(format!(
                    "Codex error: {}",
                    message.map_or_else(|| raw.to_string(), str::to_owned)
                )),
            });
        }
        if kind.is_none() {
            continue;
        }
        state.start();
        if raw["type"] == "response.done" {
            raw["type"] = json!("response.completed");
        }
        decoder
            .accept(&raw, state, model, opts, Dialect::Codex, &req.grammar)
            .map_err(Fault::provider)?;
    }
    let reason = decoder.finish(state).map_err(Fault::provider)?;
    connection.last_used = Instant::now();
    if allow_delta && let Some(response_id) = decoder.response_id {
        let items = replay_assistant(&read_partial(&state.message), model, &req.grammar, 0)
            .map_err(Fault::provider)?;
        connection.continuation = Some(Continuation {
            request: req.body.clone(),
            response_id,
            items,
        });
    }
    if let Some(guard) = guard.as_deref_mut() {
        *guard = Some(connection);
    }
    if let Some(entry) = entry {
        let weak = Arc::downgrade(entry);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE).await;
            if let Some(entry) = weak.upgrade() {
                let mut entry = entry.lock().await;
                if entry
                    .as_ref()
                    .is_some_and(|conn| conn.last_used.elapsed() >= IDLE)
                {
                    entry.take();
                }
            }
        });
    }
    Ok(reason)
}

async fn run(
    state: &mut ChatState,
    model: &Model,
    context: TranscriptContext,
    opts: &StreamOptions,
) -> Result<StopReason> {
    let key = check_key(model, opts)?;
    let acct = extract_account_id(key)?;
    let mut request = build_body(model, context, opts, Dialect::Codex)?;
    write_partial(&state.message)
        .provider_thinking_level
        .clone_from(&request.provider_effort);
    request.body = payload(request.body, model, &opts.request).await?;
    let session = if opts.cache_retention == Some(CacheRetention::None) {
        None
    } else {
        opts.session_id.as_deref().filter(|s| !s.is_empty())
    };
    let id = session.map(|s| s.chars().take(64).collect::<String>());
    let url = codex_url(&model.base_url)?;
    let transport = opts.transport.unwrap_or(Transport::Auto);
    let fallback = session.is_some_and(|s| {
        FALLBACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(s)
    });
    if transport != Transport::Sse && !fallback {
        let mut ws_url = url.clone();
        let scheme = match ws_url.scheme() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            _ => return Err(Error::Protocol("Unsupported Codex WebSocket scheme".into())),
        };
        ws_url
            .set_scheme(scheme)
            .map_err(|_| Error::Protocol("Unsupported Codex WebSocket scheme".into()))?;
        let ws_id = id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let entry = session.map(|s| pooled(s, &acct));
        let mut tried_limit = false;
        let mut tried_continuation = false;
        loop {
            match websocket(
                &ws_url,
                headers(model, opts, key, &acct, Some(&ws_id), true)?,
                model,
                opts,
                state,
                &request,
                entry.as_ref(),
            )
            .await
            {
                Ok(reason) => return Ok(reason),
                Err(fault) if matches!(fault.error, Error::Aborted) => return Err(Error::Aborted),
                Err(fault)
                    if fault.kind == FaultKind::Limit && !tried_limit && !state.started() =>
                {
                    tried_limit = true
                }
                Err(fault)
                    if fault.kind == FaultKind::Continuation
                        && !tried_continuation
                        && !state.started() =>
                {
                    tried_continuation = true
                }
                Err(fault)
                    if !state.started()
                        && matches!(fault.kind, FaultKind::Limit | FaultKind::Transport) =>
                {
                    if let Some(session) = session {
                        FALLBACK
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(session.into());
                    }
                    break;
                }
                Err(fault) => return Err(fault.error),
            }
        }
    }
    let mut headers = headers(model, opts, key, &acct, id.as_deref(), false)?;
    let json = serde_json::to_vec(&request.body)?;
    let body = match zstd::bulk::compress(&json, 3) {
        Ok(body) => {
            headers.insert(
                "content-encoding",
                reqwest::header::HeaderValue::from_static("zstd"),
            );
            body
        }
        Err(_) => json,
    };
    let response = send_sse(url, headers, body.into(), model, opts).await?;
    consume_sse(
        response,
        state,
        model,
        opts,
        Dialect::Codex,
        &request.grammar,
    )
    .await
}

fn sse_retry_delay(headers: &HeaderMap, opts: &StreamOptions, attempt: u32) -> Result<Duration> {
    let header = |key| headers.get(key).and_then(|value| value.to_str().ok());
    let numeric = |raw: &str| {
        raw.trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
    };
    let requested = header("retry-after-ms").and_then(numeric).or_else(|| {
        let value = header("retry-after")?;
        if let Some(seconds) = numeric(value) {
            Some(seconds * 1000.0)
        } else {
            httpdate::parse_http_date(value).ok().map(|date| {
                date.duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1000.0
            })
        }
    });
    let Some(ms) = requested else {
        return Ok(Duration::from_secs(1u64 << attempt.min(16)));
    };
    let cap = opts.request.max_retry_delay_ms.unwrap_or(60_000);
    if cap > 0 && ms > cap as f64 {
        return Err(Error::Protocol(format!(
            "Server requested {}s retry delay (max: {}s)",
            (ms / 1000.0).ceil(),
            cap.div_ceil(1000)
        )));
    }
    Duration::try_from_secs_f64(ms.max(0.0) / 1000.0)
        .map_err(|_| Error::Protocol("Invalid Codex retry delay".into()))
}

fn sse_error(status: StatusCode, body: &str) -> String {
    let fallback = if body.is_empty() {
        status.canonical_reason().unwrap_or("Request failed")
    } else {
        body
    };
    if let Ok(doc) = serde_json::from_str::<Value>(body)
        && let Some(error) = doc.get("error").filter(|v| v.is_object())
    {
        let code = error["code"]
            .as_str()
            .or_else(|| error["type"].as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if status == StatusCode::TOO_MANY_REQUESTS
            || [
                "usage_limit_reached",
                "usage_not_included",
                "rate_limit_exceeded",
            ]
            .iter()
            .any(|kind| code.contains(kind))
        {
            let mut message = "You have hit your ChatGPT usage limit".to_owned();
            if let Some(plan) = error["plan_type"].as_str().filter(|s| !s.is_empty()) {
                message.push_str(&format!(" ({} plan)", plan.to_ascii_lowercase()));
            }
            message.push('.');
            if let Some(reset) = error["resets_at"].as_f64().filter(|v| *v > 0.0) {
                let minutes = ((reset * 1000.0 - crate::types::unix_millis() as f64) / 60_000.0)
                    .round()
                    .max(0.0);
                message.push_str(&format!(" Try again in ~{minutes:.0} min."));
            }
            return message;
        }
        return error["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_owned();
    }
    fallback.to_owned()
}

async fn send_sse(
    url: Url,
    headers: HeaderMap,
    body: bytes::Bytes,
    model: &Model,
    opts: &StreamOptions,
) -> Result<Response> {
    let client = client_for(&url, &opts.request, false)?;
    let signal = &opts.request.signal;
    let attempts = opts.request.max_retries.unwrap_or_default();
    for attempt in 0..=attempts {
        let work = client
            .post(url.clone())
            .headers(headers.clone())
            .body(body.clone())
            .send();
        let result = abortable(signal, async {
            if let Some(ms) = opts.request.timeout_ms.filter(|v| *v > 0) {
                tokio::time::timeout(Duration::from_millis(ms), work)
                    .await
                    .map_err(|_| {
                        Error::Protocol(format!(
                            "Codex SSE response headers timed out after {ms}ms"
                        ))
                    })?
                    .map_err(Error::from)
            } else {
                Ok(work.await?)
            }
        })
        .await;
        let (error, delay) = match result {
            Err(error) => {
                if attempt == attempts || matches!(error, Error::Aborted) {
                    return Err(error);
                }
                (error, Duration::from_secs(1u64 << attempt.min(16)))
            }
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                if let Some(observer) = &opts.request.on_response {
                    let meta = ProviderResponse {
                        status: status.as_u16(),
                        headers: crate::utils::headers_to_record(&headers),
                    };
                    abortable(signal, observer(&meta, model)).await?;
                }
                if status.is_success() {
                    return Ok(response);
                }
                let body = abortable(signal, async { Ok(response.text().await?) }).await?;
                let detail = sse_error(status, &body);
                let error = Error::Protocol(detail);
                if attempt == attempts {
                    return Err(error);
                }
                let raw = body.to_ascii_lowercase();
                if status == StatusCode::TOO_MANY_REQUESTS
                    && [
                        "gousagelimiterror",
                        "freeusagelimiterror",
                        "monthly usage limit reached",
                        "available balance",
                        "insufficient_quota",
                        "out of budget",
                        "quota exceeded",
                        "billing",
                    ]
                    .iter()
                    .any(|s| raw.contains(s))
                {
                    return Err(error);
                }
                let retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
                    || [
                        "rate_limit",
                        "rate limit",
                        "overloaded",
                        "service unavailable",
                        "upstream connect",
                        "connection refused",
                    ]
                    .iter()
                    .any(|s| raw.contains(s));
                let delay = if retryable {
                    sse_retry_delay(&headers, opts, attempt)?
                } else {
                    if error.to_string().contains("usage limit") {
                        return Err(error);
                    }
                    Duration::from_secs(1u64 << attempt.min(16))
                };
                (error, delay)
            }
        };
        if attempt == attempts {
            return Err(error);
        }
        abortable(signal, async {
            tokio::time::sleep(delay).await;
            Ok(())
        })
        .await?;
    }
    Err(Error::Protocol("Failed after retries".into()))
}
