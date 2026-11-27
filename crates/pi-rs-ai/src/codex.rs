use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Response, StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls,
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
type Socket = WebSocketStream<MaybeTlsStream<ProxyStream>>;
/// The stream to the provider: plain, or TLS to an HTTPS proxy.
enum ProxyStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}
impl AsyncRead for ProxyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for ProxyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}
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

/// Debug counters of the Codex WebSocket transport for one session, like upstream's
/// `OpenAICodexWebSocketDebugStats`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenAICodexWebSocketDebugStats {
    /// WebSocket requests the session started.
    pub requests: u64,
    /// WebSocket connections the session opened.
    pub connections_created: u64,
    /// Pooled WebSocket connections the session reused.
    pub connections_reused: u64,
    /// Requests that could use the cached continuation context.
    pub cached_context_requests: u64,
    /// Requests sent with `store: true`.
    pub store_true_requests: u64,
    /// Requests sent with the whole conversation.
    pub full_context_requests: u64,
    /// Requests sent as a delta of the previous response.
    pub delta_requests: u64,
    /// Number of input items of the last request.
    pub last_input_items: usize,
    /// Number of input items of the last delta request.
    pub last_delta_input_items: Option<usize>,
    /// `previous_response_id` of the last delta request.
    pub last_previous_response_id: Option<String>,
    /// WebSocket failures the session hit.
    pub websocket_failures: u64,
    /// Times the session fell back to SSE.
    pub sse_fallbacks: u64,
    /// Whether the session currently uses the SSE fallback.
    pub websocket_fallback_active: Option<bool>,
    /// The last WebSocket error of the session.
    pub last_websocket_error: Option<String>,
}
static STATS: LazyLock<Mutex<HashMap<String, OpenAICodexWebSocketDebugStats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The debug counters of a Codex WebSocket session, if the session has any.
pub fn codex_websocket_debug_stats(session_id: &str) -> Option<OpenAICodexWebSocketDebugStats> {
    STATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
}

/// Forgets the debug counters of one session, or of every session.
pub fn reset_codex_websocket_debug_stats(session_id: Option<&str>) {
    let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
    match session_id {
        Some(session) => {
            stats.remove(session);
        }
        None => stats.clear(),
    }
}
fn record_stats(session: Option<&str>, update: impl FnOnce(&mut OpenAICodexWebSocketDebugStats)) {
    let Some(session) = session else {
        return;
    };
    let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
    update(stats.entry(session.to_owned()).or_default());
}
fn record_websocket_failure(session: Option<&str>, error: &Error) {
    if let Some(session) = session {
        FALLBACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.to_owned());
    }
    record_stats(session, |stats| {
        stats.websocket_failures += 1;
        stats.last_websocket_error = Some(error.to_string());
        stats.websocket_fallback_active = Some(true);
    });
}
fn record_sse_fallback(session: Option<&str>) {
    let active = session.is_some_and(|session| {
        FALLBACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session)
    });
    record_stats(session, |stats| {
        stats.sse_fallbacks += 1;
        stats.websocket_fallback_active = Some(active);
    });
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
        match proxy {
            // Use CONNECT to keep TLS and the WebSocket handshake end-to-end with the
            // provider. An HTTPS proxy is reached over TLS itself.
            Some(proxy) => {
                let stream = proxy_tunnel(&proxy, url).await?;
                client_async_tls(req, stream)
                    .await
                    .map(|(socket, _)| socket)
                    .map_err(|e| Error::Protocol(format!("Codex WebSocket connect failed: {e}")))
            }
            None => {
                let stream = TcpStream::connect(host_port(url)?).await?;
                client_async_tls(req, ProxyStream::Plain(stream))
                    .await
                    .map(|(socket, _)| socket)
                    .map_err(|e| Error::Protocol(format!("Codex WebSocket connect failed: {e}")))
            }
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
/// The address of a URL, as `(host, port)`.
fn host_port(url: &Url) -> Result<(String, u16)> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::Protocol("WebSocket target host missing".into()))?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::Protocol("WebSocket target port missing".into()))?;
    Ok((host, port))
}
/// TLS to an HTTPS proxy, trusting the operating system's root CAs.
async fn tls_to_proxy(host: &str, stream: TcpStream) -> Result<ProxyStream> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, _ignored) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        return Err(Error::Protocol(format!(
            "no native root CA certificates found for the HTTPS proxy (errors: {:?})",
            native.errors
        )));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Protocol(format!("TLS protocol configuration failed: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = rustls_pki_types::ServerName::try_from(host.to_owned())
        .map_err(|e| Error::Protocol(format!("invalid proxy host name: {e}")))?;
    let tls = connector
        .connect(name, stream)
        .await
        .map_err(|e| Error::Protocol(format!("HTTPS proxy TLS handshake failed: {e}")))?;
    Ok(ProxyStream::Tls(Box::new(tls)))
}
async fn proxy_tunnel(proxy: &Url, target: &Url) -> Result<ProxyStream> {
    let host = proxy
        .host_str()
        .ok_or_else(|| Error::Protocol("proxy host missing".into()))?;
    let port = proxy
        .port_or_known_default()
        .ok_or_else(|| Error::Protocol("proxy port missing".into()))?;
    let tcp = TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
    let mut stream = match proxy.scheme() {
        "http" => ProxyStream::Plain(tcp),
        "https" => tls_to_proxy(host.trim_matches(['[', ']']), tcp).await?,
        other => {
            return Err(Error::Protocol(format!(
                "unsupported proxy scheme for the Codex WebSocket: {other}"
            )));
        }
    };
    let (target_host, target_port) = host_port(target)?;
    let authority = if target_host.contains(':') {
        format!("[{target_host}]:{target_port}")
    } else {
        format!("{target_host}:{target_port}")
    };
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
fn cache_session(opts: &StreamOptions) -> Option<&str> {
    if opts.cache_retention == Some(CacheRetention::None) {
        None
    } else {
        opts.session_id.as_deref().filter(|s| !s.is_empty())
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
    let session = cache_session(opts);
    let mut guard = entry.and_then(|map| map.try_lock().ok());
    let mut conn = guard
        .as_deref_mut()
        .and_then(Option::take)
        .filter(|conn| conn.created.elapsed() < MAX_AGE && conn.last_used.elapsed() < IDLE);
    let reused = conn.is_some();
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
    let input_items = chosen
        .get("input")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let previous_response_id = chosen
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let store = chosen
        .get("store")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    record_stats(session, |stats| {
        stats.requests += 1;
        if reused {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if allow_delta {
            stats.cached_context_requests += 1;
        }
        if store {
            stats.store_true_requests += 1;
        }
        stats.last_input_items = input_items;
        match previous_response_id {
            Some(previous) => {
                stats.delta_requests += 1;
                stats.last_delta_input_items = Some(input_items);
                stats.last_previous_response_id = Some(previous);
            }
            None => {
                stats.full_context_requests += 1;
                stats.last_delta_input_items = None;
                stats.last_previous_response_id = None;
            }
        }
    });
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
    let session = cache_session(opts);
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
                    record_websocket_failure(session, &fault.error);
                    record_sse_fallback(session);
                    break;
                }
                Err(fault) => {
                    record_websocket_failure(session, &fault.error);
                    return Err(fault.error);
                }
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

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn https_proxy_requires_tls_before_sending_connect() -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let proxy = Url::parse(&format!("https://{}", listener.local_addr()?))?;
        let target = Url::parse("wss://service.example/socket")?;
        let observe = async {
            let (mut stream, _) = listener.accept().await?;
            let mut header = [0u8; 3];
            stream.read_exact(&mut header).await?;
            Ok::<_, std::io::Error>(header)
        };
        let (received, tunnel) = tokio::join!(observe, proxy_tunnel(&proxy, &target));
        // TLS handshake record. The listener closes before presenting a certificate.
        let header = received?;
        assert_eq!(header[..2], [0x16, 0x03]);
        assert!(tunnel.is_err());
        Ok(())
    }
}
