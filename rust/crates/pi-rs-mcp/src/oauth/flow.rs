//! The OAuth authorization flow (`packages/mcp/src/oauth/flow.ts`), adapted from the MCP
//! TypeScript SDK: discovery, dynamic client registration, PKCE, the code exchange, refreshes,
//! and the auth provider adaptation for HTTP transports.

use std::sync::Arc;

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::auth_provider::{AuthProvider, McpFetch, UnauthorizedContext};
use crate::fetch::McpFetchRequest;
use crate::oauth::discovery::{
    DiscoveryOptions, discover_authorization_server_metadata, discover_oauth_server_info,
    parse_www_authenticate, select_resource,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::provider::McpStoreError;
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata,
    OAuthClientMetadataDocument, OAuthDiscoveryState, OAuthProtectedResourceMetadata, OAuthTokens,
    parse_client_information, parse_oauth_tokens,
};
use crate::protocol::is_object;

impl From<McpStoreError> for OAuthError {
    fn from(error: McpStoreError) -> Self {
        match error {
            McpStoreError::RefreshLockBusy => Self::RefreshLockBusy,
            McpStoreError::Other(message) => Self::Message(message),
        }
    }
}

/// Adds client authentication to a token request, in place of the default client-id methods.
pub type AddClientAuthentication = Arc<
    dyn Fn(
            &mut Vec<(String, String)>,
            &mut Vec<(String, String)>,
            &Url,
            Option<&AuthorizationServerMetadata>,
        ) + Send
        + Sync,
>;

/// A Client ID Metadata Document: an https URL used as `client_id`, and a redirect URI it lists.
pub use crate::oauth::types::OAuthClientMetadataDocument as ClientMetadataDocument;

/// Everything the flow needs from the application: where its state lives, how it registers,
/// where it redirects, and what it does when the user has to sign in.
pub trait OAuthClientProvider: Send + Sync {
    /// The redirect URI the authorization request uses.
    fn redirect_url(&self) -> String;
    /// The client metadata of dynamic registration.
    fn client_metadata(&self) -> OAuthClientMetadata;
    /// Client ID Metadata Document to identify as instead of registering dynamically, or `None` to
    /// register. Called when no client information is stored; the document is not stored.
    /// `metadata` is `None` when the authorization server has none; check
    /// `client_id_metadata_document_supported`.
    fn client_metadata_document(
        &self,
        _metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<OAuthClientMetadataDocument>, String> {
        Ok(None)
    }
    /// The `state` of the authorization request, saved by the flow.
    fn state(&self) -> StoreFuture<'_, Option<String>> {
        Box::pin(async { Ok(None) })
    }
    fn client_information(&self) -> StoreFuture<'_, Option<OAuthClientInformation>>;
    /// Fails with "OAuth client information cannot be persisted" when the provider cannot store a
    /// client, which is what the flow needs to know before registering one.
    fn save_client_information(&self, _information: OAuthClientInformation) -> StoreFuture<'_, ()> {
        Box::pin(async {
            Err(McpStoreError::Other(
                "OAuth client information cannot be persisted".to_string(),
            ))
        })
    }
    fn tokens(&self) -> StoreFuture<'_, Option<OAuthTokens>>;
    fn save_tokens(&self, tokens: OAuthTokens) -> StoreFuture<'_, ()>;
    /// Called with the URL the user's browser has to open.
    fn redirect_to_authorization(&self, url: &Url) -> StoreFuture<'_, ()>;
    fn save_code_verifier(&self, verifier: String) -> StoreFuture<'_, ()>;
    fn code_verifier(&self) -> StoreFuture<'_, String>;
    fn add_client_authentication(&self) -> Option<AddClientAuthentication> {
        None
    }
    fn invalidate_credentials(&self, _kind: InvalidateKind) -> StoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn save_discovery_state(&self, _state: OAuthDiscoveryState) -> StoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn discovery_state(&self) -> StoreFuture<'_, Option<OAuthDiscoveryState>> {
        Box::pin(async { Ok(None) })
    }
}

/// A future of one store operation.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, McpStoreError>> + Send + 'a>>;

use std::future::Future;
use std::pin::Pin;

/// What a flow used, for deciding what to invalidate after it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidateKind {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

/// Options of one run of [`authorize_mcp`].
#[derive(Clone, Default)]
pub struct OAuthFlowOptions {
    pub server_url: String,
    pub authorization_code: Option<String>,
    /// `iss` parameter of the authorization response that delivered `authorization_code`
    /// (RFC 9207).
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<Url>,
    /// Authorization server metadata document to use instead of discovery, for servers that
    /// advertise a wrong authorization server or none. It is trusted as configured. Must use
    /// https, except on loopback.
    pub authorization_server_metadata_url: Option<Url>,
    pub fetch: Option<McpFetch>,
    /// Aborts every request of the flow. Requests have no time limit of their own; combine with a
    /// timeout as needed.
    pub signal: Option<crate::abort::AbortSignal>,
    pub skip_issuer_validation: bool,
    /// Go straight to the authorization redirect instead of refreshing stored tokens, for example
    /// when the server asks for scopes the current grant lacks (a refresh keeps the old scope).
    pub skip_refresh: bool,
    /// Only refresh the stored tokens: return [`OAuthFlowResult::Redirect`] instead of registering
    /// a client or preparing an authorization request, so a pending authorization (its `state`
    /// and PKCE verifier) stays usable.
    pub refresh_only: bool,
    /// With `authorization_code`: the redirect URI the authorization request used. The exchange
    /// fails before sending the code when the flow would send another one (RFC 6749 section
    /// 4.1.3 requires the same).
    pub authorization_redirect_url: Option<String>,
}

/// How a flow ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthFlowResult {
    /// Tokens were refreshed or exchanged and saved.
    Authorized,
    /// The user has to authorize in a browser; the flow prepared that step.
    Redirect,
}

/// Options of the token request helpers.
#[derive(Clone)]
pub struct TokenRequestOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_information: OAuthClientInformation,
    pub resource: Option<String>,
    pub add_client_authentication: Option<AddClientAuthentication>,
    pub fetch: Option<McpFetch>,
    pub signal: Option<crate::abort::AbortSignal>,
}

/// Whether a hostname is loopback, brackets included.
fn loopback(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The OpenID Connect `application_type` for `redirect_uris` (MCP SEP-837). Without one, OpenID
/// Connect servers assume `web`, which rejects http loopback redirect URIs. Loopback hosts and
/// custom schemes are native (RFC 8252).
fn application_type(redirect_uris: &[String]) -> &'static str {
    let native = redirect_uris.iter().any(|uri| match Url::parse(uri) {
        Ok(url) => {
            (url.scheme() != "http" && url.scheme() != "https")
                || loopback(url.host_str().unwrap_or_default())
        }
        Err(_) => false,
    });
    if native { "native" } else { "web" }
}

/// The token endpoint, refused unless it is https or loopback.
fn secure_endpoint(value: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(value).map_err(|error| OAuthError::Message(error.to_string()))?;
    if url.scheme() != "https" && !loopback(url.host_str().unwrap_or_default()) {
        return Err(OAuthError::Insecure {
            endpoint: url.to_string(),
        });
    }
    Ok(url)
}

/// How the client authenticates at the token endpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientAuthMethod {
    Basic,
    Post,
    None,
}

fn select_client_auth_method(
    information: &OAuthClientInformation,
    supported: &[String],
) -> ClientAuthMethod {
    let hinted = information
        .extra
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str);
    let usable = |method: &str| {
        matches!(
            method,
            "client_secret_basic" | "client_secret_post" | "none"
        ) && (supported.is_empty() || supported.iter().any(|candidate| candidate == method))
    };
    if let Some(hinted) = hinted.filter(|hinted| usable(hinted)) {
        return match hinted {
            "client_secret_basic" => ClientAuthMethod::Basic,
            "client_secret_post" => ClientAuthMethod::Post,
            _ => ClientAuthMethod::None,
        };
    }
    if supported.is_empty() {
        return if information.client_secret.is_some() {
            ClientAuthMethod::Basic
        } else {
            ClientAuthMethod::None
        };
    }
    if information.client_secret.is_some()
        && supported
            .iter()
            .any(|method| method == "client_secret_basic")
    {
        return ClientAuthMethod::Basic;
    }
    if information.client_secret.is_some()
        && supported
            .iter()
            .any(|method| method == "client_secret_post")
    {
        return ClientAuthMethod::Post;
    }
    if supported.iter().any(|method| method == "none") {
        return ClientAuthMethod::None;
    }
    if information.client_secret.is_some() {
        ClientAuthMethod::Post
    } else {
        ClientAuthMethod::None
    }
}

/// The client authentication of a token request, in headers or body parameters.
fn apply_client_authentication(
    method: ClientAuthMethod,
    information: &OAuthClientInformation,
    headers: &mut Vec<(String, String)>,
    params: &mut Vec<(String, String)>,
) -> Result<(), OAuthError> {
    match method {
        ClientAuthMethod::Basic => {
            let Some(secret) = &information.client_secret else {
                return Err(OAuthError::Message(
                    "client_secret_basic requires a client secret".to_string(),
                ));
            };
            let credentials = format!("{}:{secret}", information.client_id);
            let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
            set_header(headers, "Authorization", &format!("Basic {encoded}"));
        }
        ClientAuthMethod::Post => {
            params.push(("client_id".to_string(), information.client_id.clone()));
            if let Some(secret) = &information.client_secret {
                params.push(("client_secret".to_string(), secret.clone()));
            }
        }
        ClientAuthMethod::None => {
            params.push(("client_id".to_string(), information.client_id.clone()));
        }
    }
    Ok(())
}

/// A fresh PKCE pair: the verifier, and its S256 challenge.
fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = engine.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, engine.encode(digest))
}

/// The authorization request the browser step builds.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    pub authorization_url: Url,
    pub code_verifier: String,
}

/// Prepares the browser step: checks the server supports authorization codes and PKCE S256,
/// builds the authorization URL with a fresh PKCE pair.
pub async fn start_authorization(
    authorization_server_url: &str,
    options: StartAuthorizationOptions<'_>,
) -> Result<AuthorizationRequest, OAuthError> {
    let metadata = options.metadata;
    if let Some(metadata) = metadata.filter(|metadata| {
        !metadata
            .response_types_supported
            .iter()
            .any(|kind| kind == "code")
    }) {
        let _ = metadata;
        return Err(OAuthError::Message(
            "Authorization server does not support authorization codes".to_string(),
        ));
    }
    if let Some(metadata) = metadata
        && let Some(methods) = &metadata.code_challenge_methods_supported
        && !methods.iter().any(|method| method == "S256")
    {
        return Err(OAuthError::Message(
            "Authorization server does not support PKCE S256".to_string(),
        ));
    }
    let endpoint = metadata
        .map(|metadata| metadata.authorization_endpoint.clone())
        .unwrap_or_else(|| format!("{authorization_server_url}/authorize"));
    let mut url = Url::parse(&endpoint).map_err(|error| OAuthError::Message(error.to_string()))?;
    // Callers open this URL with the platform URL handler, which also launches files and custom
    // protocols.
    if !crate::oauth::types::is_http_url(&url) {
        return Err(OAuthError::Message(format!(
            "Invalid authorization endpoint {}",
            url.as_str()
        )));
    }
    let (verifier, challenge) = pkce();
    let mut query = url.query_pairs_mut();
    query.clear();
    query.append_pair("response_type", "code");
    query.append_pair("client_id", &options.client_information.client_id);
    query.append_pair("code_challenge", &challenge);
    query.append_pair("code_challenge_method", "S256");
    query.append_pair("redirect_uri", options.redirect_url);
    if let Some(state) = options.state {
        query.append_pair("state", state);
    }
    if let Some(scope) = options.scope {
        query.append_pair("scope", scope);
        if scope
            .split_whitespace()
            .any(|part| part == "offline_access")
        {
            query.append_pair("prompt", "consent");
        }
    }
    if let Some(resource) = options.resource {
        query.append_pair("resource", resource);
    }
    drop(query);
    Ok(AuthorizationRequest {
        authorization_url: url,
        code_verifier: verifier,
    })
}

/// Options of [`start_authorization`].
#[derive(Clone, Copy)]
pub struct StartAuthorizationOptions<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_information: &'a OAuthClientInformation,
    pub redirect_url: &'a str,
    pub scope: Option<&'a str>,
    pub state: Option<&'a str>,
    pub resource: Option<&'a str>,
}

/// One token request: the endpoint, the client's authentication, and the parameters.
async fn token_request(
    authorization_server_url: &str,
    options: &TokenRequestOptions,
    params: Vec<(String, String)>,
) -> Result<OAuthTokens, OAuthError> {
    let url = secure_endpoint(
        &options
            .metadata
            .as_ref()
            .map(|metadata| metadata.token_endpoint.clone())
            .unwrap_or_else(|| format!("{authorization_server_url}/token")),
    )?;
    let mut headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        (
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ),
    ];
    let mut params = params;
    if let Some(resource) = &options.resource {
        params.retain(|(name, _)| name != "resource");
        params.push(("resource".to_string(), resource.clone()));
    }
    if let Some(add_authentication) = &options.add_client_authentication {
        add_authentication(&mut headers, &mut params, &url, options.metadata.as_ref());
    } else {
        let method = select_client_auth_method(
            &options.client_information,
            options
                .metadata
                .as_ref()
                .map(|metadata| metadata.token_endpoint_auth_methods())
                .unwrap_or_default(),
        );
        apply_client_authentication(
            method,
            &options.client_information,
            &mut headers,
            &mut params,
        )?;
    }
    let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
    let request = McpFetchRequest {
        url: url.to_string(),
        method: http::Method::POST,
        headers,
        body: Some(form_encode(&params)),
        signal: options.signal.clone(),
    };
    let mut response = fetch.call(request).await.map_err(|error| match error {
        crate::fetch::McpFetchError::Network(message) => OAuthError::Network(message),
        crate::fetch::McpFetchError::Timeout(message)
        | crate::fetch::McpFetchError::Other(message) => OAuthError::Message(message),
    })?;
    let (text, _) = crate::fetch::read_body_text(&mut response, TOKEN_BODY_MAX)
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    let value = crate::protocol::parse_json(&text).ok();
    // Servers may report OAuth errors with any status, so check the body before the status.
    if let Some(value) = value.as_ref().filter(|value| is_object(value))
        && let Some(error) = value.get("error").and_then(Value::as_str)
    {
        let description = value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or(error);
        return Err(OAuthError::Protocol {
            code: error.to_string(),
            message: description.to_string(),
            error_uri: value
                .get("error_uri")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if !response.ok() {
        return Err(OAuthError::Protocol {
            code: "server_error".to_string(),
            message: format!("HTTP {}: {}", response.status, text),
            error_uri: None,
        });
    }
    let value = value.unwrap_or(Value::Null);
    parse_oauth_tokens(&value)
}

/// Token bodies are small; anything larger is a server error away from being read whole.
const TOKEN_BODY_MAX: usize = 1024 * 1024;

/// Sets one header, matching case-insensitively, like the `Headers` upstream uses.
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

/// URL-encoded form bodies, like `URLSearchParams` serializes.
fn form_encode(params: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in params {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>());
        out.push('=');
        out.push_str(&url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>());
    }
    out
}

/// Registers a client dynamically, deriving `application_type` from the redirect URIs.
pub async fn register_client(
    authorization_server_url: &str,
    options: RegisterClientOptions<'_>,
) -> Result<OAuthClientInformation, OAuthError> {
    let endpoint = options
        .metadata
        .and_then(|metadata| metadata.registration_endpoint.clone());
    if options.metadata.is_some() && endpoint.is_none() {
        return Err(OAuthError::Message(
            "Authorization server does not support dynamic client registration".to_string(),
        ));
    }
    let endpoint = endpoint.unwrap_or_else(|| format!("{authorization_server_url}/register"));
    let mut body = serde_json::to_value(options.client_metadata)
        .map_err(|error| OAuthError::Message(error.to_string()))?;
    if let Value::Object(object) = &mut body {
        object
            .entry("application_type".to_string())
            .or_insert_with(|| {
                json!(application_type(
                    options
                        .client_metadata
                        .redirect_uris
                        .as_deref()
                        .unwrap_or_default()
                ))
            });
        if let Some(scope) = options.scope {
            object.insert("scope".to_string(), json!(scope));
        }
    }
    let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
    let request = McpFetchRequest {
        url: endpoint,
        method: http::Method::POST,
        headers: vec![
            ("Accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ],
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        signal: options.signal.clone(),
    };
    let mut response = fetch.call(request).await.map_err(|error| match error {
        crate::fetch::McpFetchError::Network(message) => OAuthError::Network(message),
        crate::fetch::McpFetchError::Timeout(message)
        | crate::fetch::McpFetchError::Other(message) => OAuthError::Message(message),
    })?;
    if !response.ok() {
        let (text, _) = crate::fetch::read_body_text(&mut response, TOKEN_BODY_MAX)
            .await
            .unwrap_or_default();
        return Err(OAuthError::Registration {
            status: response.status,
            body: text,
        });
    }
    let (text, _) = crate::fetch::read_body_text(&mut response, TOKEN_BODY_MAX)
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    let value = crate::protocol::parse_json(&text).map_err(OAuthError::Message)?;
    parse_client_information(&value)
}

/// Options of [`register_client`].
#[derive(Clone)]
pub struct RegisterClientOptions<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_metadata: &'a OAuthClientMetadata,
    pub scope: Option<&'a str>,
    pub fetch: Option<McpFetch>,
    pub signal: Option<crate::abort::AbortSignal>,
}

/// Exchanges an authorization code for tokens.
pub async fn exchange_authorization_code(
    authorization_server_url: &str,
    options: TokenRequestOptions,
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> Result<OAuthTokens, OAuthError> {
    token_request(
        authorization_server_url,
        &options,
        vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), code_verifier.to_string()),
            ("redirect_uri".to_string(), redirect_url.to_string()),
        ],
    )
    .await
}

/// Refreshes tokens with a refresh token, keeping the grant's refresh token when the response
/// names none.
pub async fn refresh_authorization(
    authorization_server_url: &str,
    options: TokenRequestOptions,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut tokens = token_request(
        authorization_server_url,
        &options,
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ],
    )
    .await?;
    // A rotated token response names its own refresh token, which wins; without one the old
    // refresh token keeps the grant alive, like `{ refresh_token, ...tokens }` upstream.
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    Ok(tokens)
}

/// A refresh response without `scope` keeps the scope of the grant (RFC 6749 §6); a code
/// exchange without one grants the requested scope (§5.1).
fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none()
        && let Some(scope) = scope
    {
        tokens.scope = Some(scope.to_string());
    }
    tokens
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones granted so far, since
/// a challenge may list only the missing scopes and a token with just those would lose access the
/// old one had (SEP-2350). Without challenged scopes, `None` lets the flow pick its default.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged?;
    let mut scopes: Vec<&str> = Vec::new();
    for scope in [granted, Some(challenged)].into_iter().flatten() {
        for part in scope.split_whitespace().filter(|part| !part.is_empty()) {
            if !scopes.contains(&part) {
                scopes.push(part);
            }
        }
    }
    Some(scopes.join(" "))
}

/// What a flow used, for deciding what to invalidate after it failed.
#[derive(Default)]
struct FlowUse {
    /// `client_id` of the stored client the flow used; `None` when none was stored.
    stored_client_id: Option<String>,
}

/// One full run of the flow, from discovery to saved tokens or a prepared browser step.
async fn run_flow(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
    use_: &mut FlowUse,
) -> Result<OAuthFlowResult, OAuthError> {
    let metadata_url = match &options.authorization_server_metadata_url {
        Some(url) => Some(secure_endpoint(url.as_str())?),
        None => None,
    };
    // With a configured metadata URL, discovery is not cached, so changing the URL applies at
    // once.
    let cached = match &metadata_url {
        None => provider.discovery_state().await?,
        Some(_) => None,
    };
    let discovered: Discovered = match cached
        .as_ref()
        .filter(|cached| !cached.authorization_server_url.is_empty())
    {
        Some(cached) => {
            let metadata = match &cached.authorization_server_metadata {
                Some(metadata) => Some(metadata.clone()),
                None => {
                    discover_authorization_server_metadata(
                        &cached.authorization_server_url,
                        DiscoveryOptions {
                            fetch: options.fetch.clone(),
                            skip_issuer_validation: options.skip_issuer_validation,
                            signal: options.signal.as_ref(),
                            ..DiscoveryOptions::default()
                        },
                    )
                    .await?
                }
            };
            Discovered {
                authorization_server_url: cached.authorization_server_url.clone(),
                authorization_server_metadata: metadata,
                resource_metadata: cached.resource_metadata.clone(),
            }
        }
        None => {
            let info = discover_oauth_server_info(
                &options.server_url,
                DiscoveryOptions {
                    resource_metadata_url: options.resource_metadata_url.as_ref(),
                    authorization_server_metadata_url: metadata_url.as_ref(),
                    fetch: options.fetch.clone(),
                    skip_issuer_validation: options.skip_issuer_validation,
                    signal: options.signal.as_ref(),
                    ..DiscoveryOptions::default()
                },
            )
            .await?;
            Discovered {
                authorization_server_url: info.authorization_server_url,
                authorization_server_metadata: info.authorization_server_metadata,
                resource_metadata: info.resource_metadata,
            }
        }
    };
    if metadata_url.is_none() {
        provider
            .save_discovery_state(OAuthDiscoveryState {
                authorization_server_url: discovered.authorization_server_url.clone(),
                authorization_server_metadata: discovered.authorization_server_metadata.clone(),
                resource_metadata: discovered.resource_metadata.clone(),
                resource_metadata_url: options
                    .resource_metadata_url
                    .as_ref()
                    .map(|url| url.to_string()),
            })
            .await?;
    }
    let metadata = discovered.authorization_server_metadata.as_ref();
    let resource = select_resource(&options.server_url, discovered.resource_metadata.as_ref())?;
    // `||`, not `??`: an empty scope (for example from `scopes_supported: []`) falls through to
    // the next source.
    let scope = options
        .scope
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            discovered
                .resource_metadata
                .as_ref()
                .and_then(|metadata: &OAuthProtectedResourceMetadata| {
                    metadata
                        .scopes_supported
                        .as_ref()
                        .map(|scopes| scopes.join(" "))
                })
                .filter(|scope| !scope.is_empty())
        })
        .or_else(|| provider.client_metadata().scope.clone());

    let stored = provider.client_information().await?;
    use_.stored_client_id = stored.as_ref().map(|client| client.client_id.clone());
    let client_document = match &stored {
        Some(_) => None,
        None => provider
            .client_metadata_document(metadata)
            .map_err(OAuthError::Message)?,
    };
    if let Some(document) = &client_document {
        let url =
            Url::parse(&document.url).map_err(|error| OAuthError::Message(error.to_string()))?;
        if url.scheme() != "https" || url.path() == "/" {
            return Err(OAuthError::Message(
                "Invalid OAuth client metadata URL".to_string(),
            ));
        }
    }
    // The document's redirect URI may differ from the provider's, for example by a
    // server-specific path.
    let redirect_url = client_document
        .as_ref()
        .map(|document| document.redirect_url.clone())
        .unwrap_or_else(|| provider.redirect_url());
    // Checked first: another redirect URI also explains a client that is missing for the
    // exchange.
    if options.authorization_code.is_some()
        && options.authorization_redirect_url.is_some()
        && redirect_url
            != options
                .authorization_redirect_url
                .clone()
                .unwrap_or_default()
    {
        return Err(OAuthError::Message(format!(
            "The code exchange would use the redirect URI {redirect_url}, but the authorization request used {}",
            options
                .authorization_redirect_url
                .clone()
                .unwrap_or_default()
        )));
    }
    let client = match stored.clone().or_else(|| {
        client_document
            .as_ref()
            .map(|document| OAuthClientInformation::from_document_url(&document.url))
    }) {
        Some(client) => client,
        None => {
            if options.refresh_only {
                return Ok(OAuthFlowResult::Redirect);
            }
            if options.authorization_code.is_some() {
                return Err(OAuthError::Message(
					"OAuth client information is missing during code exchange: the authorization request's client was removed, or the client settings changed"
						.to_string(),
				));
            }
            let client_metadata = provider.client_metadata();
            let registered = register_client(
                &discovered.authorization_server_url,
                RegisterClientOptions {
                    metadata,
                    client_metadata: &client_metadata,
                    scope: scope.as_deref(),
                    fetch: options.fetch.clone(),
                    signal: options.signal.clone(),
                },
            )
            .await?;
            provider.save_client_information(registered.clone()).await?;
            registered
        }
    };
    let token_options = TokenRequestOptions {
        metadata: metadata.cloned(),
        client_information: client.clone(),
        resource: resource.clone(),
        add_client_authentication: provider.add_client_authentication(),
        fetch: options.fetch.clone(),
        signal: options.signal.clone(),
    };
    if let Some(authorization_code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to this one. Without
        // metadata there is no issuer to compare with, so any `iss` is rejected.
        let iss = options.iss.clone();
        let iss_expected = metadata
            .and_then(|metadata| metadata.authorization_response_iss_parameter_supported)
            .unwrap_or(false);
        if iss.is_some() || iss_expected {
            let expected = metadata.map(|metadata| metadata.issuer.clone());
            if iss != expected {
                return Err(OAuthError::IssuerMismatch {
                    expected,
                    received: iss,
                });
            }
        }
        let verifier = provider.code_verifier().await?;
        let tokens = exchange_authorization_code(
            &discovered.authorization_server_url,
            token_options,
            authorization_code,
            &verifier,
            &redirect_url,
        )
        .await?;
        // A response without `scope` grants the requested scope (RFC 6749 §5.1). Recorded so a
        // step-up can keep it. Callers pass the options of the authorization request, so `scope`
        // is what was requested.
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await?;
        return Ok(OAuthFlowResult::Authorized);
    }
    let existing = if options.skip_refresh {
        None
    } else {
        provider.tokens().await?
    };
    if let Some(existing) = existing.filter(|tokens| tokens.refresh_token.is_some()) {
        let refresh_token = existing.refresh_token.clone().unwrap_or_default();
        let result = refresh_authorization(
            &discovered.authorization_server_url,
            token_options,
            &refresh_token,
        )
        .await;
        match result {
            Ok(tokens) => {
                // A refresh without `scope` keeps the scope of the grant (RFC 6749 §6).
                provider
                    .save_tokens(with_scope(tokens, existing.scope.as_deref()))
                    .await?;
                return Ok(OAuthFlowResult::Authorized);
            }
            Err(error) => {
                if options
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(error);
                }
                if let OAuthError::Insecure { .. } = &error {
                    return Err(error);
                }
                if let OAuthError::Protocol { code, .. } = &error
                    && code != "server_error"
                {
                    return Err(error);
                }
            }
        }
    }
    if options.refresh_only {
        return Ok(OAuthFlowResult::Redirect);
    }
    let state = provider.state().await?;
    let authorization = start_authorization(
        &discovered.authorization_server_url,
        StartAuthorizationOptions {
            metadata,
            client_information: &client,
            redirect_url: &redirect_url,
            scope: scope.as_deref(),
            state: state.as_deref(),
            resource: resource.as_deref(),
        },
    )
    .await?;
    provider
        .save_code_verifier(authorization.code_verifier)
        .await?;
    provider
        .redirect_to_authorization(&authorization.authorization_url)
        .await?;
    Ok(OAuthFlowResult::Redirect)
}

/// What a flow run discovered.
struct Discovered {
    authorization_server_url: String,
    authorization_server_metadata: Option<AuthorizationServerMetadata>,
    resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// Runs the flow, retrying once when the stored client or tokens were rejected: a rejected
/// client invalidates the credentials and registers again; a rejected grant drops the tokens.
pub async fn authorize_mcp(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult, OAuthError> {
    let mut use_ = FlowUse::default();
    match run_flow(provider, options, &mut use_).await {
        Ok(result) => Ok(result),
        Err(error) => {
            let retriable_code = error.protocol_code().is_some_and(|code| {
                matches!(
                    code,
                    "invalid_client" | "unauthorized_client" | "invalid_grant"
                )
            });
            if !retriable_code {
                return Err(error);
            }
            // A refresh keeps a pending authorization's `state` and verifier: they are the user's
            // sign-in. A client stored meanwhile (by a sign-in that completed) is not the one the
            // server rejected, so it stays.
            if options.refresh_only && error.protocol_code() != Some("invalid_grant") {
                let current = provider.client_information().await?;
                if current.as_ref().map(|client| client.client_id.clone()) == use_.stored_client_id
                {
                    for kind in [
                        InvalidateKind::Client,
                        InvalidateKind::Tokens,
                        InvalidateKind::Discovery,
                    ] {
                        provider.invalidate_credentials(kind).await?;
                    }
                }
            } else if error.protocol_code() != Some("invalid_grant") {
                provider.invalidate_credentials(InvalidateKind::All).await?;
            } else {
                provider
                    .invalidate_credentials(InvalidateKind::Tokens)
                    .await?;
            }
            run_flow(provider, options, &mut FlowUse::default()).await
        }
    }
}

/// An auth provider for [`crate::transports::StreamableHttpTransport`] built from an
/// [`OAuthClientProvider`]. After a 401 it refreshes the tokens, or fails with
/// [`OAuthError::AuthorizationRequired`] when the user has to authorize (again). Concurrent 401s
/// share one refresh, and a request whose token was already replaced is just retried: with
/// rotating refresh tokens, a second refresh with the old refresh token would fail and discard
/// the new grant.
/// An auth provider for [`crate::transports::StreamableHttpTransport`] built from an
/// [`OAuthClientProvider`]. After a 401 it refreshes the tokens, or fails with
/// [`OAuthError::AuthorizationRequired`] when the user has to authorize (again). Concurrent 401s
/// share one refresh, and a request whose token was already replaced is just retried: with
/// rotating refresh tokens, a second refresh with the old refresh token would fail and discard
/// the new grant.
pub fn adapt_oauth_provider(provider: Arc<dyn OAuthClientProvider>) -> Arc<dyn AuthProvider> {
    Arc::new(OAuthAuthProviderAdapter {
        provider,
        in_flight: std::sync::Mutex::new(None),
    })
}

/// The recovery shared between concurrent 401s: one future, awaited by all of them.
type SharedRecovery =
    futures_util::future::Shared<Pin<Box<dyn Future<Output = Result<(), OAuthError>> + Send>>>;

struct OAuthAuthProviderAdapter {
    provider: Arc<dyn OAuthClientProvider>,
    in_flight: std::sync::Mutex<Option<SharedRecovery>>,
}

#[async_trait::async_trait]
impl AuthProvider for OAuthAuthProviderAdapter {
    async fn token(&self) -> Result<Option<String>, crate::auth_provider::AuthError> {
        let tokens = self
            .provider
            .tokens()
            .await
            .map_err(|error| crate::auth_provider::AuthError::Other(error.to_string()))?;
        Ok(tokens.map(|tokens| tokens.access_token))
    }

    async fn on_unauthorized(
        &self,
        context: UnauthorizedContext,
    ) -> Result<(), crate::auth_provider::AuthError> {
        let challenge = parse_www_authenticate(context.www_authenticate().as_deref());
        let insufficient_scope = challenge.error.as_deref() == Some("insufficient_scope");
        // A request whose token was already replaced does not need another refresh.
        if !insufficient_scope {
            let running = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some();
            if !running && context.token.is_some() {
                let current = self
                    .provider
                    .tokens()
                    .await
                    .map_err(|error| crate::auth_provider::AuthError::Other(error.to_string()))?
                    .map(|tokens| tokens.access_token);
                if let Some(current) = current
                    && Some(current) != context.token
                {
                    return Ok(());
                }
            }
        }
        // Take the running recovery, or start the one every concurrent 401 shares.
        let (recovery, started) = {
            let mut slot = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.clone() {
                Some(recovery) => (recovery, false),
                None => {
                    let provider = self.provider.clone();
                    let server_url = context.server_url.to_string();
                    let fetch = context.fetch.clone();
                    let resource_metadata_url = challenge.resource_metadata_url.clone();
                    let challenge_scope = challenge.scope.clone();
                    let recovery: SharedRecovery =
                        futures_util::FutureExt::shared(Box::pin(async move {
                            // A refresh keeps the granted scope; the challenge's scopes go on top of it.
                            let granted = if insufficient_scope {
                                provider
                                    .tokens()
                                    .await
                                    .ok()
                                    .flatten()
                                    .and_then(|tokens| tokens.scope)
                            } else {
                                None
                            };
                            let scope = if insufficient_scope {
                                step_up_scope(granted.as_deref(), challenge_scope.as_deref())
                            } else {
                                challenge_scope
                            };
                            let result = authorize_mcp(
                                provider.as_ref(),
                                &OAuthFlowOptions {
                                    server_url,
                                    resource_metadata_url,
                                    scope,
                                    fetch: Some(fetch),
                                    skip_refresh: insufficient_scope,
                                    ..OAuthFlowOptions::default()
                                },
                            )
                            .await;
                            match result {
                                Ok(OAuthFlowResult::Authorized) => Ok(()),
                                Ok(OAuthFlowResult::Redirect) => {
                                    Err(OAuthError::AuthorizationRequired)
                                }
                                Err(error) => Err(error),
                            }
                        }));
                    *slot = Some(recovery.clone());
                    (recovery, true)
                }
            }
        };
        let result = recovery.await;
        if started {
            *self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
        result.map_err(|error| crate::auth_provider::AuthError::Other(error.to_string()))
    }
}
