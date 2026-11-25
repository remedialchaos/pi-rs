//! OAuth protected-resource and authorization-server discovery
//! (`packages/mcp/src/oauth/discovery.ts`), adapted from the MCP TypeScript SDK, modified to
//! enforce authorization-server issuer validation.

use url::Url;

use crate::auth_provider::McpFetch;
use crate::fetch::{McpFetchError, McpFetchRequest, McpFetchResponse};
use crate::oauth::errors::OAuthError;
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthProtectedResourceMetadata, OAuthServerInfo,
    parse_authorization_server_metadata, parse_protected_resource_metadata,
};

/// Fetches one metadata document with the headers the servers expect.
async fn fetch_metadata(
    url: &Url,
    fetch: &McpFetch,
    protocol_version: &str,
    signal: Option<&crate::abort::AbortSignal>,
) -> Result<McpFetchResponse, McpFetchError> {
    let request = McpFetchRequest {
        url: url.to_string(),
        method: http::Method::GET,
        headers: vec![
            ("Accept".to_string(), "application/json".to_string()),
            (
                "MCP-Protocol-Version".to_string(),
                protocol_version.to_string(),
            ),
        ],
        body: None,
        signal: signal.cloned(),
    };
    fetch.call(request).await
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(pathname: &str) -> &str {
    pathname.strip_suffix('/').unwrap_or(pathname)
}

/// One field of a `WWW-Authenticate` header value: quoted or bare, case-insensitive in its name.
/// An empty value (`scope=""`) carries no information, so it counts as absent.
fn field(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let pattern = format!("{name}=");
    let mut search = 0;
    while let Some(at) = lower[search..].find(&pattern) {
        let start = search + at;
        let preceded = start == 0
            || lower[..start]
                .chars()
                .last()
                .is_some_and(|before| before == ' ' || before == ',' || before == '\t');
        if !preceded {
            search = start + pattern.len();
            continue;
        }
        let value_start = start + pattern.len();
        let rest = &header[value_start..];
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"')?;
            let value = &quoted[..end];
            return (!value.is_empty()).then(|| value.to_string());
        }
        let end = rest.find([',', ' ', '\t']).unwrap_or(rest.len());
        let value = &rest[..end];
        return (!value.is_empty()).then(|| value.to_string());
    }
    None
}

/// Parses a `WWW-Authenticate` header into a bearer or DPoP challenge; other schemes carry
/// nothing the client uses.
pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else {
        return OAuthChallenge::default();
    };
    let scheme = header.split_whitespace().next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("bearer") && !scheme.eq_ignore_ascii_case("dpop") {
        return OAuthChallenge::default();
    }
    let resource_metadata = field(header, "resource_metadata");
    let resource_metadata_url = resource_metadata
        .as_deref()
        .and_then(|url| Url::parse(url).ok());
    OAuthChallenge {
        resource_metadata_url,
        scope: field(header, "scope"),
        error: field(header, "error"),
        error_description: field(header, "error_description"),
    }
}

/// Loads the MCP server's protected-resource metadata: the authorization servers protecting it
/// and the scopes they want. With `resource_metadata_url`, that document is used as configured.
pub async fn discover_protected_resource_metadata(
    server_url: &str,
    options: DiscoveryOptions<'_>,
) -> Result<OAuthProtectedResourceMetadata, OAuthError> {
    let server = Url::parse(server_url).map_err(|error| OAuthError::Message(error.to_string()))?;
    let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
    let version = options
        .protocol_version
        .unwrap_or(crate::protocol::LATEST_PROTOCOL_VERSION);
    let mut url = match options.resource_metadata_url {
        Some(url) => url.clone(),
        None => Url::parse(&format!(
            "{}/.well-known/oauth-protected-resource{}",
            server.origin().ascii_serialization(),
            path_suffix(server.path())
        ))
        .map_err(|error| OAuthError::Message(error.to_string()))?,
    };
    let mut response = fetch_metadata(&url, &fetch, version, options.signal).await?;
    if options.resource_metadata_url.is_none()
        && server.path() != "/"
        && is_discovery_miss(response.status)
    {
        response.discard();
        url = Url::parse(&format!(
            "{}/.well-known/oauth-protected-resource",
            server.origin().ascii_serialization()
        ))
        .map_err(|error| OAuthError::Message(error.to_string()))?;
        response = fetch_metadata(&url, &fetch, version, options.signal).await?;
    }
    if !response.ok() {
        response.discard();
        return Err(OAuthError::Message(format!(
            "HTTP {} loading OAuth protected resource metadata",
            response.status
        )));
    }
    let body = read_json_body(&mut response).await?;
    parse_protected_resource_metadata(&body)
}

/// The candidate metadata URLs of an authorization server: RFC 8414 first, then OpenID Connect
/// discovery, in and below the issuer's path.
pub fn build_authorization_server_discovery_urls(
    authorization_server_url: &str,
) -> Result<Vec<(Url, DiscoveryKind)>, OAuthError> {
    let issuer = Url::parse(authorization_server_url)
        .map_err(|error| OAuthError::Message(error.to_string()))?;
    let path = path_suffix(issuer.path());
    let origin = issuer.origin().ascii_serialization();
    let mut urls = vec![
        (
            Url::parse(&format!(
                "{origin}/.well-known/oauth-authorization-server{path}"
            ))
            .map_err(|error| OAuthError::Message(error.to_string()))?,
            DiscoveryKind::Oauth,
        ),
        (
            Url::parse(&format!("{origin}/.well-known/openid-configuration{path}"))
                .map_err(|error| OAuthError::Message(error.to_string()))?,
            DiscoveryKind::Oidc,
        ),
    ];
    if !path.is_empty() {
        urls.push((
            Url::parse(&format!("{origin}{path}/.well-known/openid-configuration"))
                .map_err(|error| OAuthError::Message(error.to_string()))?,
            DiscoveryKind::Oidc,
        ));
    }
    Ok(urls)
}

/// Which kind of document a discovery URL serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryKind {
    Oauth,
    Oidc,
}

/// Options the discovery functions share.
#[derive(Clone, Default)]
pub struct DiscoveryOptions<'a> {
    pub resource_metadata_url: Option<&'a Url>,
    pub authorization_server_metadata_url: Option<&'a Url>,
    pub fetch: Option<McpFetch>,
    pub protocol_version: Option<&'a str>,
    pub skip_issuer_validation: bool,
    pub signal: Option<&'a crate::abort::AbortSignal>,
}

/// `value` without a trailing slash: URL parsing adds one to bare origins.
fn trim(value: &str) -> &str {
    value.strip_suffix('/').unwrap_or(value)
}

/// Loads an authorization server's metadata document from the first candidate URL that has one.
/// Validates the issuer against `authorization_server_url` unless skipped.
pub async fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    options: DiscoveryOptions<'_>,
) -> Result<Option<AuthorizationServerMetadata>, OAuthError> {
    let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
    for (url, _kind) in build_authorization_server_discovery_urls(authorization_server_url)? {
        let mut response = fetch_metadata(
            &url,
            &fetch,
            options
                .protocol_version
                .unwrap_or(crate::protocol::LATEST_PROTOCOL_VERSION),
            options.signal,
        )
        .await?;
        if !response.ok() {
            response.discard();
            if is_discovery_miss(response.status) {
                continue;
            }
            return Err(OAuthError::Message(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status
            )));
        }
        let body = read_json_body(&mut response).await?;
        let metadata = parse_authorization_server_metadata(&body)?;
        if !options.skip_issuer_validation {
            let expected = authorization_server_url;
            // URL parsing adds a trailing slash to bare origins, so compare without one on either
            // side.
            if trim(&metadata.issuer) != trim(expected) {
                return Err(OAuthError::IssuerMismatch {
                    expected: Some(expected.to_string()),
                    received: Some(metadata.issuer.clone()),
                });
            }
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// Discovers everything about one MCP server's authorization: its protected-resource metadata,
/// its authorization server, and that server's metadata document.
pub async fn discover_oauth_server_info(
    server_url: &str,
    options: DiscoveryOptions<'_>,
) -> Result<OAuthServerInfo, OAuthError> {
    let protected = discover_protected_resource_metadata(
        server_url,
        DiscoveryOptions {
            resource_metadata_url: options.resource_metadata_url,
            fetch: options.fetch.clone(),
            signal: options.signal,
            ..DiscoveryOptions::default()
        },
    )
    .await;
    // A network failure is rethrown, like the TypeError upstream rethrows; any other failure of
    // protected-resource discovery leaves the metadata unset.
    let resource_metadata = match protected {
        Err(OAuthError::Network(message)) => return Err(OAuthError::Network(message)),
        Err(_) => None,
        Ok(metadata) => Some(metadata),
    };

    if let Some(metadata_url) = options.authorization_server_metadata_url {
        let fetch = options.fetch.clone().unwrap_or_else(McpFetch::platform);
        let mut response = fetch_metadata(
            metadata_url,
            &fetch,
            crate::protocol::LATEST_PROTOCOL_VERSION,
            options.signal,
        )
        .await?;
        if !response.ok() {
            response.discard();
            return Err(OAuthError::Message(format!(
                "HTTP {} loading authorization server metadata from {metadata_url}",
                response.status
            )));
        }
        let body = read_json_body(&mut response).await?;
        let metadata = parse_authorization_server_metadata(&body)?;
        return Ok(OAuthServerInfo {
            // A configured document is trusted as configured, so its issuer is not checked.
            authorization_server_url: metadata.issuer.clone(),
            authorization_server_metadata: Some(metadata),
            resource_metadata,
        });
    }

    let authorization_server_url = resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref())
        .and_then(|servers| servers.first().cloned())
        .or_else(|| Some(server_root(server_url)))
        .unwrap_or_default();
    let authorization_server_metadata = discover_authorization_server_metadata(
        &authorization_server_url,
        DiscoveryOptions {
            fetch: options.fetch.clone(),
            skip_issuer_validation: options.skip_issuer_validation,
            signal: options.signal,
            ..DiscoveryOptions::default()
        },
    )
    .await?;
    Ok(OAuthServerInfo {
        authorization_server_url,
        authorization_server_metadata,
        resource_metadata,
    })
}

/// `String(new URL("/", serverUrl))`: the server's origin as a URL.
fn server_root(server_url: &str) -> String {
    Url::parse(server_url)
        .ok()
        .and_then(|url| url.join("/").ok())
        .map(|url| url.to_string())
        .unwrap_or_else(|| server_url.to_string())
}

/// The `resource` parameter of token requests: the MCP server's URL, fragment cleared.
pub fn resource_url_from_server_url(value: &str) -> Result<Url, OAuthError> {
    let mut url = Url::parse(value).map_err(|error| OAuthError::Message(error.to_string()))?;
    url.set_fragment(None);
    Ok(url)
}

/// The `resource` a token request names, once the protected-resource metadata is known. Fails when
/// the metadata protects a different server.
pub fn select_resource(
    server_url: &str,
    metadata: Option<&OAuthProtectedResourceMetadata>,
) -> Result<Option<String>, OAuthError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let requested = resource_url_from_server_url(server_url)?;
    let configured =
        Url::parse(&metadata.resource).map_err(|error| OAuthError::Message(error.to_string()))?;
    if requested.origin() != configured.origin() {
        return Err(OAuthError::Message(format!(
            "Protected resource {} does not match MCP server {}",
            metadata.resource, requested
        )));
    }
    let with_slash = |path: &str| {
        if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        }
    };
    if !with_slash(requested.path()).starts_with(&with_slash(configured.path())) {
        return Err(OAuthError::Message(format!(
            "Protected resource {} does not match MCP server {}",
            metadata.resource, requested
        )));
    }
    Ok(Some(metadata.resource.clone()))
}

/// Reads a whole response body as JSON, discarding the response.
async fn read_json_body(response: &mut McpFetchResponse) -> Result<serde_json::Value, OAuthError> {
    let (text, _) = crate::fetch::read_body_text(response, BODY_TEXT_MAX)
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    crate::protocol::parse_json(&text).map_err(OAuthError::Message)
}

/// How much of a metadata document's body is read before the rest is cancelled.
const BODY_TEXT_MAX: usize = 16 * 1024 * 1024;
