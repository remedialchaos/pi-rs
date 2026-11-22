//! The OAuth client that sign-in and refreshes use for one MCP server
//! (`packages/mcp/src/oauth/client-settings.ts`): its settings, how it identifies itself (a
//! pre-registered client, a Client ID Metadata Document, or dynamic client registration), and
//! the provider built from them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine;
use sha2::{Digest, Sha256};
use url::Url;

use crate::oauth::errors::OAuthError;
use crate::oauth::provider::{
    McpOAuthProvider, McpOAuthProviderOptions, McpOAuthStateStore, OnRedirect,
};
use crate::oauth::types::OAuthClientMetadataDocument;
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata,
};

/// How the client identifies itself without `clientId`. `auto` registers when the user asked for
/// a client name or the documents do not list the redirect URI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientRegistration {
    /// A Client ID Metadata Document when the authorization server supports them for public
    /// clients and the documents list the redirect URI, otherwise dynamic client registration.
    #[default]
    Auto,
    /// Always dynamic client registration.
    Dcr,
    /// Always the document; sign-in fails without support.
    Cimd,
}

impl ClientRegistration {
    fn from_setting(value: Option<&str>) -> Option<Self> {
        match value {
            None => Some(Self::Auto),
            Some("auto") => Some(Self::Auto),
            Some("dcr") => Some(Self::Dcr),
            Some("cimd") => Some(Self::Cimd),
            Some(_) => None,
        }
    }

    fn as_setting(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dcr => "dcr",
            Self::Cimd => "cimd",
        }
    }
}

/// Client settings of one MCP server, usually from the application's configuration.
#[derive(Clone, Debug, Default)]
pub struct McpOAuthSettings {
    /// Pre-registered client id. Without it, sign-in uses a Client ID Metadata Document or
    /// registers a client.
    pub client_id: Option<String>,
    /// Already resolved.
    pub client_secret: Option<String>,
    /// Port of the loopback callback server, for clients registered with a fixed redirect URI.
    /// Without `callback_url`, the redirect URI is `http://127.0.0.1:<port>/callback`. Only
    /// [`crate::oauth::loopback::listen_for_loopback_sign_in`] and refreshes use it.
    pub callback_port: Option<u16>,
    /// Loopback redirect URI registered for `client_id`, for example
    /// `http://localhost:8080/oauth/callback`. Without a port, the callback server listens on
    /// `callback_port` or a free port, which is added to the URI (RFC 8252). Default:
    /// `http://127.0.0.1/callback`.
    pub callback_url: Option<String>,
    /// Scopes to request, separated by spaces. Default: the scopes the server advertises.
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration. Default: the application's `client_name`.
    pub client_name: Option<String>,
    /// How to identify without `client_id`.
    pub client_registration: Option<ClientRegistration>,
    /// Authorization server metadata document to use instead of discovery through the server,
    /// trusted as configured.
    pub auth_server_metadata_url: Option<Url>,
}

impl McpOAuthSettings {
    fn registration(&self) -> ClientRegistration {
        self.client_registration.unwrap_or_default()
    }
}

/// The Client ID Metadata Documents an application publishes: `<baseUrl>/client.json`, and for
/// each MCP server `<baseUrl>/<callback ID>/client.json`, whose redirect URIs carry the path
/// `<path>/<callback ID>`.
pub trait McpClientMetadataDocuments: Send + Sync {
    /// For example `https://pi.dev/oauth`.
    fn base_url(&self) -> String;
    /// Whether the documents list `redirect_url`. Loopback redirect URIs match on any port
    /// (RFC 8252).
    fn lists_redirect_url(&self, redirect_url: &str) -> bool;
}

/// A store with a lock that keeps other processes from refreshing at the same time.
pub type LockFn = Arc<
    dyn Fn() -> Pin<
            Box<dyn Future<Output = Result<(), crate::oauth::provider::McpStoreError>> + Send>,
        > + Send
        + Sync,
>;

/// State store of one MCP server, with a lock that keeps other processes from refreshing at the
/// same time.
pub trait McpOAuthServerStore: McpOAuthStateStore {
    /// Run `run` while no other process refreshes the server's tokens. Not reentrant:
    /// `start_mcp_sign_in` and `create_mcp_auth_provider` take it themselves, so never call them
    /// from `run`. Fail with [`McpStoreError::RefreshLockBusy`] when another process holds the
    /// lock for the whole wait: `start_mcp_sign_in` then goes to the browser step. Any other
    /// error fails the sign-in or refresh.
    fn with_refresh_lock(
        &self,
        run: LockFn,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::oauth::provider::McpStoreError>> + Send>>;
}

/// The OAuth client of one MCP server: where its state lives, its settings, and the
/// application's identity.
#[derive(Clone)]
pub struct McpOAuthClientOptions {
    pub server_url: String,
    pub store: Arc<dyn McpOAuthServerStore>,
    pub settings: McpOAuthSettings,
    /// `client_name` for dynamic client registration unless `settings.client_name` names another:
    /// the application.
    pub client_name: String,
    /// Omitted: dynamic client registration only.
    pub client_metadata_documents: Option<Arc<dyn McpClientMetadataDocuments>>,
}

/// An [`McpOAuthProvider`] for the client, redirecting to `redirect_url` or the document's
/// variant of it.
pub fn create_oauth_client_provider(
    options: &McpOAuthClientOptions,
    redirect_url: &str,
    on_redirect: OnRedirect,
) -> McpOAuthProvider {
    let documents = options.client_metadata_documents.clone();
    let settings = options.settings.clone();
    let client_metadata = OAuthClientMetadata {
        client_name: settings
            .client_name
            .clone()
            .or_else(|| Some(options.client_name.clone())),
        ..OAuthClientMetadata::default()
    };
    McpOAuthProvider::new(McpOAuthProviderOptions {
		server_url: options.server_url.clone(),
		redirect_url: redirect_url.to_string(),
		client_metadata,
		client_metadata_document: offers_client_metadata_document(
			&options.settings,
			documents.as_deref(),
			redirect_url,
		)
		.then(|| {
			let documents = documents.clone();
			let settings = settings.clone();
			let server_url = options.server_url.clone();
			let redirect = redirect_url.to_string();
			Arc::new(move |metadata: Option<&AuthorizationServerMetadata>| {
				let documents = documents.as_deref();
				if !documents.is_some_and(|documents| documents.lists_redirect_url(&redirect)) {
					return Err(format!(
						"oauth.clientRegistration \"cimd\" needs Client ID Metadata Documents that list the redirect URI {redirect}"
					));
				}
				let document = client_metadata_document(&server_url, &redirect, metadata, documents);
				if document.is_none() && settings.registration() == ClientRegistration::Cimd {
					return Err(
						"The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\""
							.to_string(),
					);
				}
				Ok(document)
			}) as crate::oauth::provider::ClientMetadataDocumentHook
		}),
		client_id: options.settings.client_id.clone(),
		client_secret: options.settings.client_secret.clone(),
		store: Some(options.store.clone()),
		on_redirect,
	})
}

/// Whether the client may identify with a Client ID Metadata Document instead of registering.
/// `auto` registers when the user asked for a client name or the documents do not list the
/// redirect URI.
pub fn offers_client_metadata_document(
    settings: &McpOAuthSettings,
    documents: Option<&dyn McpClientMetadataDocuments>,
    redirect_url: &str,
) -> bool {
    match settings.registration() {
        ClientRegistration::Dcr => false,
        ClientRegistration::Cimd => true,
        ClientRegistration::Auto => {
            settings.client_id.is_none()
                && settings.client_name.is_none()
                && documents.is_some_and(|documents| documents.lists_redirect_url(redirect_url))
        }
    }
}

/// The Client ID Metadata Document to use, or `None` when the authorization server does not
/// support documents for public clients. Without the `iss` parameter in authorization responses
/// (RFC 9207), the redirect URI and the document are specific to the MCP server, so a response
/// cannot be mixed up with one from another authorization server (RFC 9700 section 4.4.2.2).
fn client_metadata_document(
    server_url: &str,
    redirect_url: &str,
    metadata: Option<&AuthorizationServerMetadata>,
    documents: Option<&dyn McpClientMetadataDocuments>,
) -> Option<OAuthClientMetadataDocument> {
    let metadata = metadata?;
    if !metadata
        .client_id_metadata_document_supported
        .unwrap_or(false)
    {
        return None;
    }
    if !metadata
        .token_endpoint_auth_methods()
        .iter()
        .any(|method| method == "none")
    {
        return None;
    }
    let base_url = documents
        .map(|documents| documents.base_url())
        .unwrap_or_default();
    if metadata
        .authorization_response_iss_parameter_supported
        .unwrap_or(false)
    {
        return Some(OAuthClientMetadataDocument {
            url: format!("{base_url}/client.json"),
            redirect_url: redirect_url.to_string(),
        });
    }
    let mut redirect = Url::parse(redirect_url).ok()?;
    redirect.set_path(&server_specific_path(redirect.path(), server_url));
    Some(OAuthClientMetadataDocument {
        url: format!("{base_url}/{}/client.json", callback_id(server_url)),
        redirect_url: redirect.to_string(),
    })
}

/// The path of the redirect URI of a server-specific Client ID Metadata Document:
/// `<path>/<callback ID>`.
pub fn server_specific_path(path: &str, server_url: &str) -> String {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    format!("{trimmed}/{}", callback_id(server_url))
}

/// 12 characters identifying an MCP server URL in callback paths, computed like Codex does.
fn callback_id(server_url: &str) -> String {
    let mut url = match Url::parse(server_url) {
        Ok(url) => url,
        Err(_) => return String::new(),
    };
    url.set_fragment(None);
    let href = url.to_string();
    let digest = Sha256::digest(href.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..9])
}

/// Redirect URIs a stored client was registered with.
pub fn registered_redirect_urls(client: Option<&OAuthClientInformation>) -> Vec<String> {
    match client {
        Some(client) => match client.extra.get("redirect_uris") {
            Some(serde_json::Value::Array(uris)) => uris
                .iter()
                .filter_map(|uri| uri.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    }
}

/// Bounds each request to the authorization server, so an unresponsive one cannot hold the
/// refresh lock, delay shutdown, or keep a sign-in waiting for long.
const OAUTH_REQUEST_TIMEOUT_MS: u64 = 15_000;

/// `fetch` with [`OAUTH_REQUEST_TIMEOUT_MS`] per request, on top of the request's own signal.
pub fn timed_fetch(
    fetch: Option<crate::auth_provider::McpFetch>,
) -> crate::auth_provider::McpFetch {
    let inner = fetch.unwrap_or_else(crate::auth_provider::McpFetch::platform);
    crate::auth_provider::McpFetch::from_fn(move |request| {
        let inner = inner.clone();
        Box::pin(async move {
            let mut pending = inner.call(request);
            let timeout = crate::abort::abort_timeout(OAUTH_REQUEST_TIMEOUT_MS);
            tokio::select! {
                biased;
                _ = timeout.clone().wait_aborted() => {
                    Err(crate::fetch::McpFetchError::Timeout(
                        "The operation was aborted due to timeout".to_string(),
                    ))
                }
                response = &mut pending => response,
            }
        })
    })
}

/// The callback host the loopback server listens on.
const CALLBACK_HOST: &str = "127.0.0.1";

/// Redirect URI when the settings name none and no client is stored: `127.0.0.1` on `/callback`.
pub const DEFAULT_REDIRECT_URL: &str = "http://127.0.0.1/callback";

/// Where the loopback callback server listens and the redirect URI it serves, from
/// `callback_url` and `callback_port`.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopbackSettings {
    /// The redirect URI, without the port unless one is configured.
    pub url: String,
    /// Address to listen on.
    pub host: String,
    /// Host name in the redirect URI.
    pub redirect_host: String,
    pub port: Option<u16>,
    pub path: String,
    /// The exact redirect URI, when the port is fixed.
    pub fixed_redirect_url: Option<String>,
}

/// The loopback callback settings of `settings`.
pub fn loopback_settings(settings: &McpOAuthSettings) -> Result<LoopbackSettings, String> {
    let configured = settings
        .callback_url
        .clone()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URL.to_string());
    let url = Url::parse(&configured).map_err(|error| error.to_string())?;
    let address = url
        .host_str()
        .unwrap_or(CALLBACK_HOST)
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port().or(settings.callback_port);
    let mut fixed_url = url.clone();
    let fixed_redirect_url = if url.port().is_some() {
        Some(configured.clone())
    } else if let Some(port) = port {
        let _ = fixed_url.set_port(Some(port));
        Some(fixed_url.to_string())
    } else {
        None
    };
    Ok(LoopbackSettings {
        url: configured,
        // `localhost` is served on 127.0.0.1; browsers fall back to it when ::1 refuses.
        host: if address == "localhost" {
            CALLBACK_HOST.to_string()
        } else {
            address.clone()
        },
        redirect_host: address,
        port,
        path: url.path().to_string(),
        fixed_redirect_url,
    })
}

/// Reads `clientRegistration` from a raw settings value, the way the config validator sees it.
pub fn client_registration_from_value(
    value: Option<&serde_json::Value>,
) -> Option<ClientRegistration> {
    ClientRegistration::from_setting(value.and_then(serde_json::Value::as_str))
}

/// Writes the registration mode back as the configuration's string, for diagnostics.
pub fn client_registration_as_string(registration: ClientRegistration) -> &'static str {
    registration.as_setting()
}

/// The error of building a client provider: unsupported settings, in the messages upstream
/// produces.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientSettingsError {
    #[error("{0}")]
    Message(String),
}

impl From<OAuthError> for ClientSettingsError {
    fn from(error: OAuthError) -> Self {
        Self::Message(error.to_string())
    }
}
