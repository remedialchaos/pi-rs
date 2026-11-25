//! Sign-in to an OAuth-protected MCP server, in two halves so an application can complete it in
//! another request with the same state store (`packages/mcp/src/oauth/sign-in.ts`):
//! `start_mcp_sign_in` refreshes the stored tokens when it can, or else prepares the browser step
//! and returns the authorization URL; `complete_mcp_sign_in` exchanges the authorization code.

use std::future::Future;
use std::sync::Arc;

use crate::abort::AbortSignal;
use crate::oauth::auth_provider::{RefreshStoredTokensOptions, refresh_stored_tokens};
use crate::oauth::callback::OAuthCallback;
use crate::oauth::client_settings::{
    ClientRegistration, McpOAuthClientOptions, McpOAuthServerStore, McpOAuthSettings,
    offers_client_metadata_document, registered_redirect_urls, timed_fetch,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::{OAuthFlowOptions, OAuthFlowResult, authorize_mcp, step_up_scope};
use crate::oauth::provider::{McpOAuthState, McpStoreError};

/// Options of both halves of a sign-in. Pass the same values to `start_mcp_sign_in` and
/// `complete_mcp_sign_in`.
#[derive(Clone)]
pub struct McpSignInOptions {
    /// The MCP server's URL.
    pub server_url: String,
    /// The store, whose refresh lock serializes refreshes across processes.
    pub store: Arc<dyn McpOAuthServerStore>,
    pub settings: McpOAuthSettings,
    /// `client_name` for dynamic client registration unless `settings.client_name` names another:
    /// the application.
    pub client_name: String,
    /// The Client ID Metadata Documents the application publishes.
    pub client_metadata_documents:
        Option<Arc<dyn crate::oauth::client_settings::McpClientMetadataDocuments>>,
    /// Redirect URI chosen by the caller: a loopback one (`listen_for_loopback_sign_in`) or an
    /// https callback route of the application. Pass the same one to both halves: the
    /// authorization request may use a variant specific to the MCP server
    /// ([`McpSignInRedirect::redirect_url`]), which the flow derives from it.
    pub redirect_url: String,
    /// The server's challenge that asked for the sign-in; its resource metadata URL and scope
    /// apply.
    pub challenge: Option<crate::oauth::types::OAuthChallenge>,
    /// Aborting stops the sign-in at once with [`OAuthError::SignInCancelled`], also while
    /// `start_mcp_sign_in` waits for the store's refresh lock. A refresh already sent still
    /// completes and saves its tokens.
    pub signal: Option<AbortSignal>,
}

impl McpSignInOptions {
    fn client_options(&self) -> McpOAuthClientOptions {
        McpOAuthClientOptions {
            server_url: self.server_url.clone(),
            store: self.store.clone(),
            settings: self.settings.clone(),
            client_name: self.client_name.clone(),
            client_metadata_documents: self.client_metadata_documents.clone(),
        }
    }
}

/// The browser step of a sign-in, from `start_mcp_sign_in`. Plain data, so an application can
/// persist it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpSignInRedirect {
    /// The `kind` marker upstream's plain data carries.
    pub kind: String,
    /// Open in the user's browser.
    pub authorization_url: String,
    /// The redirect URI of the authorization request; the authorization response must arrive on
    /// it.
    pub redirect_url: String,
    /// The `state` parameter the authorization response must carry.
    pub state: String,
    /// The scope requested, if any. Saved with the tokens when the token response names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Result of [`start_mcp_sign_in`]: signed in by refreshing the stored tokens, or the browser
/// step.
#[derive(Clone, Debug, PartialEq)]
pub enum McpSignInStart {
    Authorized,
    Redirect(McpSignInRedirect),
}

/// First half of a sign-in. Without a step-up (`insufficient_scope`), it first tries to refresh
/// the stored tokens with the stored client, whatever `redirect_url` is. The refresh takes the
/// store's refresh lock, so never call this while holding it; when another process holds the
/// lock for the whole wait, the refresh is skipped, and any other failure of the lock fails the
/// sign-in. Otherwise it prepares the browser step: a fresh `state`, and a stored client that
/// cannot use `redirect_url` is dropped with its tokens. Requests to the authorization server
/// are time-limited.
pub async fn start_mcp_sign_in(options: McpSignInOptions) -> Result<McpSignInStart, OAuthError> {
    if options.signal.as_ref().is_some_and(AbortSignal::aborted) {
        return Err(OAuthError::SignInCancelled);
    }
    // A refresh keeps the granted scope; a server asking for more needs the browser step.
    let step_up = options
        .challenge
        .as_ref()
        .is_some_and(|challenge| challenge.error.as_deref() == Some("insufficient_scope"));
    if !step_up && refresh_before_sign_in(&options).await? == OAuthFlowResult::Authorized {
        return Ok(McpSignInStart::Authorized);
    }
    let signal = options.signal.clone();
    let for_browser_step = options.clone();
    let started = move || async move { start_browser_step(&for_browser_step, step_up).await };
    cancellable(signal.as_ref(), started)
        .await
        .map(McpSignInStart::Redirect)
}

/// Refresh the stored tokens, unless `cimd` is to replace a registered client. The refresh uses
/// `redirect_url` when the tokens came from a Client ID Metadata Document whose documents list
/// it. Runs under the store's refresh lock.
async fn refresh_before_sign_in(options: &McpSignInOptions) -> Result<OAuthFlowResult, OAuthError> {
    let store = options.store.clone();
    let client = options.client_options();
    let challenge = options.challenge.clone();
    let redirect_url = options.redirect_url.clone();
    let signal = options.signal.clone();
    let outcome_cell: Arc<std::sync::Mutex<Option<OAuthFlowResult>>> =
        Arc::new(std::sync::Mutex::new(None));
    let outcome_cell_for_lock = outcome_cell.clone();
    let lock = store.with_refresh_lock(Arc::new(move || {
        let client = client.clone();
        let challenge = challenge.clone();
        let redirect_url = redirect_url.clone();
        let signal = signal.clone();
        let outcome_cell = outcome_cell_for_lock.clone();
        Box::pin(async move {
            // The lock may arrive after the sign-in was cancelled: return at once, so it is
            // released. Nothing runs under it, and a pending sign-in stays usable.
            if signal.as_ref().is_some_and(AbortSignal::aborted) {
                *outcome_cell
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(OAuthFlowResult::Redirect);
                return Ok(());
            }
            let state = client.store.load().await?;
            let Some(state) = state else {
                *outcome_cell
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(OAuthFlowResult::Redirect);
                return Ok(());
            };
            let has_refresh_token = state
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.refresh_token.clone())
                .is_some();
            if !has_refresh_token {
                *outcome_cell
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(OAuthFlowResult::Redirect);
                return Ok(());
            }
            let cimd_replaces = client.settings.client_registration
                == Some(ClientRegistration::Cimd)
                && state.client_information.is_some()
                && client.settings.client_id.is_none();
            if cimd_replaces {
                *outcome_cell
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(OAuthFlowResult::Redirect);
                return Ok(());
            }
            let outcome = refresh_stored_tokens(
                &client,
                &state,
                &RefreshStoredTokensOptions {
                    challenge: challenge.clone(),
                    redirect_url: Some(redirect_url.clone()),
                    fetch: None,
                },
            )
            .await;
            match outcome {
                Ok(result_value) => {
                    *outcome_cell
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result_value);
                    Ok(())
                }
                Err(error) => Err(McpStoreError::Other(error.to_string())),
            }
        })
    }));
    // The lock future keeps running when the sign-in is cancelled: it releases the lock, and a
    // refresh the server answered must save its tokens.
    let outcome = until_aborted(options.signal.as_ref(), lock).await?;
    match outcome {
        // Another process is refreshing and took too long. The user signs in instead.
        Err(OAuthError::RefreshLockBusy) => Ok(OAuthFlowResult::Redirect),
        Err(error) => Err(error),
        Ok(()) => {
            let flow = outcome_cell
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match flow {
                Some(OAuthFlowResult::Authorized) => Ok(OAuthFlowResult::Authorized),
                _ => Ok(OAuthFlowResult::Redirect),
            }
        }
    }
}

/// `pending`, or [`OAuthError::SignInCancelled`] as soon as `signal` aborts. `pending` keeps
/// running detached, like the promise upstream leaves behind.
async fn until_aborted(
    signal: Option<&AbortSignal>,
    pending: impl Future<Output = Result<(), McpStoreError>> + Send + 'static,
) -> Result<Result<(), OAuthError>, OAuthError> {
    let task = tokio::spawn(pending);
    match signal {
        None => Ok(task
            .await
            .map_err(|error| OAuthError::Message(error.to_string()))?
            .map_err(OAuthError::from)),
        Some(signal) => {
            if signal.aborted() {
                return Err(OAuthError::SignInCancelled);
            }
            tokio::select! {
                biased;
                _ = signal.clone().wait_aborted() => Err(OAuthError::SignInCancelled),
                outcome = task => Ok(outcome
                    .map_err(|error| OAuthError::Message(error.to_string()))
                    .and_then(|outcome| outcome.map_err(OAuthError::from))),
            }
        }
    }
}

/// The browser step: a fresh authorization request, registered with the caller's redirect URI.
async fn start_browser_step(
    options: &McpSignInOptions,
    step_up: bool,
) -> Result<McpSignInRedirect, OAuthError> {
    let stored = options.store.load().await?;
    if let Some(stored) = &stored {
        options
            .store
            .save(state_for_sign_in(stored.clone(), options))
            .await?;
    }
    let authorization_url: Arc<std::sync::Mutex<Option<url::Url>>> =
        Arc::new(std::sync::Mutex::new(None));
    let provider = crate::oauth::client_settings::create_oauth_client_provider(
        &options.client_options(),
        &options.redirect_url,
        {
            let authorization_url = authorization_url.clone();
            Arc::new(move |url: &url::Url| {
                *authorization_url
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(url.clone());
            })
        },
    );
    // A server asking for more scope gets it on top of the configured scope and, since the
    // challenge may list only the missing scopes, on top of the scope granted so far.
    let granted = stored
        .as_ref()
        .and_then(|state| state.tokens.as_ref())
        .and_then(|tokens| tokens.scope.clone());
    let step_scopes = if step_up {
        step_up_scope(
            granted.as_deref(),
            options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.scope.as_deref()),
        )
    } else {
        options
            .challenge
            .as_ref()
            .and_then(|challenge| challenge.scope.clone())
    };
    let scope = merge_scopes(&[options.settings.scope.clone(), step_scopes]);
    let flow = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: options.server_url.clone(),
            scope,
            skip_refresh: true,
            resource_metadata_url: options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.resource_metadata_url.clone()),
            fetch: Some(timed_fetch(None)),
            signal: options.signal.clone(),
            authorization_server_metadata_url: options.settings.auth_server_metadata_url.clone(),
            ..OAuthFlowOptions::default()
        },
    )
    .await;
    match flow {
        Ok(OAuthFlowResult::Redirect) => {}
        Ok(OAuthFlowResult::Authorized) => {
            return Err(OAuthError::Message(
                "OAuth flow refreshed instead of redirecting".to_string(),
            ));
        }
        Err(error) => return Err(error),
    }
    let url = authorization_url
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(url) = url else {
        return Err(OAuthError::Message(
            "OAuth flow did not produce an authorization URL".to_string(),
        ));
    };
    let value = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
    };
    Ok(McpSignInRedirect {
        kind: "redirect".to_string(),
        authorization_url: url.to_string(),
        // The flow picks the redirect URI, which may be specific to the MCP server.
        redirect_url: value("redirect_uri").unwrap_or_else(|| options.redirect_url.clone()),
        state: value("state").unwrap_or_default(),
        scope: value("scope"),
    })
}

/// The stored state for a browser step: without the last sign-in's `state`, and without a client
/// that cannot use `redirect_url`, along with its tokens.
fn state_for_sign_in(stored: McpOAuthState, options: &McpSignInOptions) -> McpOAuthState {
    let mut next = stored;
    next.oauth_state = None;
    // A registered client cannot use another redirect URI, and its tokens belong to it. `cimd`
    // replaces a registered client; `auto` keeps it. A Client ID Metadata Document is not stored,
    // so tokens without a stored client belong to the document.
    let keep_client = options.settings.client_id.is_some()
        || match next.client_information.as_ref() {
            Some(_) => {
                options.settings.client_registration != Some(ClientRegistration::Cimd)
                    && registered_redirect_urls(next.client_information.as_ref())
                        .contains(&options.redirect_url)
            }
            None => offers_client_metadata_document(
                &options.settings,
                options.client_metadata_documents.as_deref(),
                &options.redirect_url,
            ),
        };
    if !keep_client {
        next.client_information = None;
        next.tokens = None;
        next.tokens_expire_at = None;
    }
    next
}

/// Scopes of both lists, each once.
fn merge_scopes(scopes: &[Option<String>]) -> Option<String> {
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes.iter().flatten() {
        for part in scope.split_whitespace() {
            if !part.is_empty() && !merged.contains(&part) {
                merged.push(part);
            }
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

/// The second half of a sign-in: exchanges the authorization code of `response` and saves the
/// tokens. Pass the options given to `start_mcp_sign_in`, including its `redirect_url` (not the
/// redirect's `redirect_url`). Fails when the response carries another `state`, when another
/// sign-in started with the store since, or when the exchange would use another redirect URI
/// than the authorization request (the options changed). A redirect URL the user pasted goes
/// through `response_from_redirect_url` first, which checks its path.
pub async fn complete_mcp_sign_in(
    options: McpSignInOptions,
    authorization: McpSignInRedirect,
    response: OAuthCallback,
) -> Result<(), OAuthError> {
    let signal = options.signal.clone();
    let started = async move {
        if response.state != authorization.state {
            return Err(OAuthError::Message(
                "The authorization response belongs to a different sign-in".to_string(),
            ));
        }
        // The stored PKCE verifier belongs to the latest sign-in; exchanging with it would fail
        // with `invalid_grant`, which discards the stored tokens.
        let stored = options.store.load().await?;
        let current = stored.as_ref().and_then(|state| state.oauth_state.clone());
        if current.is_none_or(|state| state != authorization.state) {
            return Err(OAuthError::Message(
                "Another sign-in to this MCP server started since; sign in again".to_string(),
            ));
        }
        let provider = crate::oauth::client_settings::create_oauth_client_provider(
            &options.client_options(),
            &options.redirect_url,
            Arc::new(|_url: &url::Url| {}),
        );
        authorize_mcp(
            &provider,
            &OAuthFlowOptions {
                server_url: options.server_url.clone(),
                // Saved with the tokens when the token response names no scope, so it must be the
                // requested one.
                scope: authorization.scope.clone(),
                resource_metadata_url: options
                    .challenge
                    .as_ref()
                    .and_then(|challenge| challenge.resource_metadata_url.clone()),
                authorization_server_metadata_url: options
                    .settings
                    .auth_server_metadata_url
                    .clone(),
                fetch: Some(timed_fetch(None)),
                signal: options.signal.clone(),
                authorization_redirect_url: Some(authorization.redirect_url.clone()),
                authorization_code: Some(response.code.clone()),
                iss: response.iss.clone(),
                ..OAuthFlowOptions::default()
            },
        )
        .await
        .map(|_| ())
    };
    cancellable(signal.as_ref(), move || started).await
}

/// Run `run`, reporting an abort of `signal` as [`OAuthError::SignInCancelled`]. The signal also
/// reaches `run`'s requests, so dropping it on abort cancels them, like upstream.
async fn cancellable<T, MakeFuture, F>(
    signal: Option<&AbortSignal>,
    make: MakeFuture,
) -> Result<T, OAuthError>
where
    T: Send + 'static,
    MakeFuture: FnOnce() -> F,
    F: Future<Output = Result<T, OAuthError>> + Send + 'static,
{
    if signal.is_some_and(AbortSignal::aborted) {
        return Err(OAuthError::SignInCancelled);
    }
    let mut run = Box::pin(make());
    match signal {
        None => run.await,
        Some(signal) => {
            tokio::select! {
                biased;
                _ = signal.clone().wait_aborted() => Err(OAuthError::SignInCancelled),
                outcome = &mut run => outcome,
            }
        }
    }
}
