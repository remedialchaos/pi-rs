//! The default stateful OAuth provider for one exact MCP server URL
//! (`packages/mcp/src/oauth/provider.ts`). Applications inject durable storage if needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures_util::FutureExt;
use rand::RngCore;

use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata,
    OAuthClientMetadataDocument, OAuthDiscoveryState, OAuthTokens,
};

/// The stored OAuth state of one MCP server: the application's whole view of it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpOAuthState {
    #[serde(rename = "serverUrl")]
    pub server_url: String,
    #[serde(
        rename = "clientInformation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_information: Option<OAuthClientInformation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// When the access token expires, in milliseconds since the epoch, from `expires_in` at the
    /// time it was saved.
    #[serde(
        rename = "tokensExpireAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tokens_expire_at: Option<f64>,
    #[serde(
        rename = "codeVerifier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub code_verifier: Option<String>,
    #[serde(
        rename = "oauthState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub oauth_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OAuthDiscoveryState>,
}

/// Where one MCP server's OAuth state lives.
pub type LoadFuture =
    Pin<Box<dyn Future<Output = Result<Option<McpOAuthState>, McpStoreError>> + Send>>;
pub type SaveFuture = Pin<Box<dyn Future<Output = Result<(), McpStoreError>> + Send>>;

/// Loads and saves the state of one MCP server.
pub trait McpOAuthStateStore: Send + Sync {
    fn load(&self) -> LoadFuture;
    fn save(&self, state: McpOAuthState) -> SaveFuture;
}

/// Why loading or saving failed: another process holds the refresh lock for the whole wait, or
/// the storage failed for another reason.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpStoreError {
    #[error("Another process is refreshing the MCP server's tokens")]
    RefreshLockBusy,
    #[error("{0}")]
    Other(String),
}

/// An in-memory store, for tests and single-run clients.
#[derive(Default)]
pub struct MemoryOAuthStateStore {
    value: std::sync::Mutex<Option<McpOAuthState>>,
}

impl MemoryOAuthStateStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The store as the trait object the options take.
    pub fn default_store() -> Arc<dyn McpOAuthStateStore> {
        Arc::new(Self::default())
    }
}

impl McpOAuthStateStore for MemoryOAuthStateStore {
    fn load(&self) -> LoadFuture {
        let value = self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Box::pin(async move { Ok(value) })
    }

    fn save(&self, state: McpOAuthState) -> SaveFuture {
        *self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
        Box::pin(async { Ok(()) })
    }
}

/// The redirect URL as a plain string, like upstream stores it.
pub type RedirectUrl = String;

/// The callback for authorization redirects.
pub type OnRedirect = Arc<dyn Fn(&url::Url) + Send + Sync>;

/// The optional hook that supplies a Client ID Metadata Document instead of registering.
pub type ClientMetadataDocumentHook = Arc<
    dyn Fn(
            Option<&AuthorizationServerMetadata>,
        ) -> Result<Option<OAuthClientMetadataDocument>, String>
        + Send
        + Sync,
>;

/// Options of [`McpOAuthProvider`].
pub struct McpOAuthProviderOptions {
    pub server_url: String,
    pub redirect_url: String,
    /// The client metadata of registration; `redirect_uris` defaults to the redirect URL.
    pub client_metadata: OAuthClientMetadata,
    /// See [`crate::oauth::flow::OAuthClientProvider::client_metadata_document`].
    pub client_metadata_document: Option<ClientMetadataDocumentHook>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub store: Option<Arc<dyn McpOAuthStateStore>>,
    pub on_redirect: OnRedirect,
}

/// The default stateful provider for one exact MCP server URL.
pub struct McpOAuthProvider {
    redirect_url: String,
    client_metadata: OAuthClientMetadata,
    pub client_metadata_document: Option<ClientMetadataDocumentHook>,
    server_url: String,
    configured_client: Option<OAuthClientInformation>,
    store: Arc<dyn McpOAuthStateStore>,
    on_redirect: OnRedirect,
    /// Writes serialize against each other, and loads wait for the writes before them, like the
    /// promise chain upstream keeps.
    writes: Mutex<WriteChain>,
}

/// The serialized write chain: every update waits for the one before it.
type WriteChain =
    futures_util::future::Shared<Pin<Box<dyn Future<Output = Result<(), McpStoreError>> + Send>>>;

impl McpOAuthProvider {
    pub fn new(options: McpOAuthProviderOptions) -> Self {
        let server_url = url::Url::parse(&options.server_url)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| options.server_url.clone());
        let redirect_url = options.redirect_url.clone();
        let mut client_metadata = options.client_metadata;
        if client_metadata.redirect_uris.is_none() {
            client_metadata.redirect_uris = Some(vec![redirect_url.clone()]);
        }
        if client_metadata.grant_types.is_none() {
            client_metadata.grant_types = Some(vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ]);
        }
        if client_metadata.response_types.is_none() {
            client_metadata.response_types = Some(vec!["code".to_string()]);
        }
        if client_metadata.token_endpoint_auth_method.is_none() {
            client_metadata.token_endpoint_auth_method = Some(if options.client_secret.is_some() {
                "client_secret_post".to_string()
            } else {
                "none".to_string()
            });
        }
        let configured_client = options.client_id.map(|client_id| OAuthClientInformation {
            client_id,
            client_secret: options.client_secret.clone(),
            client_id_issued_at: None,
            client_secret_expires_at: None,
            extra: Default::default(),
        });
        Self {
            redirect_url,
            client_metadata,
            client_metadata_document: options.client_metadata_document,
            server_url,
            configured_client,
            store: options
                .store
                .unwrap_or_else(|| MemoryOAuthStateStore::default_store()),
            on_redirect: options.on_redirect,
            writes: Mutex::new(futures_util::FutureExt::shared(Box::pin(async { Ok(()) }))),
        }
    }

    async fn load(&self) -> Result<McpOAuthState, McpStoreError> {
        let writes = {
            let writes = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            writes.clone()
        };
        writes.await?;
        let state = self.store.load().await?;
        Ok(self.own(state))
    }

    async fn update(
        &self,
        update: impl FnOnce(McpOAuthState) -> McpOAuthState + Send + 'static,
    ) -> Result<(), McpStoreError> {
        let store = self.store.clone();
        let server_url = self.server_url.clone();
        let previous = {
            let writes = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            writes.clone()
        };
        let next: SaveFuture = Box::pin(async move {
            previous.await?;
            let state = store.load().await?;
            let owned = own_state(state, &server_url);
            store.save(update(owned)).await
        });
        let next = next.shared();
        *self
            .writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        next.await
    }

    /// Stored state for another server URL is ignored so credentials never leak across servers.
    fn own(&self, state: Option<McpOAuthState>) -> McpOAuthState {
        own_state(state, &self.server_url)
    }
}

fn own_state(state: Option<McpOAuthState>, server_url: &str) -> McpOAuthState {
    match state {
        Some(state) if state.server_url == server_url => state,
        _ => McpOAuthState {
            server_url: server_url.to_string(),
            ..McpOAuthState::default()
        },
    }
}

impl crate::oauth::flow::OAuthClientProvider for McpOAuthProvider {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn client_metadata(&self) -> OAuthClientMetadata {
        self.client_metadata.clone()
    }

    fn client_metadata_document(
        &self,
        metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<OAuthClientMetadataDocument>, String> {
        match &self.client_metadata_document {
            Some(hook) => hook(metadata),
            None => Ok(None),
        }
    }

    fn state(&self) -> crate::oauth::flow::StoreFuture<'_, Option<String>> {
        Box::pin(async move {
            let existing = self.load().await?;
            if let Some(state) = existing.oauth_state.filter(|state| !state.is_empty()) {
                return Ok(Some(state));
            }
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            let state: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            let value = state.clone();
            self.update(move |mut state| {
                state.oauth_state = Some(value);
                state
            })
            .await?;
            Ok(Some(state))
        })
    }

    fn client_information(
        &self,
    ) -> crate::oauth::flow::StoreFuture<'_, Option<OAuthClientInformation>> {
        Box::pin(async move {
            if let Some(configured) = &self.configured_client {
                return Ok(Some(configured.clone()));
            }
            Ok(self.load().await?.client_information)
        })
    }

    fn save_client_information(
        &self,
        information: OAuthClientInformation,
    ) -> crate::oauth::flow::StoreFuture<'_, ()> {
        if self.configured_client.is_some() {
            return Box::pin(async { Ok(()) });
        }
        Box::pin(async move {
            self.update(move |mut state| {
                state.client_information = Some(information);
                state
            })
            .await
        })
    }

    fn tokens(&self) -> crate::oauth::flow::StoreFuture<'_, Option<OAuthTokens>> {
        Box::pin(async move { Ok(self.load().await?.tokens) })
    }

    fn save_tokens(&self, tokens: OAuthTokens) -> crate::oauth::flow::StoreFuture<'_, ()> {
        Box::pin(async move {
            let expires_at = tokens
                .expires_in
                .map(|expires_in| now_millis() + expires_in * 1000.0);
            self.update(move |mut state| {
                state.tokens = Some(tokens);
                state.tokens_expire_at = expires_at;
                state
            })
            .await
        })
    }

    fn redirect_to_authorization(&self, url: &url::Url) -> crate::oauth::flow::StoreFuture<'_, ()> {
        let url = url.clone();
        Box::pin(async move {
            (self.on_redirect)(&url);
            Ok(())
        })
    }

    fn save_code_verifier(&self, verifier: String) -> crate::oauth::flow::StoreFuture<'_, ()> {
        Box::pin(async move {
            self.update(move |mut state| {
                state.code_verifier = Some(verifier);
                state
            })
            .await
        })
    }

    fn code_verifier(&self) -> crate::oauth::flow::StoreFuture<'_, String> {
        Box::pin(async move {
            let verifier = self.load().await?.code_verifier;
            match verifier {
                Some(verifier) if !verifier.is_empty() => Ok(verifier),
                _ => Err(McpStoreError::Other(
                    "No OAuth PKCE code verifier is stored".to_string(),
                )),
            }
        })
    }

    fn invalidate_credentials(
        &self,
        kind: crate::oauth::flow::InvalidateKind,
    ) -> crate::oauth::flow::StoreFuture<'_, ()> {
        Box::pin(async move {
            self.update(move |mut state| {
                match kind {
                    crate::oauth::flow::InvalidateKind::All
                    | crate::oauth::flow::InvalidateKind::Client => {
                        state.client_information = None;
                    }
                    _ => {}
                }
                match kind {
                    crate::oauth::flow::InvalidateKind::All
                    | crate::oauth::flow::InvalidateKind::Tokens => {
                        state.tokens = None;
                        state.tokens_expire_at = None;
                    }
                    _ => {}
                }
                if matches!(
                    kind,
                    crate::oauth::flow::InvalidateKind::All
                        | crate::oauth::flow::InvalidateKind::Verifier
                ) {
                    state.code_verifier = None;
                }
                if matches!(
                    kind,
                    crate::oauth::flow::InvalidateKind::All
                        | crate::oauth::flow::InvalidateKind::Discovery
                ) {
                    state.discovery = None;
                }
                if matches!(kind, crate::oauth::flow::InvalidateKind::All) {
                    state.oauth_state = None;
                }
                state
            })
            .await
        })
    }

    fn save_discovery_state(
        &self,
        discovery: OAuthDiscoveryState,
    ) -> crate::oauth::flow::StoreFuture<'_, ()> {
        Box::pin(async move {
            self.update(move |mut state| {
                state.discovery = Some(discovery);
                state
            })
            .await
        })
    }

    fn discovery_state(&self) -> crate::oauth::flow::StoreFuture<'_, Option<OAuthDiscoveryState>> {
        Box::pin(async move { Ok(self.load().await?.discovery) })
    }
}

/// `Date.now()`, in milliseconds.
pub(crate) fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as f64)
        .unwrap_or_default()
}
