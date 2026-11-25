//! A small, standalone Model Context Protocol client for pi and other applications: the Rust
//! port of `packages/mcp` (`@earendil-works/pi-mcp`). It does not depend on the official MCP SDK
//! or other pi crates.
//!
//! The package provides a transport-neutral client core, stdio and Streamable HTTP transports, a
//! stdio transport over a pipe to a program started elsewhere, and an in-memory testing
//! transport. A transport owns framing and I/O; it delivers individual JSON-RPC messages to
//! [`McpClient`], which owns request correlation, initialization, timeouts, cancellation, server
//! requests, and protocol-level helpers.
//!
//! ```no_run
//! # async fn example() -> Result<(), pi_rs_mcp::McpClientError> {
//! use pi_rs_mcp::{McpClient, McpClientOptions, StdioTransport, StdioTransportOptions};
//!
//! let transport = StdioTransport::new(StdioTransportOptions {
//!     command: "npx".to_string(),
//!     args: vec![
//!         "-y".to_string(),
//!         "@modelcontextprotocol/server-filesystem".to_string(),
//!         "/workspace".to_string(),
//!     ],
//!     ..StdioTransportOptions::default()
//! });
//! let client = McpClient::new(McpClientOptions {
//!     name: "my-client".to_string(),
//!     version: "1.0.0".to_string(),
//!     ..McpClientOptions::default()
//! });
//! client.connect(transport).await?;
//! let tools = client.list_tools(&Default::default()).await?;
//! let result = client
//!     .call_tool("search", Some(serde_json::json!({ "query": "MCP" })), &Default::default())
//!     .await?;
//! client.close().await;
//! # let _ = (tools, result);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod abort;
mod auth_provider;
mod client;
mod config;
mod fetch;
mod input_schema;
mod model_content;
mod naming;
pub mod oauth;
mod output_limit;
mod protocol;
mod result_schema;
mod testing;
mod transports;

pub use abort::{AbortController, AbortSignal, abort_any, abort_timeout};
pub use auth_provider::{
    AuthError, AuthProvider, FetchInit, McpFetch, StaticToken, UnauthorizedContext,
};
pub use client::{
    ClientState, CloseListener, ErrorListener, McpClient, McpClientError, McpClientOptions,
    McpRequestOptions, NotificationListener, ProgressCallback, RequestHandler, RequestHandlerError,
    Roots,
};
pub use config::{
    McpServerConfigOptions, McpServerConfigResult, is_mcp_server_override,
    validate_mcp_server_config,
};
pub use fetch::{
    ByteStream, McpFetchError, McpFetchRequest, McpFetchResponse, StreamingDecoder, header_map,
    read_body_text,
};
pub use input_schema::{McpInputSchemaError, normalize_mcp_input_schema};
pub use model_content::{McpModelContentOptions, to_mcp_model_content};
pub use naming::{create_mcp_tool_name, mcp_namespace, mcp_server_summary};
pub use output_limit::{
    LimitedMcpContent, MCP_OUTPUT_MAX_BYTES, McpOutputSaver, SaveOutput, limit_mcp_content,
};
pub use protocol::{
    CallToolResult, CancelledNotification, ClientCapabilities, Implementation, InitializeParams,
    InitializeResult, JsonRpcErrorObject, JsonRpcErrorResponse, JsonRpcId, JsonRpcMessage,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcSuccessResponse, JsonRpcVersion,
    LATEST_PROTOCOL_VERSION, ListChangedCapability, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, LlmContent, McpError, McpErrorEvent, McpTransportError,
    ProgressNotification, ReadResourceResult, Resource, ResourceTemplate, ResourcesCapability,
    Root, RootsCapability, SUPPORTED_PROTOCOL_VERSIONS, ServerCapabilities, Tool, is_json_rpc_id,
    is_json_rpc_notification, is_json_rpc_request, is_json_rpc_response, is_object,
    is_supported_protocol_version, json_rpc_error_codes, parse_json, parse_json_rpc_message,
    resource_contents_text, to_llm_content,
};
pub use result_schema::create_mcp_result_schema;
pub use testing::{InMemoryTransportPair, create_in_memory_transport_pair};
pub use transports::{
    ConsumeSseOptions, DEFAULT_MAX_MESSAGE_BYTES, InMemoryTransport, JsonRpcLineReader,
    JsonRpcLineReaderOptions, McpPipe, McpPipeHandlers, McpPipeOpener, McpTransport,
    McpTransportCloseListener, McpTransportErrorListener, McpTransportMessageListener,
    McpTransportSendOptions, OnError, OnMessage, OnStderr, PipeData, PipeExited, PipeTransport,
    PipeTransportOptions, SseEvent, StdioStderr, StdioTransport, StdioTransportOptions,
    StreamableHttpReconnectOptions, StreamableHttpTransport, StreamableHttpTransportOptions,
    Subscription, TransportEvents, consume_sse_stream,
};
