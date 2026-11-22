//! Shared test support: the loopback HTTP test server (`test/helpers.ts`), the authorization
//! server fixture (`test/authorization-server.ts`), and small builders, so the suites run with
//! in-memory transports and loopback HTTP servers only.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use futures_util::StreamExt;
use http::HeaderValue;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use pi_rs_mcp::oauth::client_settings::McpOAuthServerStore;
use pi_rs_mcp::oauth::provider::{McpOAuthState, McpStoreError, MemoryOAuthStateStore};
use pi_rs_mcp::{ByteStream, McpError, McpFetch, McpFetchRequest, McpFetchResponse};

/// The fixture binary: the `.mjs` fixtures of upstream, as one executable.
pub fn fixture_exe() -> String {
    std::env::var("CARGO_BIN_EXE_mcp-test-fixture")
        .unwrap_or_else(|_| "./target/debug/mcp-test-fixture".to_string())
}

/// One recorded request of a [`TestServer`].
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// The request's query, without the `?`.
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub message: Option<Value>,
    /// The raw request body, decoded as UTF-8.
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    /// The recorded request target, with its query.
    pub fn path_with_query(&self) -> String {
        self.path.clone()
    }
}

/// What a test handler answers: a status, headers, and a body that may stream or never end.
pub struct TestResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: TestBody,
}

impl TestResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: TestBody::Full(serde_json::to_string(&body).unwrap_or_default()),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: TestBody::Full(body.to_string()),
        }
    }
}

/// A response body: complete text, streamed chunks, or nothing ever (a stalled server).
pub enum TestBody {
    Empty,
    Full(String),
    Chunks(mpsc::Receiver<Vec<u8>>),
    Hang,
}

/// A test server handler: the request in, the response out. It may await gates or spawn work.
pub type TestHandler = Arc<
    dyn Fn(RecordedRequest) -> futures_util::future::BoxFuture<'static, TestResponse> + Send + Sync,
>;

/// A loopback HTTP test server. It keeps serving until dropped; handlers record what arrived.
pub struct TestServer {
    pub origin: String,
    handler: TestHandler,
    _serve: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Starts the server; `handler` answers every request.
    pub async fn listen(handler: TestHandler) -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| error.to_string())
            .expect("the test server binds to a free port");
        let port = listener
            .local_addr()
            .map(|address| address.port())
            .unwrap_or_default();
        let origin = format!("http://127.0.0.1:{port}");

        Arc::new(Self {
            origin: origin.clone(),
            handler: handler.clone(),
            _serve: tokio::spawn(async move {
                let app = axum::Router::new().fallback(axum::routing::any(
                    move |method: axum::http::Method,
                          OriginalUri(uri): OriginalUri,
                          headers: HeaderMap,
                          body: axum::body::Bytes| {
                        let handler = handler.clone();
                        async move {
                            let request = RecordedRequest {
                                method: method.to_string(),
                                path: uri.path().to_string(),
                                query: uri.query().map(str::to_string),
                                headers,
                                message: serde_json::from_slice::<Value>(&body).ok(),
                                body: String::from_utf8_lossy(&body).into_owned(),
                            };
                            let response = handler(request).await;
                            build_response(response)
                        }
                    },
                ));
                let _ = axum::serve(listener, app).await;
            }),
        })
    }

    /// The server's origin with a path appended, the way the suites use it.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

/// `Date.now()`, in milliseconds, for the tests that check recorded expiries.
pub fn now_millis_test() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as f64)
        .unwrap_or_default()
}

/// Merges two JSON objects into a fresh one, like a JavaScript spread.
pub fn merge_into(base: Value, extra: Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    merge(&mut out, extra);
    Value::Object(out)
}

/// Merges one JSON object's entries into another, like a JavaScript spread.
pub fn merge(target: &mut serde_json::Map<String, Value>, from: Value) {
    if let Some(entries) = from.as_object() {
        for (key, value) in entries {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// Builds the axum response: headers first, then the body, streamed or held.
fn build_response(response: TestResponse) -> axum::response::Response {
    let mut builder = axum::response::Response::builder()
        .status(StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK));
    for (name, value) in &response.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = match response.body {
        TestBody::Empty => Body::empty(),
        TestBody::Full(text) => Body::from(text),
        TestBody::Chunks(receiver) => Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(receiver).map(Ok::<_, std::io::Error>),
        ),
        TestBody::Hang => Body::from_stream(futures_util::stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >()),
    };
    builder
        .body(body)
        .unwrap_or_else(|_| axum::response::Response::new(Body::empty()))
}

/// A stream that yields the given text chunks, for SSE and body tests.
pub fn stream_of_chunks(chunks: Vec<Vec<u8>>) -> ByteStream {
    Box::pin(futures_util::stream::iter(
        chunks.into_iter().map(Ok::<_, pi_rs_mcp::McpFetchError>),
    ))
}

/// One text chunk as bytes.
pub fn chunk(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// A stream whose producer stays alive until it is dropped: a cancel flag observes that.
pub struct CancellingStream {
    pub cancelled: Arc<AtomicBool>,
    sender: Option<mpsc::Sender<Vec<u8>>>,
}

impl CancellingStream {
    /// Creates the stream and a producer that keeps sending `data` until cancelled.
    pub fn endless(data: &'static str) -> (ByteStream, Arc<Self>) {
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(Self {
            cancelled: cancelled.clone(),
            sender: Some(sender),
        });
        let stream: ByteStream = Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiver)
                .map(Ok::<_, pi_rs_mcp::McpFetchError>),
        );
        let producer = handle.clone();
        tokio::spawn(async move {
            // Dropping the receiver (the consumer cancelling) fails the send.
            while let Some(sender) = producer.sender.clone() {
                if sender.send(data.as_bytes().to_vec()).await.is_err() {
                    producer.cancelled.store(true, Ordering::SeqCst);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        (stream, handle)
    }
}

/// A fake `fetch` answering what `build` produces for every request.
pub fn fake_fetch(build: Arc<dyn Fn() -> McpFetchResponse + Send + Sync>) -> McpFetch {
    McpFetch::from_fn(move |_request: McpFetchRequest| {
        let build = build.clone();
        Box::pin(async move { Ok(build()) })
    })
}

/// Builds a fetch response with status, headers, and a chunk-stream body.
pub fn fetch_response(status: u16, headers: &[(&str, &str)], body: ByteStream) -> McpFetchResponse {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            name.parse::<axum::http::HeaderName>(),
            value.parse::<HeaderValue>(),
        ) {
            map.insert(name, value);
        }
    }
    McpFetchResponse::new(status, map, Some(body))
}

/// A JSON-RPC request value.
pub fn request_value(id: Value, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// A JSON-RPC response value.
pub fn response_value(id: Value, result: Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An in-memory store whose refresh lock only runs the closure: there is one process.
pub fn memory_store() -> Arc<dyn McpOAuthServerStore> {
    Arc::new(MemoryServerStore {
        memory: MemoryOAuthStateStore::new(),
    })
}

struct MemoryServerStore {
    memory: Arc<MemoryOAuthStateStore>,
}

impl pi_rs_mcp::oauth::provider::McpOAuthStateStore for MemoryServerStore {
    fn load(&self) -> pi_rs_mcp::oauth::provider::LoadFuture {
        let memory = self.memory.clone();
        Box::pin(async move { memory.load().await })
    }

    fn save(&self, state: McpOAuthState) -> pi_rs_mcp::oauth::provider::SaveFuture {
        let memory = self.memory.clone();
        Box::pin(async move { memory.save(state).await })
    }
}

impl McpOAuthServerStore for MemoryServerStore {
    fn with_refresh_lock(
        &self,
        run: pi_rs_mcp::oauth::client_settings::LockFn,
    ) -> futures_util::future::BoxFuture<'static, Result<(), McpStoreError>> {
        Box::pin(async move { run().await })
    }
}

/// What each save of a recording store saw.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSave {
    pub locked: bool,
    pub access_token: Option<String>,
}

/// An in-memory store that records whether each save happened while the refresh lock was held.
/// `lock_error` fails taking the lock; `hold()` stands for another process holding the lock until
/// the returned function releases it. `lock_runs` are the runs of `with_refresh_lock`.
pub struct RecordingStore {
    memory: Arc<MemoryOAuthStateStore>,
    pub saves: Arc<Mutex<Vec<RecordedSave>>>,
    pub lock_runs: Arc<Mutex<Vec<tokio::sync::watch::Receiver<bool>>>>,
    held: Arc<tokio::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    lock_error: Option<String>,
    locked: Arc<AtomicBool>,
}

/// Creates a recording store and its handle.
pub fn recording_store(lock_error: Option<String>) -> Arc<RecordingStore> {
    Arc::new(RecordingStore {
        memory: MemoryOAuthStateStore::new(),
        saves: Arc::new(Mutex::new(Vec::new())),
        lock_runs: Arc::new(Mutex::new(Vec::new())),
        held: Arc::new(tokio::sync::Mutex::new(None)),
        lock_error,
        locked: Arc::new(AtomicBool::new(false)),
    })
}

impl RecordingStore {
    /// Stands for another process holding the refresh lock; the returned function releases it.
    pub async fn hold(self: &Arc<Self>) -> impl FnOnce() + Send + 'static {
        let held = self.held.clone();
        let (sender, receiver) = tokio::sync::watch::channel(false);
        {
            let mut current = held.lock().await;
            *current = Some(receiver);
        }
        move || {
            let _ = sender.send(true);
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    /// The store as the sign-in options take it.
    pub fn as_server_store(self: &Arc<Self>) -> Arc<dyn McpOAuthServerStore> {
        self.clone()
    }
}

impl pi_rs_mcp::oauth::provider::McpOAuthStateStore for RecordingStore {
    fn load(&self) -> pi_rs_mcp::oauth::provider::LoadFuture {
        let memory = self.memory.clone();
        Box::pin(async move { memory.load().await })
    }

    fn save(&self, state: McpOAuthState) -> pi_rs_mcp::oauth::provider::SaveFuture {
        self.saves
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(RecordedSave {
                locked: self.locked.load(Ordering::SeqCst),
                access_token: state
                    .tokens
                    .as_ref()
                    .map(|tokens| tokens.access_token.clone()),
            });
        let memory = self.memory.clone();
        Box::pin(async move { memory.save(state).await })
    }
}

impl McpOAuthServerStore for RecordingStore {
    fn with_refresh_lock(
        &self,
        run: pi_rs_mcp::oauth::client_settings::LockFn,
    ) -> futures_util::future::BoxFuture<'static, Result<(), McpStoreError>> {
        let held = self.held.clone();
        let lock_error = self.lock_error.clone();
        let locked = self.locked.clone();
        let lock_runs = self.lock_runs.clone();
        Box::pin(async move {
            // Each run leaves a signal the tests can await, like the promise upstream records.
            let (done_tx, done_rx) = tokio::sync::watch::channel(false);
            lock_runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(done_rx);
            let run = async move {
                let result = async {
                    if let Some(gate) = held.lock().await.as_ref() {
                        let mut gate = gate.clone();
                        while !*gate.borrow_and_update() {
                            if gate.changed().await.is_err() {
                                break;
                            }
                        }
                    }
                    if let Some(error) = lock_error {
                        // The upstream fixture injects a `McpRefreshLockBusyError`; any other
                        // error reaches the caller unchanged.
                        if error == "refresh lock busy" {
                            return Err(McpStoreError::RefreshLockBusy);
                        }
                        return Err(McpStoreError::Other(error));
                    }
                    locked.store(true, Ordering::SeqCst);
                    let outcome = run().await;
                    locked.store(false, Ordering::SeqCst);
                    outcome
                }
                .await;
                let _ = done_tx.send(true);
                result
            };
            run.await
        })
    }
}

/// Waits for the runs of `with_refresh_lock` the store recorded.
pub async fn wait_lock_runs(store: &RecordingStore) {
    let runs: Vec<_> = {
        let mut runs = store
            .lock_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *runs)
    };
    for mut run in runs {
        while !*run.borrow_and_update() {
            if run.changed().await.is_err() {
                break;
            }
        }
    }
}

type RecordedParams = Arc<Mutex<Vec<Vec<(String, String)>>>>;
type RefreshFailure = Arc<Mutex<Option<(String, Option<OnRefreshRequest>)>>>;

/// The authorization server of an MCP server at `<origin>/mcp` (`test/authorization-server.ts`):
/// discovery, dynamic client registration, PKCE, and rotating refresh tokens. Token responses
/// name no scope. `cimd` advertises Client ID Metadata Documents, `iss_parameter` the `iss`
/// parameter of authorization responses. Paths in `stall` never answer.
pub struct AuthorizationServer {
    pub server_url: String,
    /// Client metadata of dynamic client registrations.
    pub registrations: Arc<Mutex<Vec<Value>>>,
    /// Query parameters of authorization requests.
    pub authorizations: RecordedParams,
    /// Parameters of token requests.
    pub token_requests: RecordedParams,
    pub stall: Arc<Mutex<std::collections::HashSet<String>>>,
    refresh_tokens: Arc<Mutex<std::collections::HashSet<String>>>,
    refresh_failure: RefreshFailure,
    issued: Arc<AtomicUsize>,
    /// The challenge of each issued authorization code.
    challenges: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

/// A hook run before a failing refresh is answered.
pub type OnRefreshRequest =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

/// Starts the authorization server and binds it to `/mcp` as the MCP server URL.
pub async fn start_authorization_server(
    options: AuthorizationServerOptions,
) -> Arc<AuthorizationServer> {
    let cimd = options.cimd;
    let iss_parameter = options.iss_parameter;
    let registrations: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let authorizations: RecordedParams = Arc::new(Mutex::new(Vec::new()));
    let token_requests: RecordedParams = Arc::new(Mutex::new(Vec::new()));
    let stall: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let refresh_tokens: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let refresh_failure: RefreshFailure = Arc::new(Mutex::new(None));
    let issued: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let challenges: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let origin_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let handler_state = {
        let origin_holder = origin_holder.clone();
        let stall = stall.clone();
        let registrations = registrations.clone();
        let authorizations = authorizations.clone();
        let token_requests = token_requests.clone();
        let refresh_tokens = refresh_tokens.clone();
        let refresh_failure = refresh_failure.clone();
        let issued = issued.clone();
        let challenges = challenges.clone();
        Arc::new(move |request: RecordedRequest| {
            let origin_holder = origin_holder.clone();
            let stall = stall.clone();
            let registrations = registrations.clone();
            let authorizations = authorizations.clone();
            let token_requests = token_requests.clone();
            let refresh_tokens = refresh_tokens.clone();
            let refresh_failure = refresh_failure.clone();
            let issued = issued.clone();
            let challenges = challenges.clone();
            Box::pin(async move {
                let origin = origin_holder
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if stall
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(&request.path)
                {
                    return TestResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: TestBody::Hang,
                    };
                }
                match request.path.as_str() {
                    "/.well-known/oauth-protected-resource/mcp" => TestResponse::json(
                        200,
                        serde_json::json!({ "resource": format!("{origin}/mcp"), "authorization_servers": [origin] }),
                    ),
                    "/.well-known/oauth-authorization-server" => {
                        let mut metadata = serde_json::Map::new();
                        merge(
                            &mut metadata,
                            serde_json::json!({
                                "issuer": origin,
                                "authorization_endpoint": format!("{origin}/authorize"),
                                "token_endpoint": format!("{origin}/token"),
                                "registration_endpoint": format!("{origin}/register"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"],
                                "token_endpoint_auth_methods_supported": ["none"],
                            }),
                        );
                        if cimd {
                            merge(
                                &mut metadata,
                                serde_json::json!({ "client_id_metadata_document_supported": true }),
                            );
                        }
                        if iss_parameter {
                            merge(
                                &mut metadata,
                                serde_json::json!({ "authorization_response_iss_parameter_supported": true }),
                            );
                        }
                        TestResponse::json(200, Value::Object(metadata))
                    }
                    "/register" => {
                        let mut metadata = request
                            .message
                            .clone()
                            .and_then(|value| value.as_object().cloned())
                            .unwrap_or_default();
                        registrations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(Value::Object(metadata.clone()));
                        let count = registrations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .len();
                        merge(
                            &mut metadata,
                            serde_json::json!({ "client_id": format!("client-{count}") }),
                        );
                        TestResponse::json(201, Value::Object(metadata))
                    }
                    "/authorize" => {
                        let query = request
                            .query
                            .as_deref()
                            .map(|query| {
                                url::form_urlencoded::parse(query.as_bytes())
                                    .into_owned()
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        authorizations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(query.clone());
                        let count = authorizations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .len();
                        let code = format!("code-{count}");
                        let challenge = query
                            .iter()
                            .find(|(key, _)| key == "code_challenge")
                            .map(|(_, value)| value.clone())
                            .unwrap_or_default();
                        challenges
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(code.clone(), challenge);
                        let redirect = query
                            .iter()
                            .find(|(key, _)| key == "redirect_uri")
                            .map(|(_, value)| value.clone())
                            .unwrap_or_default();
                        let state = query
                            .iter()
                            .find(|(key, _)| key == "state")
                            .map(|(_, value)| value.clone())
                            .unwrap_or_default();
                        let mut location = match url::Url::parse(&redirect) {
                            Ok(url) => url,
                            Err(_) => {
                                return TestResponse {
                                    status: 400,
                                    headers: Vec::new(),
                                    body: TestBody::Empty,
                                };
                            }
                        };
                        {
                            let mut pairs = location.query_pairs_mut();
                            pairs.clear();
                            pairs.append_pair("code", &code);
                            pairs.append_pair("state", &state);
                            if iss_parameter {
                                pairs.append_pair("iss", &origin);
                            }
                        }
                        TestResponse {
                            status: 302,
                            headers: vec![("location".into(), location.to_string())],
                            body: TestBody::Empty,
                        }
                    }
                    "/token" => {
                        let params = form_params_of(&request.body);
                        token_requests
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(params.clone());
                        let grant = form_get(&params, "grant_type");
                        if grant == "authorization_code" {
                            let code = form_get(&params, "code");
                            let verifier = form_get(&params, "code_verifier");
                            let expected = challenges
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .get(&code)
                                .cloned()
                                .unwrap_or_default();
                            let digest = Sha256::digest(verifier.as_bytes());
                            let computed =
                                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                            if computed != expected {
                                return TestResponse::json(
                                    400,
                                    serde_json::json!({ "error": "invalid_grant" }),
                                );
                            }
                            challenges
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(&code);
                            return TestResponse::json(200, issue_tokens(&issued, &refresh_tokens));
                        }
                        let failure = refresh_failure
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        if let Some((error, on_request)) = failure {
                            if let Some(on_request) = on_request {
                                on_request().await;
                            }
                            return TestResponse::json(400, serde_json::json!({ "error": error }));
                        }
                        let refresh_token = form_get(&params, "refresh_token");
                        if !refresh_tokens
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&refresh_token)
                        {
                            return TestResponse::json(
                                400,
                                serde_json::json!({ "error": "invalid_grant" }),
                            );
                        }
                        TestResponse::json(200, issue_tokens(&issued, &refresh_tokens))
                    }
                    _ => TestResponse {
                        status: 404,
                        headers: Vec::new(),
                        body: TestBody::Empty,
                    },
                }
            }) as futures_util::future::BoxFuture<'static, TestResponse>
        }) as TestHandler
    };

    let http = TestServer::listen(handler_state).await;
    *origin_holder
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = http.origin.clone();
    let server_url = http.url("/mcp");
    let server = Arc::new(AuthorizationServer {
        server_url,
        registrations,
        authorizations,
        token_requests,
        stall,
        refresh_tokens,
        refresh_failure,
        issued,
        challenges,
    });
    SERVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(http);
    server
}

/// The test servers still alive, for the cleanup at the end of each test.
static SERVERS: Mutex<Vec<Arc<TestServer>>> = Mutex::new(Vec::new());

/// Options of [`start_authorization_server`].
#[derive(Clone, Copy, Default)]
pub struct AuthorizationServerOptions {
    pub cimd: bool,
    pub iss_parameter: bool,
}

/// Issues a rotating token pair.
fn issue_tokens(
    issued: &AtomicUsize,
    refresh_tokens: &Mutex<std::collections::HashSet<String>>,
) -> Value {
    let number = issued.fetch_add(1, Ordering::SeqCst) + 1;
    refresh_tokens
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(format!("refresh-{number}"));
    serde_json::json!({
        "access_token": format!("access-{number}"),
        "refresh_token": format!("refresh-{number}"),
        "token_type": "Bearer",
    })
}

/// Serializes the OAuth suites: upstream runs a file's tests one at a time, and the shared
/// server list makes that order matter.
static SERVER_SEQUENCE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Waits for the suites' turn; keep the guard alive for the whole test.
pub async fn suite_turn() -> tokio::sync::MutexGuard<'static, ()> {
    SERVER_SEQUENCE.lock().await
}

/// Drops the servers started by the test, like upstream's `closeServers` in `afterEach`.
pub fn close_servers() {
    let servers: Vec<_> = SERVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain(..)
        .collect();
    for server in servers {
        server._serve.abort();
    }
}

impl AuthorizationServer {
    /// Makes every refresh fail with `invalid_grant`, like a revoked grant.
    pub fn revoke_refresh_tokens(&self) {
        self.refresh_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Answers the next refresh with the OAuth `error`, after running `on_request`.
    pub fn fail_next_refresh(&self, error: &str, on_request: Option<OnRefreshRequest>) {
        *self
            .refresh_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((error.to_string(), on_request));
    }
}

/// Decodes an `application/x-www-form-urlencoded` body.
pub fn form_params_of(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// The first value of a form parameter.
pub fn form_get(params: &[(String, String)], name: &str) -> String {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// Follows the authorization URL like a browser, up to the redirect back to the application.
pub async fn authorize_in_browser(authorization_url: &str) -> String {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let response = client.get(authorization_url).send().await;
    response
        .ok()
        .and_then(|response| response.headers().get("location").cloned())
        .and_then(|location| location.to_str().ok().map(str::to_string))
        .unwrap_or_default()
}

/// Polls `condition` until it holds, up to `timeout_ms`.
pub async fn wait_for(condition: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while !condition() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// The authorization response a callback route or a pasted URL carries.
#[derive(Clone, Debug)]
pub struct OAuthCallbackResponse {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// A stand-in URL for unparsable locations; tests always pass parsable ones.
fn unreachable_url() -> url::Url {
    url::Url::parse("http://127.0.0.1/unreachable")
        .unwrap_or_else(|_| panic!("the stand-in URL parses"))
}

impl OAuthCallbackResponse {
    /// Reads the response off a redirect location's query.
    pub fn from_url(location: &str) -> Self {
        let base = url::Url::parse("http://127.0.0.1/").unwrap_or_else(|_| unreachable_url());
        let url = url::Url::options()
            .base_url(Some(&base))
            .parse(location)
            .unwrap_or_else(|_| unreachable_url());
        let value = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
        };
        OAuthCallbackResponse {
            code: value("code").unwrap_or_default(),
            state: value("state").unwrap_or_default(),
            iss: value("iss"),
        }
    }

    /// The response as the sign-in's completion takes it.
    pub fn into_callback(self) -> pi_rs_mcp::oauth::OAuthCallback {
        pi_rs_mcp::oauth::OAuthCallback {
            code: self.code,
            state: self.state,
            iss: self.iss,
        }
    }
}

/// A client error with a message, for handler stubs.
pub fn mcp_error(message: &str) -> McpError {
    McpError::new(-32603, message.to_string())
}

/// 12 characters identifying an MCP server URL, computed like the port's `callbackId`.
pub fn callback_id(server_url: &str) -> String {
    let url = match url::Url::parse(server_url) {
        Ok(mut url) => {
            url.set_fragment(None);
            url
        }
        Err(_) => return String::new(),
    };
    let href = url.to_string();
    let digest = Sha256::digest(href.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..9])
}
