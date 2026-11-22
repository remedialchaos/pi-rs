//! The MCP protocol surface: JSON-RPC framing, protocol types, and content blocks.

pub mod content;
pub mod jsonrpc;
pub mod types;

pub use content::{CallToolResult, LlmContent, to_llm_content};
pub use jsonrpc::{
    JsonRpcErrorObject, JsonRpcErrorResponse, JsonRpcId, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, JsonRpcSuccessResponse, JsonRpcVersion, McpError,
    McpErrorEvent, McpTransportError, is_json_rpc_id, is_json_rpc_notification,
    is_json_rpc_request, is_json_rpc_response, is_object, json_rpc_error_codes, parse_json,
    parse_json_rpc_message,
};
pub use types::{
    CancelledNotification, ClientCapabilities, Implementation, InitializeParams, InitializeResult,
    LATEST_PROTOCOL_VERSION, ListChangedCapability, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, ProgressNotification, ReadResourceResult, Resource,
    ResourceTemplate, ResourcesCapability, Root, RootsCapability, SUPPORTED_PROTOCOL_VERSIONS,
    ServerCapabilities, Tool, is_supported_protocol_version, resource_contents_text,
};
