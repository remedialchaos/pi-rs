//! JSON-RPC 2.0 messages and the protocol errors `packages/mcp/src/protocol/jsonrpc.ts` defines.
//!
//! Messages are typed but open: `params` and `result` are raw JSON, validated structurally where
//! upstream validates structurally, so server-shaped objects keep every field they carry.

use serde_json::Value;

/// The JSON-RPC version marker, serialized as `"2.0"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl serde::Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> serde::Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        if value == "2.0" {
            Ok(Self)
        } else {
            Err(<D::Error as serde::de::Error>::custom("expected \"2.0\""))
        }
    }
}

/// A JSON-RPC request or response id: a string or a finite number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(serde_json::Number),
    String(String),
}

impl JsonRpcId {
    pub fn number(id: i64) -> Self {
        Self::Number(serde_json::Number::from(id))
    }

    /// The id as a JSON value, like `String(id)` upstream for error messages.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Number(number) => Value::Number(number.clone()),
            Self::String(text) => Value::String(text.clone()),
        }
    }
}

/// The client's request IDs and progress tokens are integers. Some servers echo them back as
/// strings ("1" for 1); map canonical non-negative decimal strings to their integer so they
/// match, like other MCP clients do.
pub(crate) fn normalize_own_request_id(id: &JsonRpcId) -> JsonRpcId {
    if let JsonRpcId::String(text) = id {
        let bytes = text.as_bytes();
        let canonical = bytes == b"0"
            || (!bytes.is_empty() && bytes[0] != b'0' && bytes.iter().all(u8::is_ascii_digit));
        if canonical && let Ok(number) = text.parse::<i64>() {
            return JsonRpcId::number(number);
        }
    }
    id.clone()
}

/// A request: has an id and a method.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Envelope fields the struct does not name, kept for pass-through.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// A notification: a method without an id.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Envelope fields the struct does not name, kept for pass-through.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// The `error` member of a failed response.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A successful response.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcSuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
    /// Envelope fields the struct does not name, kept for pass-through.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// A failed response.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub error: JsonRpcErrorObject,
    /// Envelope fields the struct does not name, kept for pass-through.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// Any response: success or failure.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Success(JsonRpcSuccessResponse),
    Error(JsonRpcErrorResponse),
}

impl JsonRpcResponse {
    pub fn id(&self) -> &JsonRpcId {
        match self {
            Self::Success(response) => &response.id,
            Self::Error(response) => &response.id,
        }
    }
}

/// Any JSON-RPC message a transport delivers.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

impl JsonRpcMessage {
    /// The message as a JSON value, for comparisons and logging.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The JSON-RPC error codes the client and transports use.
pub mod json_rpc_error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// An error carrying a JSON-RPC error code, like upstream's `McpError` class.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct McpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl McpError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    /// An `invalidRequest` error, the code the protocol validations throw with.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(json_rpc_error_codes::INVALID_REQUEST, message)
    }
}

/// The errors the transports report, as upstream's thrown error classes.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum McpTransportError {
    /// `McpConnectionClosedError`.
    #[error("{0}")]
    ConnectionClosed(String),
    /// `McpAbortError`.
    #[error("MCP request aborted")]
    Aborted,
    /// `McpAuthRequiredError`: the server answered 401.
    #[error("MCP server requires authentication")]
    AuthRequired {
        www_authenticate: Option<String>,
        body: String,
    },
    /// `McpSessionExpiredError`: an established session answered 404.
    #[error("MCP session expired")]
    SessionExpired { body: String },
    /// `McpHttpError`: any other HTTP failure.
    #[error("{message}")]
    Http {
        status: u16,
        message: String,
        body: String,
    },
    /// A network failure, the retryable kind a Web `fetch` reports as `TypeError`.
    #[error("{0}")]
    Network(String),
    /// A plain `Error`, by its message.
    #[error("{0}")]
    Message(String),
}

impl McpTransportError {
    /// The default `McpConnectionClosedError`.
    pub fn connection_closed() -> Self {
        Self::ConnectionClosed("MCP connection closed".to_string())
    }

    /// The status an HTTP-class error carries, for the transient-status checks.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::AuthRequired { .. } => Some(401),
            Self::SessionExpired { .. } => Some(404),
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// An error as it reaches `on_error` listeners: upstream passes the `Error` itself, of which the
/// message is the part applications read.
#[derive(Clone, Debug, PartialEq)]
pub struct McpErrorEvent {
    pub message: String,
    /// The JSON-RPC error code, when the error is one.
    pub code: Option<i64>,
    pub data: Option<Value>,
}

impl McpErrorEvent {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            data: None,
        }
    }

    pub fn from_mcp(error: &McpError) -> Self {
        Self {
            message: error.message.clone(),
            code: Some(error.code),
            data: error.data.clone(),
        }
    }

    pub fn from_transport(error: &McpTransportError) -> Self {
        Self {
            message: error.to_string(),
            code: None,
            data: None,
        }
    }
}

/// Whether a value is a plain JSON object, like upstream's `isObject`.
pub fn is_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// Whether a value is a JSON-RPC id: a string or a finite number.
pub fn is_json_rpc_id(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Number(number) => number.as_f64().is_some_and(|number| number.is_finite()),
        _ => false,
    }
}

/// Whether a message is a request: `"2.0"`, a valid id and a string method.
pub fn is_json_rpc_request(message: &Value) -> bool {
    is_object(message)
        && message.get("jsonrpc") == Some(&Value::String("2.0".to_string()))
        && message.get("id").is_some_and(is_json_rpc_id)
        && message
            .get("method")
            .is_some_and(|method| method.as_str().is_some())
}

/// Whether a message is a notification: `"2.0"` and a method, without an id member at all.
pub fn is_json_rpc_notification(message: &Value) -> bool {
    is_object(message)
        && message.get("jsonrpc") == Some(&Value::String("2.0".to_string()))
        && !message
            .as_object()
            .is_some_and(|object| object.contains_key("id"))
        && message
            .get("method")
            .is_some_and(|method| method.as_str().is_some())
}

/// Whether a message is a response: `"2.0"`, a valid id and either a result or a proper error.
pub fn is_json_rpc_response(message: &Value) -> bool {
    if !is_object(message)
        || message.get("jsonrpc") != Some(&Value::String("2.0".to_string()))
        || !message.get("id").is_some_and(is_json_rpc_id)
    {
        return false;
    }
    let object = match message.as_object() {
        Some(object) => object,
        None => return false,
    };
    if object.contains_key("result") {
        return !object.contains_key("error");
    }
    match object.get("error") {
        Some(Value::Object(error)) => {
            error
                .get("code")
                .is_some_and(|code| code.as_i64().is_some())
                && error
                    .get("message")
                    .is_some_and(|message| message.as_str().is_some())
        }
        _ => false,
    }
}

/// Parses an untyped JSON value into a message, or fails with the `McpError` upstream throws for
/// invalid input. Fields the message types do not name stay in each variant's `extra` map; the
/// validation order matches upstream: request, then notification, then response.
pub fn parse_json_rpc_message(value: &Value) -> Result<JsonRpcMessage, McpError> {
    if is_json_rpc_request(value) {
        let request = serde_json::from_value(value.clone())
            .map_err(|_| McpError::invalid("Invalid JSON-RPC message"))?;
        return Ok(JsonRpcMessage::Request(request));
    }
    if is_json_rpc_notification(value) {
        let notification = serde_json::from_value(value.clone())
            .map_err(|_| McpError::invalid("Invalid JSON-RPC message"))?;
        return Ok(JsonRpcMessage::Notification(notification));
    }
    if is_json_rpc_response(value) {
        let has_result = value
            .as_object()
            .is_some_and(|object| object.contains_key("result"));
        if has_result {
            let response = serde_json::from_value(value.clone())
                .map_err(|_| McpError::invalid("Invalid JSON-RPC message"))?;
            return Ok(JsonRpcMessage::Response(JsonRpcResponse::Success(response)));
        }
        let response = serde_json::from_value(value.clone())
            .map_err(|_| McpError::invalid("Invalid JSON-RPC message"))?;
        return Ok(JsonRpcMessage::Response(JsonRpcResponse::Error(response)));
    }
    Err(McpError::invalid("Invalid JSON-RPC message"))
}

/// Parses text as JSON the way V8's `JSON.parse` does: one leading U+FEFF is tolerated, anything
/// else must be valid JSON. The error is the parse failure's message.
pub fn parse_json(text: &str) -> Result<Value, String> {
    let stripped = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    serde_json::from_str(stripped).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_canonical_non_negative_decimal_string_ids_only() {
        for (input, expected) in [
            (JsonRpcId::String("0".into()), JsonRpcId::number(0)),
            (JsonRpcId::String("1".into()), JsonRpcId::number(1)),
            (JsonRpcId::String("42".into()), JsonRpcId::number(42)),
            (JsonRpcId::number(7), JsonRpcId::number(7)),
            (
                JsonRpcId::String("01".into()),
                JsonRpcId::String("01".into()),
            ),
            (
                JsonRpcId::String("-1".into()),
                JsonRpcId::String("-1".into()),
            ),
            (
                JsonRpcId::String("1.0".into()),
                JsonRpcId::String("1.0".into()),
            ),
            (
                JsonRpcId::String(" 1 ".into()),
                JsonRpcId::String(" 1 ".into()),
            ),
            (
                JsonRpcId::String(String::new()),
                JsonRpcId::String(String::new()),
            ),
            (
                JsonRpcId::String("abc".into()),
                JsonRpcId::String("abc".into()),
            ),
            (
                JsonRpcId::String("99999999999999999999999".into()),
                JsonRpcId::String("99999999999999999999999".into()),
            ),
        ] {
            assert_eq!(normalize_own_request_id(&input), expected);
        }
    }
}
