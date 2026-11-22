//! The `mcpServers` configuration shape that MCP clients share
//! (`packages/mcp/src/config.ts`), so configurations can be copied between them: stdio servers
//! (`command`, `args`, `env`, `cwd`), Streamable HTTP servers (`url`, `headers`, `oauth`), and
//! `enabled`, `description` and `timeout` for both.
//!
//! Applications add fields of their own: validation keeps fields it does not know for the caller
//! to check afterwards.

use std::sync::Arc;

use serde_json::Value;
use url::Url;

/// Checks a configured `oauth.callbackUrl` against the application's Client ID Metadata Documents.
type CheckMetadataDocumentCallback = Arc<dyn Fn(Option<&str>) -> Option<String> + Send + Sync>;

/// What the application adds to the validation of server entries.
#[derive(Clone, Default)]
pub struct McpServerConfigOptions {
    /// Checks the `oauth.callbackUrl` (None: none configured) of an entry with
    /// `oauth.clientRegistration: "cimd"`: the application's Client ID Metadata Documents must
    /// list it. Returns an error message. Without this check, `"cimd"` is refused.
    pub check_metadata_document_callback: Option<CheckMetadataDocumentCallback>,
}

/// One validated entry of the `mcpServers` shape, tagged with its transport, or why it is invalid.
/// The config is the entry as given: fields the validator does not know are kept, unchecked, for
/// the caller to check.
#[derive(Clone, Debug, PartialEq)]
pub enum McpServerConfigResult {
    Stdio(Value),
    Http(Value),
    Invalid(String),
}

/// Hosts a loopback callback server may serve.
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Validate one entry of the `mcpServers` shape (configuration files are a trust boundary). Server
/// names are letters, digits, `_` and `-`, which tool names and namespaces are built from.
pub fn validate_mcp_server_config(
    name: &str,
    raw: &Value,
    options: &McpServerConfigOptions,
) -> McpServerConfigResult {
    let invalid = |error: String| McpServerConfigResult::Invalid(error);
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return invalid(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(entry) = raw.as_object() else {
        return invalid(format!("server \"{name}\" must be an object"));
    };
    if let Some(error) = validate_base_fields(entry) {
        return invalid(format!("server \"{name}\": {error}"));
    }
    let kind = entry.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return invalid(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    let url = entry.get("url").and_then(Value::as_str);
    let command = entry.get("command").and_then(Value::as_str);
    if let Some(url) =
        url.filter(|_| kind.is_none() || kind == Some("http") || kind == Some("streamable-http"))
    {
        if let Some(error) = validate_http_fields(entry, url, options) {
            return invalid(format!("server \"{name}\": {error}"));
        }
        return McpServerConfigResult::Http(raw.clone());
    }
    if let Some(_command) = command.filter(|_| kind.is_none() || kind == Some("stdio")) {
        if let Some(error) = validate_stdio_fields(entry) {
            return invalid(format!("server \"{name}\": {error}"));
        }
        return McpServerConfigResult::Stdio(raw.clone());
    }
    invalid(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

/// Whether an entry overrides a server defined elsewhere instead of defining one: it has no
/// `command`, `url` or `type`. Applications let a less trusted file (a project's) override a few
/// fields, such as `enabled`, of a server a trusted file defines; which fields is theirs to decide.
pub fn is_mcp_server_override(entry: &Value) -> bool {
    entry.as_object().is_some_and(|entry| {
        !entry.contains_key("command") && !entry.contains_key("url") && !entry.contains_key("type")
    })
}

/// Fields every server entry may have.
fn validate_base_fields(entry: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(enabled) = entry.get("enabled")
        && !enabled.is_boolean()
    {
        return Some("enabled must be a boolean".to_string());
    }
    if let Some(description) = entry.get("description")
        && !description.is_string()
    {
        return Some("description must be a string".to_string());
    }
    if let Some(timeout) = entry.get("timeout") {
        let valid = timeout.as_f64().is_some_and(|timeout| timeout > 0.0);
        if !valid {
            return Some("timeout must be a positive number of seconds".to_string());
        }
    }
    None
}

fn validate_http_fields(
    entry: &serde_json::Map<String, Value>,
    url: &str,
    options: &McpServerConfigOptions,
) -> Option<String> {
    let Ok(parsed) = Url::parse(url) else {
        return Some("url must be an http or https URL".to_string());
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Some("url must be an http or https URL".to_string());
    }
    if let Some(headers) = entry.get("headers")
        && !is_string_record(headers)
    {
        return Some("headers must map names to strings".to_string());
    }
    validate_oauth(entry.get("oauth"), options)
}

fn validate_stdio_fields(entry: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(args) = entry.get("args") {
        let valid = args
            .as_array()
            .is_some_and(|args| args.iter().all(|arg| arg.is_string()));
        if !valid {
            return Some("args must be an array of strings".to_string());
        }
    }
    if let Some(env) = entry.get("env")
        && !is_string_record(env)
    {
        return Some("env must map names to strings".to_string());
    }
    if let Some(cwd) = entry.get("cwd")
        && !cwd.is_string()
    {
        return Some("cwd must be a string".to_string());
    }
    None
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.values().all(|value| value.is_string()))
}

/// Validates the `oauth` settings of an HTTP server entry.
fn validate_oauth(value: Option<&Value>, options: &McpServerConfigOptions) -> Option<String> {
    let value = value?;
    let Some(oauth) = value.as_object() else {
        return Some("oauth must be an object".to_string());
    };
    if let Some(client_id) = oauth.get("clientId")
        && !client_id.is_string()
    {
        return Some("oauth.clientId must be a string".to_string());
    }
    if let Some(client_secret) = oauth.get("clientSecret")
        && !client_secret.is_string()
    {
        return Some("oauth.clientSecret must be a string".to_string());
    }
    let callback_port = oauth.get("callbackPort");
    if let Some(port) = callback_port {
        let valid = port
            .as_f64()
            .is_some_and(|port| port.fract() == 0.0 && (1.0..=65535.0).contains(&port));
        if !valid {
            return Some("oauth.callbackPort must be a port number".to_string());
        }
    }
    let callback_port = callback_port.and_then(Value::as_f64);
    if let Some(callback_url) = oauth.get("callbackUrl") {
        let parsed = callback_url.as_str().and_then(parse_loopback_redirect_uri);
        let Some(url) = parsed else {
            return Some(
				"oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment"
					.to_string(),
			);
        };
        // A URL without a port (`http://localhost/cb`) listens on `callbackPort` or a free one, so
        // only a configured port and a URL port must agree.
        let url_port = url.port().map(f64::from);
        if let (Some(url_port), Some(port)) = (url_port, callback_port)
            && url_port != port
        {
            return Some(
                "oauth.callbackUrl and oauth.callbackPort name different ports".to_string(),
            );
        }
    }
    if let Some(scope) = oauth.get("scope")
        && !scope.is_string()
    {
        return Some("oauth.scope must be a string".to_string());
    }
    if let Some(client_name) = oauth.get("clientName") {
        let valid = client_name
            .as_str()
            .is_some_and(|name| !js_trim(name).is_empty());
        if !valid {
            return Some("oauth.clientName must be a non-empty string".to_string());
        }
    }
    let registration = oauth.get("clientRegistration").and_then(Value::as_str);
    if let Some(registration) =
        registration.filter(|registration| *registration != "auto" && *registration != "dcr")
    {
        if registration != "cimd" {
            return Some(
                "oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\"".to_string(),
            );
        }
        if oauth.contains_key("clientId") || oauth.contains_key("clientName") {
            return Some(
				"oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName"
					.to_string(),
			);
        }
        let Some(check) = &options.check_metadata_document_callback else {
            return Some(
				"oauth.clientRegistration \"cimd\" is not supported: there are no Client ID Metadata Documents"
					.to_string(),
			);
        };
        let callback_url = oauth.get("callbackUrl").and_then(Value::as_str);
        if let Some(error) = check(callback_url) {
            return Some(error);
        }
    }
    if let Some(metadata_url) = oauth.get("authServerMetadataUrl") {
        let parsed = metadata_url.as_str().and_then(|url| Url::parse(url).ok());
        let valid = parsed.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && LOOPBACK_HOSTS.contains(&url.host_str().unwrap_or_default()))
        });
        if !valid {
            return Some(
				"oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]"
					.to_string(),
			);
        }
    }
    None
}

/// Whether a redirect URI can be served by a loopback callback server.
fn parse_loopback_redirect_uri(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "http"
        || !LOOPBACK_HOSTS.contains(&url.host_str().unwrap_or_default())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

/// `String.prototype.trim()` per ECMAScript, for the one place config validation trims.
fn js_trim(text: &str) -> &str {
    let is_space = |c: char| {
        matches!(
            c,
            '\u{0009}'
                | '\u{000A}'
                | '\u{000B}'
                | '\u{000C}'
                | '\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    };
    text.trim_matches(is_space)
}
