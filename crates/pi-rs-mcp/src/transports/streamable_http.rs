//! The Streamable HTTP transport (`packages/mcp/src/transports/streamable-http.ts`): JSON and SSE
//! responses, session IDs, the server-to-client GET stream with reconnection, resumption of
//! dropped response streams with `Last-Event-ID`, retries and cancellation, and auth headers.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures_util::StreamExt;
use url::Url;

use crate::abort::{AbortSignal, abort_timeout};
use crate::auth_provider::{AuthProvider, McpFetch, UnauthorizedContext};
use crate::fetch::{
    McpFetchError, McpFetchRequest, McpFetchResponse, StreamingDecoder, read_body_text,
};
use crate::protocol::{
    JsonRpcId, JsonRpcMessage, McpErrorEvent, McpTransportError, parse_json, parse_json_rpc_message,
};
use crate::transports::{
    DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportSendOptions, Subscription, TransportEvents,
};

const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_MESSAGE_BODY_CHARS: usize = 500;
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
const DEFAULT_RECONNECT_MAX_RETRIES: u32 = 5;

/// One parsed SSE event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

type OnId = Arc<dyn Fn(&str) + Send + Sync>;
type OnMessage = Arc<dyn Fn(&JsonRpcMessage) + Send + Sync>;

/// Options of [`consume_sse_stream`].
pub struct ConsumeSseOptions {
    pub max_event_bytes: Option<u64>,
    pub on_event: Arc<dyn Fn(&SseEvent) + Send + Sync>,
    /// Called for every `id` field, including events without data (for example resumption priming
    /// events).
    pub on_id: Option<OnId>,
    /// Called for every valid `retry` field, in milliseconds.
    pub on_retry: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

/// Reads an SSE byte stream to its end, reporting every event. Fails with the message of the first
/// violation; the stream is dropped, which cancels it, like the `reader.cancel()` upstream does.
pub async fn consume_sse_stream(
    mut stream: crate::fetch::ByteStream,
    options: ConsumeSseOptions,
) -> Result<(), String> {
    let max_event_bytes = options.max_event_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES) as usize;
    let mut state = SseState {
        decoder: StreamingDecoder::default(),
        buffered: String::new(),
        skip_leading_lf: false,
        event_name: None,
        event_id: None,
        data_lines: Vec::new(),
        data_bytes: 0,
        max_event_bytes,
    };
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        let mut text = state.decoder.push(&chunk);
        // A CRLF split across chunks: the CR already ended the line, so drop the LF.
        if state.skip_leading_lf && !text.is_empty() {
            if let Some(rest) = text.strip_prefix('\n') {
                text = rest.to_string();
            }
            state.skip_leading_lf = false;
        }
        state.buffered.push_str(&text);
        state.consume_lines(&options)?;
        if state.buffered.len() > max_event_bytes {
            return Err(format!("MCP SSE event exceeds {max_event_bytes} bytes"));
        }
    }
    let tail = state.decoder.finish();
    if state.skip_leading_lf {
        if let Some(rest) = tail.strip_prefix('\n') {
            state.buffered.push_str(rest);
        }
    } else {
        state.buffered.push_str(&tail);
    }
    state.consume_lines(&options)?;
    if !state.buffered.is_empty() {
        let line = std::mem::take(&mut state.buffered);
        state.process_line(&line, &options)?;
    }
    state.dispatch(&options);
    Ok(())
}

/// The SSE parsing state, kept across chunks.
struct SseState {
    decoder: StreamingDecoder,
    buffered: String,
    skip_leading_lf: bool,
    event_name: Option<String>,
    event_id: Option<String>,
    data_lines: Vec<String>,
    /// Bytes of the pending event's data, including the "\n" joins, so events streamed as many
    /// short `data:` lines without a terminating blank line cannot grow without bound.
    data_bytes: usize,
    max_event_bytes: usize,
}

impl SseState {
    fn dispatch(&mut self, options: &ConsumeSseOptions) {
        if self.data_lines.is_empty() {
            self.event_name = None;
            self.event_id = None;
            return;
        }
        let data = self.data_lines.join("\n");
        let event = SseEvent {
            event: self.event_name.take(),
            data,
            id: self.event_id.take(),
        };
        self.data_lines.clear();
        self.data_bytes = 0;
        (options.on_event)(&event);
    }

    fn process_line(&mut self, line: &str, options: &ConsumeSseOptions) -> Result<(), String> {
        if line.is_empty() {
            self.dispatch(options);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (
                &line[..colon],
                line[colon + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&line[colon + 1..]),
            ),
            None => (line, ""),
        };
        match field {
            "data" => {
                self.data_bytes += value.len() + usize::from(!self.data_lines.is_empty());
                if self.data_bytes > self.max_event_bytes {
                    return Err(format!(
                        "MCP SSE event exceeds {} bytes",
                        self.max_event_bytes
                    ));
                }
                self.data_lines.push(value.to_string());
            }
            "event" => self.event_name = Some(value.to_string()),
            "id" if !value.contains('\0') => {
                self.event_id = Some(value.to_string());
                if let Some(on_id) = &options.on_id {
                    on_id(value);
                }
            }
            "retry" if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) => {
                if let Some(on_retry) = &options.on_retry
                    && let Ok(delay) = value.parse::<u64>()
                {
                    on_retry(delay);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// SSE lines end with CRLF, LF, or a lone CR.
    fn consume_lines(&mut self, options: &ConsumeSseOptions) -> Result<(), String> {
        let mut start = 0;
        let mut index = 0;
        let bytes = self.buffered.as_bytes().to_vec();
        while index < bytes.len() {
            let character = bytes[index];
            if character != b'\r' && character != b'\n' {
                index += 1;
                continue;
            }
            let line = self.buffered[start..index].to_string();
            self.process_line(&line, options)?;
            if character == b'\r' {
                if index + 1 < bytes.len() {
                    if bytes[index + 1] == b'\n' {
                        index += 1;
                    }
                } else {
                    self.skip_leading_lf = true;
                }
            }
            start = index + 1;
            index += 1;
        }
        self.buffered.drain(..start);
        Ok(())
    }
}

/// Reconnection of dropped SSE streams (the GET stream, and response streams that carry event IDs).
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamableHttpReconnectOptions {
    /// Delay before the first reconnection attempt, unless the server sent a `retry` field.
    /// Default: 1000.
    pub initial_delay_ms: Option<u64>,
    /// Upper bound for the exponential backoff. Default: 30000.
    pub max_delay_ms: Option<u64>,
    /// Consecutive failed attempts before giving up on a stream. Default: 5.
    pub max_retries: Option<u32>,
}

/// Options of [`StreamableHttpTransport`].
#[derive(Clone, Default)]
pub struct StreamableHttpTransportOptions {
    pub url: String,
    pub headers: Option<indexmap::IndexMap<String, String>>,
    pub fetch: Option<McpFetch>,
    /// Open the server-to-client GET stream after initialization. Default: true.
    pub open_get_stream: Option<bool>,
    pub max_message_bytes: Option<u64>,
    pub auth_provider: Option<Arc<dyn AuthProvider>>,
    pub reconnect: Option<StreamableHttpReconnectOptions>,
}

/// Where a response stream left off, for resuming it.
#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    /// Whether the stream delivered any event since it was (re)opened.
    received: bool,
}

/// The Streamable HTTP transport.
pub struct StreamableHttpTransport {
    events: Arc<TransportEvents>,
    url: Url,
    options: StreamableHttpTransportOptions,
    fetch: McpFetch,
    controller: AbortSignal,
    /// The transport's own handle, for the streams it runs in the background.
    weak: Mutex<Option<std::sync::Weak<StreamableHttpTransport>>>,
    started: AtomicBool,
    closed: AtomicBool,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    get_stream_started: AtomicBool,
    /// Access token of the latest request, which `close()` reuses instead of asking the auth
    /// provider.
    last_token: Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    /// Creates the transport. Fails like upstream's `new URL(options.url)` does on an invalid URL.
    pub fn new(options: StreamableHttpTransportOptions) -> Result<Arc<Self>, McpTransportError> {
        let url = Url::parse(&options.url)
            .map_err(|_| McpTransportError::Message("Invalid URL".to_string()))?;
        let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
        let this = Arc::new(Self {
            events: Arc::new(TransportEvents::default()),
            url,
            fetch,
            controller: AbortSignal::default(),
            weak: Mutex::new(None),
            options,
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            get_stream_started: AtomicBool::new(false),
            last_token: Mutex::new(None),
        });
        *this
            .weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(&this));
        Ok(this)
    }

    /// The session the server assigned, once it assigned one.
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Fetch with auth headers. A 401 (or a 403 asking for more scope) is handed to the auth
    /// provider once, and the request is retried with whatever credentials it left behind.
    ///
    /// `signal` only stops dispatch: getting a token or the authorization recovery can take a
    /// while, and a request its sender gave up on in the meantime must not reach the server. It
    /// does not abort the shared recovery, which other requests may wait for.
    async fn authorized_fetch(
        &self,
        method: &str,
        extra_headers: Vec<(String, String)>,
        body: Option<String>,
        signal: Option<&AbortSignal>,
    ) -> Result<McpFetchResponse, McpTransportError> {
        let auth_provider = self.options.auth_provider.clone();
        let mut attempt = 0;
        loop {
            let (headers, token) = self.headers(extra_headers.clone()).await?;
            if signal.is_some_and(AbortSignal::aborted) {
                return Err(McpTransportError::Aborted);
            }
            let mut response = self.call_fetch(method, headers, body.clone()).await?;
            let needs_recovery = attempt == 0 && needs_authorization(&response);
            if !needs_recovery {
                return Ok(response);
            }
            let Some(provider) = auth_provider.as_ref() else {
                return Ok(response);
            };
            let context = UnauthorizedContext {
                status: response.status,
                headers: response.headers.clone(),
                server_url: self.url.clone(),
                fetch: self.fetch.clone(),
                token,
            };
            // The response is dropped before the recovery runs, like the `discard` upstream.
            response.discard();
            provider
                .on_unauthorized(context)
                .await
                .map_err(|error| McpTransportError::Message(error.to_string()))?;
            attempt += 1;
        }
    }

    async fn call_fetch(
        &self,
        method: &str,
        headers: Vec<(String, String)>,
        body: Option<String>,
    ) -> Result<McpFetchResponse, McpTransportError> {
        let request = McpFetchRequest {
            url: self.url.to_string(),
            method: http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::GET),
            headers,
            body,
            signal: Some(self.controller.clone()),
        };
        self.fetch.call(request).await.map_err(|error| match error {
            McpFetchError::Network(message) => McpTransportError::Network(message),
            McpFetchError::Timeout(message) => McpTransportError::Message(message),
            McpFetchError::Other(message) => McpTransportError::Message(message),
        })
    }

    async fn headers(
        &self,
        extra: Vec<(String, String)>,
    ) -> Result<(Vec<(String, String)>, Option<String>), McpTransportError> {
        let token = match &self.options.auth_provider {
            Some(provider) => provider
                .token()
                .await
                .map_err(|error| McpTransportError::Message(error.to_string()))?,
            None => None,
        };
        *self
            .last_token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = token.clone();
        let headers = self.build_headers(extra, token.as_deref());
        Ok((headers, token))
    }

    fn build_headers(
        &self,
        extra: Vec<(String, String)>,
        token: Option<&str>,
    ) -> Vec<(String, String)> {
        let mut headers = self
            .options
            .headers
            .as_ref()
            .map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (name, value) in extra {
            if let Some(existing) = headers
                .iter_mut()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
            {
                existing.1 = value;
            } else {
                headers.push((name, value));
            }
        }
        if let Some(session) = self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            set_header(&mut headers, "Mcp-Session-Id", session);
        }
        if let Some(version) = self
            .protocol_version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            set_header(&mut headers, "MCP-Protocol-Version", version);
        }
        if let Some(token) = token {
            set_header(&mut headers, "Authorization", &format!("Bearer {token}"));
        }
        headers
    }

    fn capture_session(&self, response: &McpFetchResponse) {
        if let Some(session) = response.header("mcp-session-id") {
            *self
                .session_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(session.to_string());
        }
    }

    async fn check_response(
        &self,
        response: &mut McpFetchResponse,
    ) -> Result<(), McpTransportError> {
        if response.ok() {
            return Ok(());
        }
        // Reads only the excerpt the error keeps and cancels the rest, so a huge error body is
        // never buffered.
        let body = read_body_text(response, MAX_ERROR_BODY_BYTES)
            .await
            .map(|(text, _)| text)
            .unwrap_or_default();
        if response.status == 401 {
            return Err(McpTransportError::AuthRequired {
                www_authenticate: response.header("www-authenticate").map(str::to_string),
                body,
            });
        }
        if response.status == 404 && self.session_id().is_some() {
            return Err(McpTransportError::SessionExpired { body });
        }
        Err(McpTransportError::Http {
            status: response.status,
            message: describe_http_failure(response.status, &body),
            body,
        })
    }

    async fn consume_sse(
        &self,
        stream: crate::fetch::ByteStream,
        cursor: Arc<Mutex<StreamCursor>>,
        on_message: Option<OnMessage>,
    ) -> Result<(), String> {
        let events = self.events.clone();
        let max_event_bytes = self.options.max_message_bytes;
        let on_id = {
            let cursor = cursor.clone();
            Arc::new(move |id: &str| {
                cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .last_event_id = Some(id.to_string());
            }) as Arc<dyn Fn(&str) + Send + Sync>
        };
        let on_retry = {
            let cursor = cursor.clone();
            Arc::new(move |delay_ms: u64| {
                cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .retry_ms = Some(delay_ms);
            }) as Arc<dyn Fn(u64) + Send + Sync>
        };
        let on_event = Arc::new(move |event: &SseEvent| {
            {
                let mut cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.received = true;
            }
            // Events without data prime resumption; other event types are not JSON-RPC.
            if event.data.trim().is_empty() {
                return;
            }
            if event.event.as_ref().is_some_and(|kind| kind != "message") {
                return;
            }
            let message = match parse_json(&event.data) {
                Ok(value) => value,
                Err(error) => {
                    events.emit_error(McpErrorEvent::message(error));
                    return;
                }
            };
            let message = match parse_json_rpc_message(&message) {
                Ok(message) => message,
                Err(error) => {
                    events.emit_error(McpErrorEvent::from_mcp(&error));
                    return;
                }
            };
            if let Some(on_message) = &on_message {
                on_message(&message);
            }
            events.emit_message(&message);
        });
        consume_sse_stream(
            stream,
            ConsumeSseOptions {
                max_event_bytes,
                on_event,
                on_id: Some(on_id),
                on_retry: Some(on_retry),
            },
        )
        .await
    }

    /// Read the SSE stream answering one request. When the stream ends or breaks before the
    /// response arrives and the server assigned event IDs, resume it with GET and `Last-Event-ID`,
    /// as the server may close response streams at will. Otherwise only this request fails.
    async fn consume_response_stream(&self, body: crate::fetch::ByteStream, request_id: JsonRpcId) {
        let cursor = Arc::new(Mutex::new(StreamCursor::default()));
        let answered = Arc::new(AtomicBool::new(false));
        let on_message = {
            let answered = answered.clone();
            let watched_id = request_id.clone();
            Arc::new(move |message: &JsonRpcMessage| {
                if let crate::protocol::JsonRpcMessage::Response(response) = message
                    && response.id() == &watched_id
                {
                    answered.store(true, Ordering::SeqCst);
                }
            }) as Arc<dyn Fn(&JsonRpcMessage) + Send + Sync>
        };
        let mut stream = Some(body);
        let mut failure: Option<McpTransportError> = None;
        let mut attempt: u32 = 0;
        loop {
            if let Some(open) = stream.take() {
                match self
                    .consume_sse(open, cursor.clone(), Some(on_message.clone()))
                    .await
                {
                    Ok(()) => failure = None,
                    Err(error) => failure = Some(McpTransportError::Message(error)),
                }
            }
            if answered.load(Ordering::SeqCst) || self.closed() {
                return;
            }
            if failure.is_some() && !self.is_retryable(failure.as_ref()) {
                break;
            }
            let (last_event_id, received) = {
                let cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (cursor.last_event_id.clone(), cursor.received)
            };
            if last_event_id.is_none() || attempt >= self.max_retries() {
                break;
            }
            if received {
                attempt = 0;
            }
            {
                let mut cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.received = false;
            }
            let delay = self.reconnect_delay(attempt, {
                let cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.retry_ms
            });
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
            match self.open_sse_stream(last_event_id).await {
                Ok(next) => stream = next,
                Err(error) => {
                    failure = Some(error);
                    if !self.is_retryable(failure.as_ref()) {
                        break;
                    }
                }
            }
        }
        if self.closed() {
            return;
        }
        let reason = match &failure {
            None => "stream ended without a response".to_string(),
            Some(error) => error.to_string(),
        };
        let message = crate::protocol::JsonRpcMessage::Response(
            crate::protocol::JsonRpcResponse::Error(crate::protocol::JsonRpcErrorResponse {
                jsonrpc: crate::protocol::JsonRpcVersion,
                id: request_id,
                error: crate::protocol::JsonRpcErrorObject {
                    code: crate::protocol::json_rpc_error_codes::INTERNAL_ERROR,
                    message: format!("MCP response stream failed: {reason}"),
                    data: None,
                },
                extra: serde_json::Map::new(),
            }),
        );
        self.events.emit_message(&message);
    }

    fn start_get_stream(&self) {
        if self.options.open_get_stream == Some(false)
            || self.get_stream_started.swap(true, Ordering::SeqCst)
            || self.closed()
        {
            return;
        }
        let this = self.weak_self();
        tokio::spawn(async move {
            if let Some(this) = this.upgrade() {
                this.run_get_stream().await;
            }
        });
    }

    /// Keep the server-to-client stream open, reconnecting with backoff when it drops.
    async fn run_get_stream(&self) {
        let cursor = Arc::new(Mutex::new(StreamCursor::default()));
        let mut attempt: u32 = 0;
        while !self.closed() {
            let last_event_id = {
                let cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.last_event_id.clone()
            };
            let opened = self.open_sse_stream(last_event_id).await;
            match opened {
                Err(error) => {
                    if self.closed() {
                        return;
                    }
                    if !self.is_retryable(Some(&error)) {
                        self.events
                            .emit_error(McpErrorEvent::from_transport(&error));
                        return;
                    }
                }
                Ok(None) => return,
                Ok(Some(stream)) => {
                    let opened_at = std::time::Instant::now();
                    let consumed = self.consume_sse(stream, cursor.clone(), None).await;
                    let received = {
                        let cursor = cursor
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        cursor.received
                    };
                    if let Err(error) = consumed {
                        if self.closed() {
                            return;
                        }
                        let failure = McpTransportError::Message(error);
                        if !self.is_retryable(Some(&failure)) {
                            self.events
                                .emit_error(McpErrorEvent::from_transport(&failure));
                            return;
                        }
                    }
                    // A stream that stayed up for a while counts as healthy, even if it was idle.
                    if received || opened_at.elapsed() > Duration::from_millis(self.max_delay_ms())
                    {
                        attempt = 0;
                    }
                }
            }
            {
                let mut cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.received = false;
            }
            if attempt >= self.max_retries() {
                self.events.emit_error(McpErrorEvent::message(
                    "MCP server-to-client stream dropped and could not be reopened",
                ));
                return;
            }
            let delay = self.reconnect_delay(attempt, {
                let cursor = cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cursor.retry_ms
            });
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
        }
    }

    /// Open a GET SSE stream. `Ok(None)` when the server answers 405 (no GET stream).
    async fn open_sse_stream(
        &self,
        last_event_id: Option<String>,
    ) -> Result<Option<crate::fetch::ByteStream>, McpTransportError> {
        let mut extra = vec![("accept".to_string(), "text/event-stream".to_string())];
        if let Some(last_event_id) = last_event_id {
            extra.push(("last-event-id".to_string(), last_event_id));
        }
        let mut response = self.authorized_fetch("GET", extra, None, None).await?;
        if response.status == 405 {
            response.discard();
            return Ok(None);
        }
        self.check_response(&mut response).await?;
        self.capture_session(&response);
        let kind = response.content_type();
        if kind.as_deref() != Some("text/event-stream") || response.body.is_none() {
            response.discard();
            return Err(McpTransportError::Http {
                status: response.status,
                message: format!(
                    "Unsupported MCP GET response content type: {}",
                    kind.unwrap_or_else(|| "missing".to_string())
                ),
                body: String::new(),
            });
        }
        Ok(response.body.take())
    }

    /// Network failures and transient statuses are retried; auth, session, and protocol errors are
    /// not. A fetch reports network failures, including a connection dropped mid-body, as the
    /// retryable kind (upstream: `TypeError`).
    fn is_retryable(&self, error: Option<&McpTransportError>) -> bool {
        match error {
            Some(McpTransportError::Http { status, .. }) => is_transient_status(*status),
            Some(McpTransportError::Network(_)) => true,
            _ => false,
        }
    }

    fn reconnect_delay(&self, attempt: u32, server_delay_ms: Option<u64>) -> u64 {
        if let Some(server_delay) = server_delay_ms {
            return server_delay;
        }
        let initial = self
            .options
            .reconnect
            .and_then(|reconnect| reconnect.initial_delay_ms)
            .unwrap_or(DEFAULT_RECONNECT_INITIAL_DELAY_MS);
        let shift = attempt.min(63);
        let backoff = initial.saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX));
        backoff.min(self.max_delay_ms())
    }

    fn max_delay_ms(&self) -> u64 {
        self.options
            .reconnect
            .and_then(|reconnect| reconnect.max_delay_ms)
            .unwrap_or(DEFAULT_RECONNECT_MAX_DELAY_MS)
    }

    fn max_retries(&self) -> u32 {
        self.options
            .reconnect
            .and_then(|reconnect| reconnect.max_retries)
            .unwrap_or(DEFAULT_RECONNECT_MAX_RETRIES)
    }

    /// Resolves false when the transport closed while waiting.
    async fn sleep(&self, ms: u64) -> bool {
        if self.controller.aborted() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.controller.clone().wait_aborted() => false,
            _ = tokio::time::sleep(Duration::from_millis(ms)) => true,
        }
    }

    fn weak_self(&self) -> std::sync::Weak<StreamableHttpTransport> {
        self.weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_default()
    }
}

/// Whether a response needs the auth provider: 401, or 403 with an `insufficient_scope` bearer
/// challenge (step-up authorization).
fn needs_authorization(response: &McpFetchResponse) -> bool {
    if response.status == 401 {
        return true;
    }
    if response.status != 403 {
        return false;
    }
    let Some(challenge) = response.header("www-authenticate") else {
        return false;
    };
    // Upstream's regex is permissive about what follows the scope: any token starting with it.
    challenge
        .split([',', ' ', '\t'])
        .filter(|token| !token.is_empty())
        .any(|token| {
            let mut parts = token.splitn(2, '=');
            match parts.next() {
                Some(field) if field.eq_ignore_ascii_case("error") => {
                    parts.next().is_some_and(|value| {
                        let value = value.strip_prefix('"').unwrap_or(value);
                        value.to_ascii_lowercase().starts_with("insufficient_scope")
                    })
                }
                _ => false,
            }
        })
}

/// Statuses worth retrying when a stream fails to (re)open.
fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

fn describe_http_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if text.chars().count() > ERROR_MESSAGE_BODY_CHARS {
        let cut: String = text.chars().take(ERROR_MESSAGE_BODY_CHARS - 3).collect();
        format!("{cut}...")
    } else {
        text.to_string()
    };
    if snippet.is_empty() {
        format!("MCP HTTP request failed with status {status}")
    } else {
        format!("MCP HTTP request failed with status {status}: {snippet}")
    }
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        existing.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

#[async_trait::async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Message(
                "MCP Streamable HTTP transport already started".to_string(),
            ));
        }
        if self.closed() {
            return Err(McpTransportError::connection_closed());
        }
        Ok(())
    }

    async fn send(
        &self,
        message: &JsonRpcMessage,
        options: &McpTransportSendOptions,
    ) -> Result<(), McpTransportError> {
        if !self.started.load(Ordering::SeqCst) || self.closed() {
            return Err(McpTransportError::connection_closed());
        }
        let mut response = self
            .authorized_fetch(
                "POST",
                vec![
                    (
                        "accept".to_string(),
                        "application/json, text/event-stream".to_string(),
                    ),
                    ("content-type".to_string(), "application/json".to_string()),
                ],
                Some(serde_json::to_string(message).unwrap_or_default()),
                options.signal.as_ref(),
            )
            .await?;
        self.check_response(&mut response).await?;
        self.capture_session(&response);

        let is_request = matches!(message, JsonRpcMessage::Request(_));
        let method = match message {
            JsonRpcMessage::Request(request) => Some(request.method.clone()),
            JsonRpcMessage::Notification(notification) => Some(notification.method.clone()),
            JsonRpcMessage::Response(_) => None,
        };
        if !is_request {
            // Notifications and responses are acknowledged with 202 and carry no reply; ignore any
            // body.
            response.discard();
            // The server-to-client stream may only open once the session is initialized.
            if method.as_deref() == Some("notifications/initialized") {
                self.start_get_stream();
            }
            return Ok(());
        }
        let request_id = match message {
            JsonRpcMessage::Request(request) => request.id.clone(),
            _ => return Ok(()),
        };
        if response.status == 202 || response.status == 204 {
            return Err(McpTransportError::Http {
                status: response.status,
                message: format!(
                    "MCP server accepted request {} without a response",
                    method.unwrap_or_default()
                ),
                body: String::new(),
            });
        }
        let kind = response.content_type();
        if kind.as_deref() == Some("application/json") {
            let max_bytes = self
                .options
                .max_message_bytes
                .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES) as usize;
            let (text, truncated) = read_body_text(&mut response, max_bytes)
                .await
                .map_err(|error| McpTransportError::Network(error.to_string()))?;
            if truncated {
                return Err(McpTransportError::Message(format!(
                    "MCP JSON response exceeds {max_bytes} bytes"
                )));
            }
            let body = parse_json(&text).map_err(McpTransportError::Message)?;
            let items = body.as_array().cloned().unwrap_or_else(|| vec![body]);
            for item in items {
                let message = parse_json_rpc_message(&item)
                    .map_err(|error| McpTransportError::Message(error.message))?;
                self.events.emit_message(&message);
            }
            return Ok(());
        }
        if kind.as_deref() == Some("text/event-stream") && response.body.is_some() {
            if let Some(body) = response.body.take() {
                let this = self.weak_self();
                tokio::spawn(async move {
                    if let Some(this) = this.upgrade() {
                        this.consume_response_stream(body, request_id).await;
                    }
                });
            }
            return Ok(());
        }
        response.discard();
        Err(McpTransportError::Http {
            status: response.status,
            message: format!(
                "Unsupported MCP response content type: {}",
                kind.unwrap_or_else(|| "missing".to_string())
            ),
            body: String::new(),
        })
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.controller.abort_with(None);
        if self.started.load(Ordering::SeqCst) && self.session_id().is_some() {
            // Best effort: the session expires on the server anyway. The auth provider may refresh
            // tokens over the network, so it is not asked here and closing never waits for a
            // refresh.
            let last_token = self
                .last_token
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let headers = self.build_headers(Vec::new(), last_token.as_deref());
            let request = McpFetchRequest {
                url: self.url.to_string(),
                method: http::Method::DELETE,
                headers,
                body: None,
                signal: Some(abort_timeout(1_000)),
            };
            if let Ok(mut response) = self.fetch.call(request).await {
                response.discard();
            }
        }
        self.events.emit_close();
    }

    fn set_protocol_version(&self, version: &str) {
        *self
            .protocol_version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(version.to_string());
    }

    fn on_message(&self, listener: crate::transports::McpTransportMessageListener) -> Subscription {
        self.events.on_message(listener)
    }

    fn on_error(&self, listener: crate::transports::McpTransportErrorListener) -> Subscription {
        self.events.on_error(listener)
    }

    fn on_close(&self, listener: crate::transports::McpTransportCloseListener) -> Subscription {
        self.events.on_close(listener)
    }
}
