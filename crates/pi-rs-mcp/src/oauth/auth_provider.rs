//! The auth provider for connections to an OAuth-protected MCP server
//! (`packages/mcp/src/oauth/auth-provider.ts`): it sends the stored access token and refreshes
//! it, but never starts a browser flow. When the user has to sign in, it fails with
//! [`OAuthError::AuthorizationRequired`] and the application runs `start_mcp_sign_in`.

use std::sync::{Arc, Mutex};

use crate::auth_provider::{AuthError, AuthProvider, UnauthorizedContext};
use crate::oauth::client_settings::{
    DEFAULT_REDIRECT_URL, McpOAuthClientOptions, McpOAuthServerStore, McpOAuthSettings,
    loopback_settings, registered_redirect_urls, timed_fetch,
};
use crate::oauth::discovery::parse_www_authenticate;
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::{OAuthFlowOptions, OAuthFlowResult, authorize_mcp};
use crate::oauth::provider::{McpOAuthState, now_millis};
use crate::oauth::types::OAuthChallenge;

/// An [`AuthProvider`] that can say when its refreshes have finished.
#[async_trait::async_trait]
pub trait McpAuthProvider: AuthProvider {
    /// Resolves when no refresh is running, so shutdown does not drop rotated tokens before they
    /// are saved.
    async fn settled(&self);
}

/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: f64 = 30_000.0;

/// The settings of a connection's auth provider: the client options, with settings resolved only
/// when a refresh is needed, and the challenge callback.
pub struct McpAuthProviderOptions {
    pub server_url: String,
    pub store: Arc<dyn McpOAuthServerStore>,
    pub client_name: String,
    pub client_metadata_documents:
        Option<Arc<dyn crate::oauth::client_settings::McpClientMetadataDocuments>>,
    /// Called with the settings when a refresh needs them, so a secret that fails to resolve
    /// fails the refresh instead of the whole connection setup.
    pub settings: Arc<dyn Fn() -> McpOAuthSettings + Send + Sync>,
    /// Receives the server's `WWW-Authenticate` challenge so sign-in can use its resource
    /// metadata URL and scope.
    pub on_challenge: Arc<dyn Fn(&OAuthChallenge) + Send + Sync>,
    /// A redirect URI the application signs in with, such as its https callback route. Refreshing
    /// tokens from a sign-in with a Client ID Metadata Document needs a redirect URI the
    /// documents list; a refresh never redirects, so any listed one works. Default: the loopback
    /// redirect URI of the settings.
    pub redirect_url: Option<String>,
}

/// The shared refresh of a connection's provider: one at a time, awaited by `settled`.
struct RefreshState {
    running: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
}

impl RefreshState {
    fn new() -> Self {
        Self {
            running: Mutex::new(None),
        }
    }

    /// Waits until the running refresh completes, if one is running.
    async fn wait(running: Option<tokio::sync::watch::Receiver<bool>>) {
        let Some(mut done) = running else { return };
        loop {
            if *done.borrow_and_update() {
                return;
            }
            if done.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The auth provider `create_mcp_auth_provider` returns.
pub struct McpAuthProviderImpl {
    options: McpAuthProviderOptions,
    refresh: Arc<RefreshState>,
}

/// Auth provider for MCP connections: sends the stored access token and refreshes it when it is
/// about to expire or after a 401. Fails with [`OAuthError::AuthorizationRequired`] when the user
/// has to sign in, including when the server asks for more scope (`insufficient_scope`).
/// `on_challenge` receives the server's `WWW-Authenticate` challenge so sign-in can use its
/// resource metadata URL and scope. `settings` is only called when a refresh is needed, so a
/// secret that fails to resolve fails the refresh instead of the whole connection setup.
///
/// Many servers rotate refresh tokens, so two refreshes with the same refresh token lose the
/// grant. Requests in this process share one refresh, and other processes are kept out by the
/// store's refresh lock, held from reading the tokens to saving new ones. Tokens that changed
/// meanwhile (another process refreshed them, or the user signed in) are used without refreshing.
pub fn create_mcp_auth_provider(options: McpAuthProviderOptions) -> Arc<McpAuthProviderImpl> {
    Arc::new(McpAuthProviderImpl {
        options,
        refresh: Arc::new(RefreshState::new()),
    })
}

impl McpAuthProviderImpl {
    /// Replace `stale_token`, the access token that expired or was rejected. Concurrent callers
    /// share one refresh; its outcome is their answer.
    async fn refresh(
        &self,
        stale_token: Option<String>,
        fetch: Option<crate::auth_provider::McpFetch>,
        challenge: Option<OAuthChallenge>,
    ) -> Result<(), OAuthError> {
        let options = &self.options;
        let store = options.store.clone();
        let client_name = options.client_name.clone();
        let documents = options.client_metadata_documents.clone();
        let settings = (options.settings)();
        let server_url = options.server_url.clone();
        let redirect_url = options.redirect_url.clone();
        let weak_refresh = Arc::downgrade(&self.refresh);
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let existing = {
            let mut running = self
                .refresh
                .running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match running.as_ref() {
                Some(done) => Some(done.clone()),
                None => {
                    *running = Some(done_rx);
                    None
                }
            }
        };
        if let Some(done) = existing {
            // A refresh is already running; it answers this caller too.
            RefreshState::wait(Some(done)).await;
            return Ok(());
        }
        let refresh = tokio::spawn(async move {
            let result = async {
                let state = store.load().await?;
                let current = state
                    .as_ref()
                    .and_then(|state| state.tokens.as_ref())
                    .map(|tokens| tokens.access_token.clone());
                // Tokens that changed meanwhile (another process refreshed them, or the user
                // signed in) are used without refreshing.
                if current != stale_token {
                    return Ok(());
                }
                if !state
                    .as_ref()
                    .and_then(|state| state.tokens.as_ref())
                    .is_some_and(|tokens| tokens.refresh_token.is_some())
                {
                    return Err(OAuthError::AuthorizationRequired);
                }
                let state = state.unwrap_or_default();
                let client = McpOAuthClientOptions {
                    server_url,
                    store: store.clone(),
                    settings,
                    client_name,
                    client_metadata_documents: documents,
                };
                let result = refresh_stored_tokens(
                    &client,
                    &state,
                    &RefreshStoredTokensOptions {
                        fetch,
                        challenge: challenge.clone(),
                        redirect_url,
                    },
                )
                .await;
                match result {
                    Ok(OAuthFlowResult::Redirect) => Err(OAuthError::AuthorizationRequired),
                    Ok(OAuthFlowResult::Authorized) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            .await;
            if let Some(refresh) = weak_refresh.upgrade() {
                *refresh
                    .running
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            let _ = done_tx.send(true);
            result
        });
        match refresh.await {
            Ok(result) => result,
            Err(error) => Err(OAuthError::Message(error.to_string())),
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for McpAuthProviderImpl {
    async fn token(&self) -> Result<Option<String>, AuthError> {
        // A refresh that is already running settles first, so the freshest token is the answer.
        self.settled().await;
        // Failures fall through: the request goes out with the old token and a 401 decides what
        // happens.
        let state = self.options.store.load().await.ok().flatten();
        let tokens = state.as_ref().and_then(|state| state.tokens.as_ref());
        let token = tokens.map(|tokens| tokens.access_token.clone());
        let expired = state
            .as_ref()
            .and_then(|state| state.tokens_expire_at)
            .is_some_and(|expire_at| expire_at - REFRESH_SKEW_MS <= now_millis());
        if !expired || !tokens.is_some_and(|tokens| tokens.refresh_token.is_some()) {
            return Ok(token);
        }
        let _ = self.refresh(token.clone(), None, None).await;
        let state = self.options.store.load().await.ok().flatten();
        Ok(state
            .as_ref()
            .and_then(|state| state.tokens.as_ref())
            .map(|tokens| tokens.access_token.clone()))
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<(), AuthError> {
        let challenge = parse_www_authenticate(context.www_authenticate().as_deref());
        (self.options.on_challenge)(&challenge);
        // A refresh keeps the granted scope, so more scope needs a new sign-in.
        if challenge.error.as_deref() == Some("insufficient_scope") {
            return Err(AuthError::Other(
                OAuthError::AuthorizationRequired.to_string(),
            ));
        }
        let outcome = self
            .refresh(
                context.token.clone(),
                Some(context.fetch.clone()),
                Some(challenge),
            )
            .await;
        outcome.map_err(|error| AuthError::Other(error.to_string()))
    }
}

#[async_trait::async_trait]
impl McpAuthProvider for McpAuthProviderImpl {
    async fn settled(&self) {
        let running = self
            .refresh
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        RefreshState::wait(running).await;
    }
}

/// Options of [`refresh_stored_tokens`].
pub struct RefreshStoredTokensOptions {
    pub fetch: Option<crate::auth_provider::McpFetch>,
    pub challenge: Option<OAuthChallenge>,
    pub redirect_url: Option<String>,
}

/// Refresh the tokens of `state` with the stored client (or the document it signed in with), or
/// report [`OAuthFlowResult::Redirect`] when that is not possible. `options.redirect_url` is a
/// redirect URI the application signs in with, tried before the loopback ones of the settings and
/// the stored client. Never registers a client or prepares an authorization request, so a pending
/// sign-in stays usable. Call while holding the store's refresh lock. Not cancellable: a refresh
/// the server answered may have rotated the refresh token, so its answer must be saved.
pub async fn refresh_stored_tokens(
    client: &McpOAuthClientOptions,
    state: &McpOAuthState,
    options: &RefreshStoredTokensOptions,
) -> Result<OAuthFlowResult, OAuthError> {
    // Refreshing never redirects the user; the redirect URI only decides whether tokens without a
    // stored client can be refreshed with a Client ID Metadata Document, so any URI the documents
    // list works.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(redirect_url) = &options.redirect_url {
        candidates.push(redirect_url.clone());
    }
    if let Some(fixed) = loopback_settings(&client.settings)
        .map_err(OAuthError::Message)?
        .fixed_redirect_url
    {
        candidates.push(fixed);
    }
    if let Some(registered) = registered_redirect_urls(state.client_information.as_ref()).first() {
        candidates.push(registered.clone());
    }
    let redirect_url = candidates
        .iter()
        .find(|url| {
            client
                .client_metadata_documents
                .as_deref()
                .is_some_and(|documents| documents.lists_redirect_url(url))
        })
        .cloned()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URL.to_string());
    authorize_mcp(
        &crate::oauth::client_settings::create_oauth_client_provider(
            client,
            &redirect_url,
            Arc::new(|_url: &url::Url| {}),
        ),
        &OAuthFlowOptions {
            server_url: client.server_url.clone(),
            resource_metadata_url: options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.resource_metadata_url.clone()),
            authorization_server_metadata_url: client.settings.auth_server_metadata_url.clone(),
            fetch: Some(timed_fetch(options.fetch.clone())),
            refresh_only: true,
            ..OAuthFlowOptions::default()
        },
    )
    .await
}
