//! The MCP client (`packages/mcp/src/client.ts`): a transport-neutral JSON-RPC client that owns
//! request correlation, initialization, timeouts, cancellation, server requests, and the
//! protocol-level helpers over any [`McpTransport`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicI64, Ordering as AtomicOrdering},
};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

use crate::abort::AbortSignal;
use crate::protocol::{
    CallToolResult, ClientCapabilities, Implementation, InitializeResult, JsonRpcId,
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, LATEST_PROTOCOL_VERSION, McpError,
    McpErrorEvent, McpTransportError, ProgressNotification, ReadResourceResult, Resource,
    ResourceTemplate, Root, ServerCapabilities, Tool, is_json_rpc_id, is_object,
    is_supported_protocol_version, json_rpc_error_codes, normalize_own_request_id,
};
use crate::transports::{McpTransport, McpTransportSendOptions};

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
const MAX_LIST_PAGES: u32 = 1_000;

/// The connection's lifecycle state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientState {
    #[default]
    Idle,
    Connecting,
    Connected,
    Closed,
}

impl ClientState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Closed => "closed",
        }
    }
}

/// Options of [`McpClient`].
#[derive(Clone, Default)]
pub struct McpClientOptions {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    /// Declared in `initialize`; extended with `roots` when `roots` are given.
    pub capabilities: Option<ClientCapabilities>,
    /// One of [`SUPPORTED_PROTOCOL_VERSIONS`]. Default: the latest.
    pub protocol_version: Option<String>,
    /// Per-request timeout in milliseconds. Default: 30000.
    pub request_timeout_ms: Option<u64>,
    /// The roots this client reports to `roots/list` requests: a fixed list, or a provider that
    /// may answer differently each time.
    pub roots: Option<Roots>,
}

/// A provider that may answer a `roots/list` request differently each time.
type RootsProvider = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<Root>> + Send>> + Send + Sync>;

/// The roots a client serves: fixed, or provided on demand.
#[derive(Clone)]
pub enum Roots {
    Fixed(Vec<Root>),
    Provider(RootsProvider),
}

/// Options of one request: cancellation, timeout, and progress.
#[derive(Clone, Default)]
pub struct McpRequestOptions {
    pub signal: Option<AbortSignal>,
    pub timeout_ms: Option<u64>,
    pub on_progress: Option<ProgressCallback>,
}

/// Receives `notifications/progress` updates of a request.
pub type ProgressCallback = Arc<dyn Fn(&ProgressNotification) + Send + Sync>;
/// Receives notifications by method, with the notification's `params` (`None` when absent).
pub type NotificationListener = Arc<dyn Fn(Option<&Value>) + Send + Sync>;
/// Receives client errors, like upstream's `onError` listeners.
pub type ErrorListener = Arc<dyn Fn(&McpErrorEvent) + Send + Sync>;
/// Called once when the connection closes, however it closed.
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;

/// Serves a server request: the `params`, and the signal that aborts when the server cancels.
/// Returns the response's `result`, or the error to answer with.
pub type RequestHandler = Arc<
    dyn Fn(
            Option<Value>,
            AbortSignal,
        ) -> Pin<Box<dyn Future<Output = Result<Value, RequestHandlerError>> + Send>>
        + Send
        + Sync,
>;

/// The error a request handler answers with: a JSON-RPC error, or any other failure, which the
/// client answers as `internalError` with the failure's message.
#[derive(Debug, Clone)]
pub enum RequestHandlerError {
    Mcp(McpError),
    Message(String),
}

/// The failure a request or the connection setup reports, mirroring the error classes upstream
/// rejects with.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpClientError {
    /// `McpError`: the server or the protocol rejected the request.
    #[error("{message}")]
    Mcp {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// `McpTimeoutError`.
    #[error("MCP request timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    /// `McpAbortError`.
    #[error("MCP request aborted")]
    Aborted,
    /// `McpConnectionClosedError`.
    #[error("{0}")]
    ConnectionClosed(String),
    /// A transport error: HTTP failures, authorization, session expiry, or a plain message.
    #[error("{0}")]
    Transport(#[from] McpTransportError),
    /// A plain `Error`, by its message.
    #[error("{0}")]
    Other(String),
}

impl From<McpError> for McpClientError {
    fn from(error: McpError) -> Self {
        Self::Mcp {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

/// A transport-neutral JSON-RPC MCP client (tools, resources, prompts, OAuth-agnostic).
#[derive(Clone)]
pub struct McpClient {
    shared: Arc<ClientShared>,
}

struct ClientShared {
    options: McpClientOptions,
    state: Mutex<ClientState>,
    transport: Mutex<Option<Arc<dyn McpTransport>>>,
    /// The client's own transport subscriptions, disposed on close.
    disposers: Mutex<Vec<crate::transports::Subscription>>,
    next_request_id: AtomicI64,
    inner: Mutex<ClientInner>,
    protocol_version: Mutex<Option<String>>,
    server_info: Mutex<Option<Implementation>>,
    server_capabilities: Mutex<Option<ServerCapabilities>>,
    instructions: Mutex<Option<String>>,
    /// The client's own handle, for tasks and listeners it spawns.
    weak: Mutex<Option<Weak<ClientShared>>>,
}

#[derive(Default)]
struct ClientInner {
    pending: IndexMap<JsonRpcId, PendingEntry>,
    progress_requests: IndexMap<JsonRpcId, JsonRpcId>,
    incoming: IndexMap<JsonRpcId, AbortSignal>,
    request_handlers: IndexMap<String, (u64, RequestHandler)>,
    notification_listeners: IndexMap<String, IndexMap<u64, NotificationListener>>,
    error_listeners: IndexMap<u64, ErrorListener>,
    close_listeners: IndexMap<u64, CloseListener>,
    next_listener_id: u64,
}

struct PendingEntry {
    resolve: oneshot::Sender<Result<Value, McpClientError>>,
    progress_token: Option<JsonRpcId>,
    on_progress: Option<ProgressCallback>,
    /// Fired when progress renews the timeout.
    renew: Arc<Notify>,
    /// Aborted when the request settles, so the transport does not dispatch (or retry) it
    /// afterwards.
    send_abort: AbortSignal,
}

impl McpClient {
    pub fn new(options: McpClientOptions) -> Self {
        let shared = Arc::new(ClientShared {
            options: options.clone(),
            state: Mutex::new(ClientState::Idle),
            transport: Mutex::new(None),
            disposers: Mutex::new(Vec::new()),
            next_request_id: AtomicI64::new(1),
            inner: Mutex::new(ClientInner::default()),
            protocol_version: Mutex::new(None),
            server_info: Mutex::new(None),
            server_capabilities: Mutex::new(None),
            instructions: Mutex::new(None),
            weak: Mutex::new(None),
        });
        {
            let mut inner = shared
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.request_handlers.insert(
                "ping".to_string(),
                (
                    0,
                    Arc::new(|_params, _signal| Box::pin(async { Ok(json!({})) })),
                ),
            );
            if let Some(roots) = &shared.options.roots {
                let roots = roots.clone();
                let roots: Roots = roots;
                inner.request_handlers.insert(
                    "roots/list".to_string(),
                    (
                        0,
                        Arc::new(move |_params, _signal| {
                            let roots = roots.clone();
                            Box::pin(async move {
                                let list = match &roots {
                                    Roots::Fixed(list) => list.clone(),
                                    Roots::Provider(provide) => provide().await,
                                };
                                Ok(json!({ "roots": list }))
                            })
                        }),
                    ),
                );
            }
        }
        *shared
            .weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(&shared));
        Self { shared }
    }

    fn weak(&self) -> Weak<ClientShared> {
        self.shared
            .weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// The connection's state.
    pub fn connection_state(&self) -> ClientState {
        *self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The server's `name`/`version` from `initialize`.
    pub fn server_info(&self) -> Option<Implementation> {
        self.shared
            .server_info
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The capabilities the server declared.
    pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
        self.shared
            .server_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The instructions the server sent in `initialize`.
    pub fn instructions(&self) -> Option<String> {
        self.shared
            .instructions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The protocol version the server selected.
    pub fn protocol_version(&self) -> Option<String> {
        self.shared
            .protocol_version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Initializes the connection over `transport` before exposing server information.
    pub async fn connect(
        &self,
        transport: Arc<dyn McpTransport>,
    ) -> Result<InitializeResult, McpClientError> {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *state != ClientState::Idle {
                return Err(McpClientError::Other(format!(
                    "Cannot connect MCP client in {} state",
                    state.as_str()
                )));
            }
            *state = ClientState::Connecting;
        }
        *self
            .shared
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(transport.clone());

        let weak = self.weak();
        let message_listener: crate::transports::McpTransportMessageListener =
            Arc::new(move |message| {
                if let Some(shared) = weak.upgrade() {
                    handle_message(&shared, message);
                }
            });
        let weak = self.weak();
        let error_listener: crate::transports::McpTransportErrorListener = Arc::new(move |error| {
            if let Some(shared) = weak.upgrade() {
                emit_error(&shared, error.clone());
            }
        });
        let weak = self.weak();
        let close_listener: crate::transports::McpTransportCloseListener = Arc::new(move || {
            if let Some(shared) = weak.upgrade() {
                mark_closed(&shared, "MCP connection closed");
            }
        });
        {
            let mut disposers = self
                .shared
                .disposers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Transport errors are reported only. Pending requests fail when the transport closes.
            disposers.push(transport.on_message(message_listener));
            disposers.push(transport.on_error(error_listener));
            disposers.push(transport.on_close(close_listener));
        }

        let result = async {
            transport.start().await.map_err(McpClientError::Transport)?;
            let mut capabilities = self.shared.options.capabilities.clone().unwrap_or_default();
            if self.shared.options.roots.is_some() && capabilities.roots.is_none() {
                capabilities.roots = Some(crate::protocol::RootsCapability::default());
            }
            let client_info = Implementation {
                name: self.shared.options.name.clone(),
                version: self.shared.options.version.clone(),
                title: self.shared.options.title.clone(),
                extra: IndexMap::new().into_iter().collect(),
            };
            let params = serde_json::to_value(crate::protocol::InitializeParams {
                protocol_version: self
                    .shared
                    .options
                    .protocol_version
                    .clone()
                    .unwrap_or_else(|| LATEST_PROTOCOL_VERSION.to_string()),
                capabilities,
                client_info,
            })
            .map_err(|error| McpClientError::Other(error.to_string()))?;
            let initialized = self
                .request_internal(
                    "initialize",
                    Some(params.clone()),
                    &McpRequestOptions::default(),
                    true,
                )
                .await?;
            let result = validate_initialize_result(&initialized)?;
            if !is_supported_protocol_version(&result.protocol_version) {
                return Err(McpClientError::Other(format!(
                    "MCP server selected unsupported protocol version {}",
                    result.protocol_version
                )));
            }
            *self
                .shared
                .protocol_version
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(result.protocol_version.clone());
            *self
                .shared
                .server_info
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(result.server_info.clone());
            *self
                .shared
                .server_capabilities
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(result.capabilities.clone());
            *self
                .shared
                .instructions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = result.instructions.clone();
            transport.set_protocol_version(&result.protocol_version);
            self.notify_internal("notifications/initialized", None, true)
                .await?;
            *self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = ClientState::Connected;
            Ok(result)
        }
        .await;

        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                self.close().await;
                Err(error)
            }
        }
    }

    /// Sends a request and answers with its raw `result`.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: &McpRequestOptions,
    ) -> Result<Value, McpClientError> {
        self.request_internal(method, params, options, false).await
    }

    /// Sends a notification.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpClientError> {
        self.notify_internal(method, params, false).await
    }

    /// Serves a server request `method` with `handler`. Dropping the returned subscription removes
    /// it again.
    pub fn set_request_handler(
        &self,
        method: &str,
        handler: RequestHandler,
    ) -> crate::transports::Subscription {
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = inner.next_listener_id;
        inner.next_listener_id += 1;
        inner
            .request_handlers
            .insert(method.to_string(), (id, handler));
        let method = method.to_string();
        let shared = Arc::downgrade(&self.shared);
        crate::transports::Subscription::new(move || {
            if let Some(shared) = shared.upgrade() {
                let mut inner = shared
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if inner
                    .request_handlers
                    .get(&method)
                    .is_some_and(|(current, _)| *current == id)
                {
                    inner.request_handlers.shift_remove(&method);
                }
            }
        })
    }

    /// Listens for `method` notifications. Dropping the returned subscription removes it again.
    pub fn on_notification(
        &self,
        method: &str,
        listener: NotificationListener,
    ) -> crate::transports::Subscription {
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = inner.next_listener_id;
        inner.next_listener_id += 1;
        inner
            .notification_listeners
            .entry(method.to_string())
            .or_default()
            .insert(id, listener);
        let method = method.to_string();
        let shared = Arc::downgrade(&self.shared);
        crate::transports::Subscription::new(move || {
            if let Some(shared) = shared.upgrade() {
                let mut inner = shared
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(listeners) = inner.notification_listeners.get_mut(&method) {
                    listeners.shift_remove(&id);
                    if listeners.is_empty() {
                        inner.notification_listeners.shift_remove(&method);
                    }
                }
            }
        })
    }

    /// Listens for client errors. Dropping the returned subscription removes it again.
    pub fn on_error(&self, listener: ErrorListener) -> crate::transports::Subscription {
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = inner.next_listener_id;
        inner.next_listener_id += 1;
        inner.error_listeners.insert(id, listener);
        let shared = Arc::downgrade(&self.shared);
        crate::transports::Subscription::new(move || {
            if let Some(shared) = shared.upgrade() {
                shared
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .error_listeners
                    .shift_remove(&id);
            }
        })
    }

    /// Called once when the connection closes, whether the transport dropped or `close` was
    /// called. Dropping the returned subscription removes it again.
    pub fn on_close(&self, listener: CloseListener) -> crate::transports::Subscription {
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = inner.next_listener_id;
        inner.next_listener_id += 1;
        inner.close_listeners.insert(id, listener);
        let shared = Arc::downgrade(&self.shared);
        crate::transports::Subscription::new(move || {
            if let Some(shared) = shared.upgrade() {
                shared
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .close_listeners
                    .shift_remove(&id);
            }
        })
    }

    /// Pings the server.
    pub async fn ping(&self, options: &McpRequestOptions) -> Result<(), McpClientError> {
        self.request("ping", None, options).await.map(|_| ())
    }

    /// Every tool, following `nextCursor` through all pages.
    pub async fn list_tools(
        &self,
        options: &McpRequestOptions,
    ) -> Result<Vec<Tool>, McpClientError> {
        let items = self
            .list_all("tools/list", "tools", is_tool_entry, options)
            .await?;
        Ok(items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect())
    }

    /// Every resource, following `nextCursor` through all pages.
    pub async fn list_resources(
        &self,
        options: &McpRequestOptions,
    ) -> Result<Vec<Resource>, McpClientError> {
        let items = self
            .list_all("resources/list", "resources", is_resource_entry, options)
            .await?;
        Ok(items.iter().filter_map(to_resource).collect())
    }

    /// One page of resources, starting at `cursor`.
    pub async fn list_resources_page(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<crate::protocol::ListResourcesResult, McpClientError> {
        let (items, next_cursor) = self
            .list_page(
                "resources/list",
                "resources",
                is_resource_entry,
                cursor,
                options,
            )
            .await?;
        Ok(crate::protocol::ListResourcesResult {
            resources: items.iter().filter_map(to_resource).collect(),
            next_cursor,
            meta: None,
        })
    }

    /// Every resource template, following `nextCursor` through all pages.
    pub async fn list_resource_templates(
        &self,
        options: &McpRequestOptions,
    ) -> Result<Vec<ResourceTemplate>, McpClientError> {
        let items = self
            .list_all(
                "resources/templates/list",
                "resourceTemplates",
                is_resource_template_entry,
                options,
            )
            .await?;
        Ok(items.iter().filter_map(to_resource_template).collect())
    }

    /// One page of resource templates, starting at `cursor`.
    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<crate::protocol::ListResourceTemplatesResult, McpClientError> {
        let (items, next_cursor) = self
            .list_page(
                "resources/templates/list",
                "resourceTemplates",
                is_resource_template_entry,
                cursor,
                options,
            )
            .await?;
        Ok(crate::protocol::ListResourceTemplatesResult {
            resource_templates: items.iter().filter_map(to_resource_template).collect(),
            next_cursor,
            meta: None,
        })
    }

    /// Reads one resource.
    pub async fn read_resource(
        &self,
        uri: &str,
        options: &McpRequestOptions,
    ) -> Result<ReadResourceResult, McpClientError> {
        let value = self
            .request("resources/read", Some(json!({ "uri": uri })), options)
            .await?;
        validate_read_resource_result(&value)
    }

    /// Calls a tool. MCP reports tool failures in the result instead of as a protocol error; read
    /// `isError` off the result.
    pub async fn call_tool(
        &self,
        name: &str,
        args: Option<Value>,
        options: &McpRequestOptions,
    ) -> Result<CallToolResult, McpClientError> {
        let mut params = json!({ "name": name });
        if let Some(args) = args {
            params["arguments"] = args;
        }
        let value = self.request("tools/call", Some(params), options).await?;
        validate_call_tool_result(&value)
    }

    /// Rejects in-flight requests, aborts server requests being served, closes the transport, and
    /// flips the state. Idempotent.
    pub async fn close(&self) {
        let transport = self
            .shared
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let disposers: Vec<_> = std::mem::take(
            &mut *self
                .shared
                .disposers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for disposer in disposers {
            disposer.unsubscribe();
        }
        mark_closed(&self.shared, "MCP connection closed");
        if let Some(transport) = transport {
            transport.close().await;
        }
    }

    async fn list_page(
        &self,
        method: &str,
        key: &str,
        is_item: fn(&Value) -> bool,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<(Vec<Value>, Option<String>), McpClientError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let page = self.request(method, params, options).await?;
        validate_list_page(method, key, &page, is_item)
    }

    /// Every item of a paginated list method.
    async fn list_all(
        &self,
        method: &str,
        key: &str,
        is_item: fn(&Value) -> bool,
        options: &McpRequestOptions,
    ) -> Result<Vec<Value>, McpClientError> {
        let mut items = Vec::new();
        let mut cursors = IndexMap::new();
        let mut cursor: Option<String> = None;
        for _page in 0..MAX_LIST_PAGES {
            let (page_items, next_cursor) = self
                .list_page(method, key, is_item, cursor.as_deref(), options)
                .await?;
            items.extend(page_items);
            let Some(next) = next_cursor else {
                return Ok(items);
            };
            if cursors.contains_key(&next) {
                return Err(McpClientError::Other(format!(
                    "MCP {method} returned duplicate cursor: {next}"
                )));
            }
            cursors.insert(next.clone(), ());
            cursor = Some(next);
        }
        Err(McpClientError::Other(format!(
            "MCP {method} exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    async fn notify_internal(
        &self,
        method: &str,
        params: Option<Value>,
        allow_connecting: bool,
    ) -> Result<(), McpClientError> {
        let transport = self.require_transport(allow_connecting)?;
        let notification = JsonRpcMessage::Notification(crate::protocol::JsonRpcNotification {
            jsonrpc: crate::protocol::JsonRpcVersion,
            method: method.to_string(),
            params,
            extra: IndexMap::new().into_iter().collect(),
        });
        transport
            .send(&notification, &McpTransportSendOptions::default())
            .await
            .map_err(McpClientError::Transport)
    }

    async fn request_internal(
        &self,
        method: &str,
        params: Option<Value>,
        options: &McpRequestOptions,
        allow_connecting: bool,
    ) -> Result<Value, McpClientError> {
        let transport = self.require_transport(allow_connecting)?;
        if let Some(signal) = &options.signal
            && signal.aborted()
        {
            return Err(McpClientError::Aborted);
        }
        let id = JsonRpcId::number(
            self.shared
                .next_request_id
                .fetch_add(1, AtomicOrdering::SeqCst),
        );
        let progress_token = options.on_progress.as_ref().map(|_| id.clone());
        let request_params = match &progress_token {
            Some(token) => {
                let mut base = params
                    .clone()
                    .filter(is_object)
                    .unwrap_or_else(|| json!({}));
                let mut meta = base
                    .get("_meta")
                    .cloned()
                    .filter(is_object)
                    .unwrap_or_else(|| json!({}));
                if let Value::Object(meta_object) = &mut meta {
                    meta_object.insert("progressToken".to_string(), token.to_json());
                }
                if let Value::Object(base_object) = &mut base {
                    base_object.insert("_meta".to_string(), meta);
                }
                Some(base)
            }
            None => params,
        };
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: crate::protocol::JsonRpcVersion,
            id: id.clone(),
            method: method.to_string(),
            params: request_params,
            extra: IndexMap::new().into_iter().collect(),
        });

        let timeout_ms = options
            .timeout_ms
            .or(self.shared.options.request_timeout_ms)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        let (resolve, resolve_rx) = oneshot::channel::<Result<Value, McpClientError>>();
        let renew = Arc::new(Notify::new());
        let send_abort = AbortSignal::default();
        {
            let mut inner = self
                .shared
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.pending.insert(
                id.clone(),
                PendingEntry {
                    resolve,
                    progress_token: progress_token.clone(),
                    on_progress: options.on_progress.clone(),
                    renew: renew.clone(),
                    send_abort: send_abort.clone(),
                },
            );
            if let Some(token) = &progress_token {
                inner.progress_requests.insert(token.clone(), id.clone());
            }
        }

        let send_options = McpTransportSendOptions {
            signal: Some(send_abort),
        };
        let mut send = transport.send(&message, &send_options);
        let mut send_done = false;
        let mut resolve_rx = resolve_rx;
        let timeout = Duration::from_millis(timeout_ms);
        let mut sleep = Box::pin(tokio::time::sleep(timeout));
        let has_signal = options.signal.is_some();
        let mut signal_wait: Pin<Box<dyn Future<Output = String> + Send>> =
            match options.signal.clone() {
                Some(signal) => Box::pin(signal.wait_aborted()),
                None => Box::pin(std::future::pending::<String>()),
            };
        loop {
            tokio::select! {
                biased;
                resolved = &mut resolve_rx => {
                    return match resolved {
                        Ok(result) => result,
                        // The entry was removed without resolving: the connection closed.
                        Err(_) => Err(McpClientError::ConnectionClosed(
                            "MCP connection closed".to_string(),
                        )),
                    };
                }
                _ = &mut sleep => {
                    let error = McpClientError::Timeout { timeout_ms };
                    self.cancel_pending(&id, error.clone(), method != "initialize", Some("Request timed out"));
                    return Err(error);
                }
                _ = renew.notified() => {
                    sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                }
                reason = &mut signal_wait, if has_signal => {
                    let error = McpClientError::Aborted;
                    self.cancel_pending(&id, error.clone(), method != "initialize", Some(&reason));
                    return Err(error);
                }
                send_result = &mut send, if !send_done => {
                    send_done = true;
                    if let Err(error) = send_result {
                        let error = McpClientError::Transport(error);
                        self.cancel_pending(&id, error.clone(), false, None);
                        return Err(error);
                    }
                }
            }
        }
    }

    fn require_transport(
        &self,
        allow_connecting: bool,
    ) -> Result<Arc<dyn McpTransport>, McpClientError> {
        let state = *self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transport = self
            .shared
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match transport {
            Some(transport)
                if state == ClientState::Connected
                    || (allow_connecting && state == ClientState::Connecting) =>
            {
                Ok(transport)
            }
            _ => Err(McpClientError::ConnectionClosed(format!(
                "MCP client is {}",
                state.as_str()
            ))),
        }
    }

    fn cancel_pending(
        &self,
        id: &JsonRpcId,
        error: McpClientError,
        notify_server: bool,
        reason: Option<&str>,
    ) {
        let (entry, transport) = {
            let mut inner = self
                .shared
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = inner.pending.shift_remove(id);
            if let Some(entry) = &entry
                && let Some(token) = &entry.progress_token
            {
                inner.progress_requests.shift_remove(token);
            }
            let transport = self
                .shared
                .transport
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            (entry, transport)
        };
        let Some(entry) = entry else { return };
        entry.send_abort.abort_with(None);
        let _ = entry.resolve.send(Err(error));
        if notify_server && let Some(transport) = transport {
            let mut params = json!({ "requestId": id.to_json() });
            if let Some(reason) = reason {
                params["reason"] = json!(reason);
            }
            let notification = JsonRpcMessage::Notification(crate::protocol::JsonRpcNotification {
                jsonrpc: crate::protocol::JsonRpcVersion,
                method: "notifications/cancelled".to_string(),
                params: Some(params),
                extra: IndexMap::new().into_iter().collect(),
            });
            let weak = self.weak();
            tokio::spawn(async move {
                if let Err(error) = transport
                    .send(&notification, &McpTransportSendOptions::default())
                    .await
                    && let Some(shared) = weak.upgrade()
                {
                    emit_error(&shared, McpErrorEvent::from_transport(&error));
                }
            });
        }
    }
}

/// The client's own handle over its shared state, for the message pump's spawned tasks.
fn handle_message(shared: &Arc<ClientShared>, message: &JsonRpcMessage) {
    if let JsonRpcMessage::Response(response) = message {
        handle_response(shared, response);
        return;
    }
    if let JsonRpcMessage::Request(request) = message {
        let shared = shared.clone();
        tokio::spawn(handle_request(shared, request.clone()));
        return;
    }
    if let JsonRpcMessage::Notification(notification) = message {
        handle_notification(shared, &notification.method, notification.params.as_ref());
        return;
    }
    emit_error(
        shared,
        McpErrorEvent {
            message: "Received invalid JSON-RPC message".to_string(),
            code: Some(json_rpc_error_codes::INVALID_REQUEST),
            data: None,
        },
    );
}

fn handle_response(shared: &Arc<ClientShared>, response: &JsonRpcResponse) {
    let raw_id = response.id();
    let id = normalize_own_request_id(raw_id);
    let entry = {
        let mut inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.pending.shift_remove(&id)
    };
    let Some(entry) = entry else {
        emit_error(
            shared,
            McpErrorEvent::message(format!(
                "Received response for unknown MCP request {}",
                json_rpc_id_string(raw_id)
            )),
        );
        return;
    };
    // Removed before resolving, so the transport stops dispatching (or retrying) it.
    entry.send_abort.abort_with(None);
    if let Some(token) = &entry.progress_token {
        shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .progress_requests
            .shift_remove(token);
    }
    match response {
        JsonRpcResponse::Success(success) => {
            let _ = entry.resolve.send(Ok(success.result.clone()));
        }
        JsonRpcResponse::Error(error) => {
            let _ = entry.resolve.send(Err(McpClientError::Mcp {
                code: error.error.code,
                message: error.error.message.clone(),
                data: error.error.data.clone(),
            }));
        }
    }
}

async fn handle_request(shared: Arc<ClientShared>, request: crate::protocol::JsonRpcRequest) {
    let transport = shared
        .transport
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if transport.is_none() {
        return;
    }
    let handler = shared
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .request_handlers
        .get(&request.method)
        .cloned();
    let Some((_handler_id, handler)) = handler else {
        let weak = Arc::downgrade(&shared);
        let method = request.method.clone();
        let id = request.id.clone();
        tokio::spawn(async move {
            let error = crate::protocol::JsonRpcErrorObject {
                code: json_rpc_error_codes::METHOD_NOT_FOUND,
                message: format!("Method not found: {method}"),
                data: None,
            };
            if let Some((transport, shared)) = upgrade_pair(&weak).await {
                let _ = send_error_response(&transport, &shared, id, error).await;
            }
        });
        return;
    };
    let signal = AbortSignal::default();
    shared
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .incoming
        .insert(request.id.clone(), signal.clone());
    let result = handler(request.params.clone(), signal.clone()).await;
    let response = match result {
        Ok(value) => {
            let result = if value.is_null() { json!({}) } else { value };
            Ok(result)
        }
        Err(RequestHandlerError::Mcp(error)) => Err(crate::protocol::JsonRpcErrorObject {
            code: error.code,
            message: error.message,
            data: error.data,
        }),
        Err(RequestHandlerError::Message(message)) => Err(crate::protocol::JsonRpcErrorObject {
            code: json_rpc_error_codes::INTERNAL_ERROR,
            message,
            data: None,
        }),
    };
    shared
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .incoming
        .shift_remove(&request.id);
    let weak = Arc::downgrade(&shared);
    let id = request.id.clone();
    tokio::spawn(async move {
        let Some((transport, shared)) = upgrade_pair(&weak).await else {
            return;
        };
        let sent = match response {
            Ok(result) => send_result_response(&transport, &shared, id, result).await,
            Err(error) => send_error_response(&transport, &shared, id, error).await,
        };
        if let Err(error) = sent {
            emit_error(&shared, McpErrorEvent::from_transport(&error));
        }
    });
}

/// Upgrades the client handle and reads the transport for a spawned response send.
async fn upgrade_pair(
    weak: &Weak<ClientShared>,
) -> Option<(Arc<dyn McpTransport>, Arc<ClientShared>)> {
    let shared = weak.upgrade()?;
    let transport = shared
        .transport
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()?;
    Some((transport, shared))
}

async fn send_result_response(
    transport: &Arc<dyn McpTransport>,
    _shared: &Arc<ClientShared>,
    id: JsonRpcId,
    result: Value,
) -> Result<(), McpTransportError> {
    let message = JsonRpcMessage::Response(JsonRpcResponse::Success(
        crate::protocol::JsonRpcSuccessResponse {
            jsonrpc: crate::protocol::JsonRpcVersion,
            id,
            result,
            extra: IndexMap::new().into_iter().collect(),
        },
    ));
    transport
        .send(&message, &McpTransportSendOptions::default())
        .await
}

async fn send_error_response(
    transport: &Arc<dyn McpTransport>,
    _shared: &Arc<ClientShared>,
    id: JsonRpcId,
    error: crate::protocol::JsonRpcErrorObject,
) -> Result<(), McpTransportError> {
    let message = JsonRpcMessage::Response(JsonRpcResponse::Error(
        crate::protocol::JsonRpcErrorResponse {
            jsonrpc: crate::protocol::JsonRpcVersion,
            id,
            error,
            extra: IndexMap::new().into_iter().collect(),
        },
    ));
    transport
        .send(&message, &McpTransportSendOptions::default())
        .await
}

fn handle_notification(shared: &Arc<ClientShared>, method: &str, params: Option<&Value>) {
    if method == "notifications/progress" {
        handle_progress(shared, params.unwrap_or(&Value::Null));
    } else if method == "notifications/cancelled" {
        handle_cancelled(shared, params.unwrap_or(&Value::Null));
    }
    let listeners = {
        let inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .notification_listeners
            .get(method)
            .map(|listeners| listeners.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    for listener in listeners {
        listener(params);
    }
}

fn handle_progress(shared: &Arc<ClientShared>, params: &Value) {
    let token = params.get("progressToken");
    let progress = params.get("progress").and_then(Value::as_f64);
    if !is_object(params) || !token.is_some_and(is_json_rpc_id) {
        return;
    }
    let Some(progress) = progress else { return };
    let Some(token) = token else { return };
    let token_id = json_rpc_id(token);
    let normalized_token = normalize_own_request_id(&token_id);
    let entry = {
        let inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        inner
            .progress_requests
            .get(&normalized_token)
            .and_then(|request_id| inner.pending.get(request_id))
            .map(|entry| (entry.renew.clone(), entry.on_progress.clone()))
    };
    let Some((renew, on_progress)) = entry else {
        return;
    };
    renew.notify_one();
    if let Some(on_progress) = on_progress {
        let notification = crate::protocol::ProgressNotification {
            progress_token: token_id,
            progress,
            total: params.get("total").and_then(Value::as_f64),
            message: params
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            extra: extra_fields(params),
        };
        on_progress(&notification);
    }
}

fn handle_cancelled(shared: &Arc<ClientShared>, params: &Value) {
    let request_id = params.get("requestId");
    let reason = params.get("reason").and_then(Value::as_str);
    if !is_object(params) || !request_id.is_some_and(is_json_rpc_id) {
        return;
    }
    let Some(request_id) = request_id else { return };
    let signal = {
        let inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .incoming
            .get(&json_rpc_id(request_id))
            .map(|signal| (signal.clone(), signal.reason()))
    };
    if let Some((signal, _)) = signal {
        signal.abort_with(reason);
    }
}

fn mark_closed(shared: &Arc<ClientShared>, message: &str) {
    let was_closed = *shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        == ClientState::Closed;
    *shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = ClientState::Closed;
    let error = McpClientError::ConnectionClosed(message.to_string());
    let pending: Vec<PendingEntry> = {
        let mut inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.progress_requests.clear();
        inner.pending.drain(..).map(|(_, entry)| entry).collect()
    };
    for entry in pending {
        entry.send_abort.abort_with(None);
        let _ = entry.resolve.send(Err(error.clone()));
    }
    let incoming: Vec<AbortSignal> = {
        let mut inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.incoming.drain(..).map(|(_, signal)| signal).collect()
    };
    for signal in incoming {
        signal.abort_with(Some(message));
    }
    if was_closed {
        return;
    }
    let listeners = {
        let inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.close_listeners.values().cloned().collect::<Vec<_>>()
    };
    for listener in listeners {
        listener();
    }
}

fn emit_error(shared: &Arc<ClientShared>, error: McpErrorEvent) {
    let listeners = {
        let inner = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.error_listeners.values().cloned().collect::<Vec<_>>()
    };
    for listener in listeners {
        listener(&error);
    }
}

/// The fields of a JSON object its consumers do not name.
fn extra_fields(value: &Value) -> serde_json::Map<String, Value> {
    let mut extra = value.as_object().cloned().unwrap_or_default();
    extra.shift_remove("progressToken");
    extra.shift_remove("progress");
    extra.shift_remove("total");
    extra.shift_remove("message");
    extra
}

/// One id as JavaScript's `String(id)` prints it.
fn json_rpc_id_string(id: &JsonRpcId) -> String {
    match id {
        JsonRpcId::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => integer.to_string(),
            (None, Some(value)) if value.fract() == 0.0 && value.abs() < 1e15 => {
                format!("{}", value as i64)
            }
            (None, Some(value)) => format!("{value}"),
            (None, None) => String::new(),
        },
        JsonRpcId::String(text) => text.clone(),
    }
}

fn json_rpc_id(value: &Value) -> JsonRpcId {
    match value {
        Value::Number(number) => JsonRpcId::Number(number.clone()),
        Value::String(text) => JsonRpcId::String(text.clone()),
        _ => JsonRpcId::String(String::new()),
    }
}

/// Whether an item is a tool entry: `name` plus an object `inputSchema`.
fn is_tool_entry(item: &Value) -> bool {
    item.get("name").is_some_and(Value::is_string) && item.get("inputSchema").is_some_and(is_object)
}

/// `name` is required by the spec, but some servers omit it; the URI stands in.
fn is_resource_entry(item: &Value) -> bool {
    item.get("uri").is_some_and(Value::is_string)
        && item.get("name").is_none_or(|name| name.is_string())
}

fn is_resource_template_entry(item: &Value) -> bool {
    item.get("uriTemplate").is_some_and(Value::is_string)
        && item.get("name").is_none_or(|name| name.is_string())
}

/// A resource entry with `name` defaulted to its URI, as the list helpers return it.
fn to_resource(item: &Value) -> Option<Resource> {
    let mut object = item.as_object()?.clone();
    match object.get("name") {
        None | Some(Value::Null) => {
            object.insert(
                "name".to_string(),
                Value::String(
                    item.get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
            );
        }
        _ => {}
    }
    serde_json::from_value(Value::Object(object)).ok()
}

/// A resource template entry with `name` defaulted to its URI template.
fn to_resource_template(item: &Value) -> Option<ResourceTemplate> {
    let mut object = item.as_object()?.clone();
    match object.get("name") {
        None | Some(Value::Null) => {
            object.insert(
                "name".to_string(),
                Value::String(
                    item.get("uriTemplate")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
            );
        }
        _ => {}
    }
    serde_json::from_value(Value::Object(object)).ok()
}

fn validate_initialize_result(value: &Value) -> Result<InitializeResult, McpClientError> {
    let valid = is_object(value)
        && value.get("protocolVersion").is_some_and(Value::is_string)
        && value.get("capabilities").is_some_and(is_object)
        && value.get("serverInfo").is_some_and(is_object)
        && value
            .get("serverInfo")
            .and_then(|server| server.get("name"))
            .is_some_and(Value::is_string)
        && value
            .get("serverInfo")
            .and_then(|server| server.get("version"))
            .is_some_and(Value::is_string)
        && value
            .get("instructions")
            .is_none_or(|instructions| instructions.is_string());
    if !valid {
        return Err(McpError::new(
            json_rpc_error_codes::INVALID_REQUEST,
            "Invalid MCP initialize result",
        )
        .into());
    }
    serde_json::from_value(value.clone()).map_err(|_| {
        McpError::new(
            json_rpc_error_codes::INVALID_REQUEST,
            "Invalid MCP initialize result",
        )
        .into()
    })
}

/// One page of a paginated list: the items under `key`, each checked by `is_item`.
fn validate_list_page(
    method: &str,
    key: &str,
    value: &Value,
    is_item: fn(&Value) -> bool,
) -> Result<(Vec<Value>, Option<String>), McpClientError> {
    let invalid = |message: &str| {
        McpError::new(json_rpc_error_codes::INVALID_REQUEST, message.to_string()).into()
    };
    let Some(items) = value.get(key).and_then(Value::as_array) else {
        return Err(invalid(&format!("Invalid MCP {method} result")));
    };
    for item in items {
        if !is_object(item) || !is_item(item) {
            return Err(invalid(&format!("Invalid entry in MCP {method} result")));
        }
    }
    // Some servers end pagination with `null` or `""` instead of omitting the cursor.
    let next_cursor = match value.get("nextCursor") {
        Some(Value::Null) | None => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid(&format!("Invalid MCP {method} cursor"))),
    };
    Ok((items.clone(), next_cursor))
}

fn validate_read_resource_result(value: &Value) -> Result<ReadResourceResult, McpClientError> {
    let invalid = |message: &str| {
        McpError::new(json_rpc_error_codes::INVALID_REQUEST, message.to_string()).into()
    };
    let Some(contents) = value.get("contents").and_then(Value::as_array) else {
        return Err(invalid("Invalid MCP resources/read result"));
    };
    for contents in contents {
        let has_text = contents.get("text").is_some_and(Value::is_string);
        let has_blob = contents.get("blob").is_some_and(Value::is_string);
        let valid = is_object(contents)
            && contents.get("uri").is_some_and(Value::is_string)
            && (has_text || has_blob);
        if !valid {
            return Err(invalid("Invalid contents in MCP resources/read result"));
        }
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid("Invalid MCP resources/read result"))
}

/// `content` is required by the spec, but servers that only return `structuredContent` omit it
/// (the SDK defaults it too). `structuredContent` may be any JSON value.
fn validate_call_tool_result(value: &Value) -> Result<CallToolResult, McpClientError> {
    let valid = is_object(value)
        && value
            .get("content")
            .is_none_or(|content| content.is_array());
    if !valid {
        return Err(McpError::new(
            json_rpc_error_codes::INVALID_REQUEST,
            "Invalid MCP tools/call result",
        )
        .into());
    }
    serde_json::from_value(value.clone()).map_err(|_| {
        McpError::new(
            json_rpc_error_codes::INVALID_REQUEST,
            "Invalid MCP tools/call result",
        )
        .into()
    })
}
