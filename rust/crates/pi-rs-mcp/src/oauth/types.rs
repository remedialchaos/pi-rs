//! OAuth types of the MCP client subset (`packages/mcp/src/oauth/types.ts`), adapted from
//! modelcontextprotocol/typescript-sdk v1.29.0 (MIT, Anthropic PBC; see THIRD_PARTY_NOTICES),
//! modified to use dependency-free structural validation.

use serde_json::{Map, Value};
use url::Url;

use crate::oauth::errors::OAuthError;
use crate::protocol::is_object;

/// The `.well-known/oauth-protected-resource` document of an MCP server.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    #[serde(
        rename = "authorization_servers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(
        rename = "scopes_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// The authorization server's metadata document (RFC 8414 or OpenID Connect discovery).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    #[serde(rename = "authorization_endpoint")]
    pub authorization_endpoint: String,
    #[serde(rename = "token_endpoint")]
    pub token_endpoint: String,
    #[serde(
        rename = "registration_endpoint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub registration_endpoint: Option<String>,
    #[serde(
        rename = "scopes_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(rename = "response_types_supported")]
    pub response_types_supported: Vec<String>,
    #[serde(
        rename = "grant_types_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(
        rename = "token_endpoint_auth_methods_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(
        rename = "code_challenge_methods_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(
        rename = "client_id_metadata_document_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(
        rename = "authorization_response_iss_parameter_supported",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl AuthorizationServerMetadata {
    /// The methods the token endpoint accepts; absent means the client picks by what it has.
    pub fn token_endpoint_auth_methods(&self) -> &[String] {
        self.token_endpoint_auth_methods_supported
            .as_deref()
            .unwrap_or(&[])
    }
}

/// A token response.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokens {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "token_type")]
    pub token_type: String,
    #[serde(
        rename = "expires_in",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(
        rename = "refresh_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh_token: Option<String>,
    #[serde(rename = "id_token", default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// The client metadata of dynamic client registration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientMetadata {
    #[serde(
        rename = "redirect_uris",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub redirect_uris: Option<Vec<String>>,
    #[serde(
        rename = "token_endpoint_auth_method",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(
        rename = "grant_types",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub grant_types: Option<Vec<String>>,
    #[serde(
        rename = "response_types",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_types: Option<Vec<String>>,
    /// OpenID Connect client type, `native` or `web`. Dynamic client registration derives it from
    /// `redirect_uris` when absent (MCP SEP-837).
    #[serde(
        rename = "application_type",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub application_type: Option<String>,
    #[serde(
        rename = "client_name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_name: Option<String>,
    #[serde(
        rename = "client_uri",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_uri: Option<String>,
    #[serde(rename = "logo_uri", default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contacts: Option<Vec<String>>,
    #[serde(rename = "tos_uri", default, skip_serializing_if = "Option::is_none")]
    pub tos_uri: Option<String>,
    #[serde(
        rename = "policy_uri",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub policy_uri: Option<String>,
    #[serde(rename = "jwks_uri", default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(
        rename = "software_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub software_id: Option<String>,
    #[serde(
        rename = "software_version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub software_version: Option<String>,
    #[serde(
        rename = "software_statement",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub software_statement: Option<String>,
}

/// A registered client: pre-registered, registered dynamically, or identified by a Client ID
/// Metadata Document. Fields the struct does not name (the client metadata of a dynamic
/// registration) are kept in `extra`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientInformation {
    #[serde(rename = "client_id")]
    pub client_id: String,
    #[serde(
        rename = "client_secret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
    #[serde(
        rename = "client_id_issued_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_id_issued_at: Option<f64>,
    #[serde(
        rename = "client_secret_expires_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret_expires_at: Option<f64>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl OAuthClientInformation {
    /// A client identified by a Client ID Metadata Document: its URL is the `client_id`.
    pub fn from_document_url(url: &str) -> Self {
        Self {
            client_id: url.to_string(),
            client_secret: None,
            client_id_issued_at: None,
            client_secret_expires_at: None,
            extra: Map::new(),
        }
    }
}

/// What a flow discovered about the server's authorization.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthDiscoveryState {
    #[serde(rename = "authorizationServerUrl")]
    pub authorization_server_url: String,
    #[serde(
        rename = "authorizationServerMetadata",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(
        rename = "resourceMetadata",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
    #[serde(
        rename = "resourceMetadataUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resource_metadata_url: Option<String>,
}

/// Discovery outcome for one MCP server: its authorization server and both metadata documents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// A `WWW-Authenticate` challenge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthChallenge {
    /// The `resource_metadata` URL, when the challenge names a parseable one.
    pub resource_metadata_url: Option<Url>,
    /// The scopes the challenge asks for.
    pub scope: Option<String>,
    /// The `error` of the challenge, for example `insufficient_scope`.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn object(value: &Value, name: &str) -> Result<Map<String, Value>, OAuthError> {
    if !is_object(value) {
        return Err(OAuthError::Message(format!("Invalid {name}")));
    }
    Ok(value.as_object().cloned().unwrap_or_default())
}

/// Treats `null` and `""` as absent: servers send them for fields they have no value for, like
/// `scope: ""`.
fn absent(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(_) => false,
    }
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(OAuthError::Message(format!("Invalid {name}"))),
    }
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthError> {
    if absent(value) {
        return Ok(None);
    }
    required_string(value, name).map(Some)
}

fn optional_strings(value: Option<&Value>, name: &str) -> Result<Option<Vec<String>>, OAuthError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                out.push(
                    item.as_str()
                        .ok_or_else(|| OAuthError::Message(format!("Invalid {name}")))?
                        .to_string(),
                );
            }
            Ok(Some(out))
        }
        Some(_) => Err(OAuthError::Message(format!("Invalid {name}"))),
    }
}

/// Accepts only `http:` and `https:` URLs. Servers supply these URLs, and the authorization
/// endpoint is opened with the platform URL handler, so a `file:` or custom protocol URL would
/// launch local programs.
fn safe_url(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    let text = required_string(value, name)?;
    if !Url::parse(&text).is_ok_and(|url| is_http_url(&url)) {
        return Err(OAuthError::Message(format!("Invalid {name}")));
    }
    Ok(text)
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthError> {
    if absent(value) {
        return Ok(None);
    }
    safe_url(value, name).map(Some)
}

/// Whether a URL is one the platform URL handler may open.
pub fn is_http_url(url: &Url) -> bool {
    url.scheme() == "http" || url.scheme() == "https"
}

/// Parses the `.well-known/oauth-protected-resource` document.
pub fn parse_protected_resource_metadata(
    value: &Value,
) -> Result<OAuthProtectedResourceMetadata, OAuthError> {
    let input = object(value, "OAuth protected resource metadata")?;
    let metadata = OAuthProtectedResourceMetadata {
        resource: safe_url(
            input.get("resource"),
            "OAuth protected resource metadata resource",
        )?,
        authorization_servers: match optional_strings(
            input.get("authorization_servers"),
            "authorization_servers",
        )? {
            None => None,
            Some(urls) => {
                let mut safe = Vec::new();
                for url in urls {
                    safe.push(safe_url(
                        Some(&Value::String(url.clone())),
                        "authorization server URL",
                    )?);
                }
                Some(safe)
            }
        },
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        extra: input
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "resource" | "authorization_servers" | "scopes_supported"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    };
    Ok(metadata)
}

/// Parses an authorization server's metadata document, validating its required URLs.
pub fn parse_authorization_server_metadata(
    value: &Value,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    let input = object(value, "authorization server metadata")?;
    let response_types = optional_strings(
        input.get("response_types_supported"),
        "response_types_supported",
    )?;
    if response_types.is_none() {
        return Err(OAuthError::Message(
            "Invalid response_types_supported".to_string(),
        ));
    }
    let metadata = AuthorizationServerMetadata {
        issuer: safe_url(input.get("issuer"), "authorization server issuer")?,
        authorization_endpoint: safe_url(
            input.get("authorization_endpoint"),
            "authorization endpoint",
        )?,
        token_endpoint: safe_url(input.get("token_endpoint"), "token endpoint")?,
        registration_endpoint: optional_url(
            input.get("registration_endpoint"),
            "registration endpoint",
        )?,
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        response_types_supported: response_types.unwrap_or_default(),
        grant_types_supported: optional_strings(
            input.get("grant_types_supported"),
            "grant_types_supported",
        )?,
        token_endpoint_auth_methods_supported: optional_strings(
            input.get("token_endpoint_auth_methods_supported"),
            "token_endpoint_auth_methods_supported",
        )?,
        code_challenge_methods_supported: optional_strings(
            input.get("code_challenge_methods_supported"),
            "code_challenge_methods_supported",
        )?,
        client_id_metadata_document_supported: input
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool),
        authorization_response_iss_parameter_supported: input
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool),
        extra: input
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "issuer"
                        | "authorization_endpoint"
                        | "token_endpoint"
                        | "registration_endpoint"
                        | "scopes_supported"
                        | "response_types_supported"
                        | "grant_types_supported"
                        | "token_endpoint_auth_methods_supported"
                        | "code_challenge_methods_supported"
                        | "client_id_metadata_document_supported"
                        | "authorization_response_iss_parameter_supported"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    };
    Ok(metadata)
}

/// Parses a token response. `null` and `""` count as absent, and a non-finite `expires_in` is
/// refused (`Number(null)` would be 0, which would mark the token as expired at once).
pub fn parse_oauth_tokens(value: &Value) -> Result<OAuthTokens, OAuthError> {
    let input = object(value, "OAuth token response")?;
    let expires = match input.get("expires_in") {
        Some(expires) if !absent(Some(expires)) => {
            let number = expires
                .as_f64()
                .filter(|expires| expires.is_finite())
                .ok_or_else(|| OAuthError::Message("Invalid expires_in".to_string()))?;
            Some(number)
        }
        _ => None,
    };
    Ok(OAuthTokens {
        access_token: required_string(input.get("access_token"), "access_token")?,
        token_type: required_string(input.get("token_type"), "token_type")?,
        expires_in: expires,
        scope: optional_string(input.get("scope"), "scope")?,
        refresh_token: optional_string(input.get("refresh_token"), "refresh_token")?,
        id_token: optional_string(input.get("id_token"), "id_token")?,
    })
}

/// Parses a dynamic client registration response.
pub fn parse_client_information(value: &Value) -> Result<OAuthClientInformation, OAuthError> {
    let input = object(value, "OAuth client registration response")?;
    Ok(OAuthClientInformation {
        client_id: required_string(input.get("client_id"), "client_id")?,
        client_secret: optional_string(input.get("client_secret"), "client_secret")?,
        client_id_issued_at: input.get("client_id_issued_at").and_then(Value::as_f64),
        client_secret_expires_at: input
            .get("client_secret_expires_at")
            .and_then(Value::as_f64),
        extra: input
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "client_id"
                        | "client_secret"
                        | "client_id_issued_at"
                        | "client_secret_expires_at"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

/// A Client ID Metadata Document: an https URL used as `client_id`, and a redirect URI it lists.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthClientMetadataDocument {
    pub url: String,
    pub redirect_url: String,
}
