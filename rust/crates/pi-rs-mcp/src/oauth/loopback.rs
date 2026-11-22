//! How the authorization response of a sign-in reaches a local application
//! (`packages/mcp/src/oauth/loopback.ts`): a loopback callback server the browser redirects to,
//! or a redirect URL the user pastes from the browser address bar.

use std::sync::Arc;

use url::Url;

use crate::abort::AbortSignal;
use crate::oauth::callback::{OAuthCallback, OAuthCallbackServer, OAuthCallbackServerOptions};
use crate::oauth::client_settings::{
    McpClientMetadataDocuments, McpOAuthServerStore, McpOAuthSettings, loopback_settings,
    offers_client_metadata_document, registered_redirect_urls, server_specific_path,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::sign_in::McpSignInRedirect;

/// A loopback callback server and the redirect URI it serves.
pub struct McpLoopbackCallback {
    pub callback: Arc<OAuthCallbackServer>,
    /// Pass to `start_mcp_sign_in` and `complete_mcp_sign_in`.
    pub redirect_url: String,
}

/// Listen for the authorization response of a sign-in on loopback, where `settings` say:
/// `callback_url` and `callback_port`, else the port of the stored client's redirect URI (so
/// that client stays valid) or a free one. Pass the sign-in's `client_metadata_documents`, so
/// the callback server also serves the server-specific redirect URI. Close the callback server
/// when the sign-in ends.
pub async fn listen_for_loopback_sign_in(
    options: LoopbackSignInOptions<'_>,
) -> Result<McpLoopbackCallback, OAuthError> {
    let loopback = loopback_settings(options.settings).map_err(OAuthError::Message)?;
    let stored = options.store.load().await?;
    let registered = registered_redirect_urls(
        stored
            .as_ref()
            .and_then(|state| state.client_information.as_ref()),
    );
    let registered_port = registered
        .first()
        .and_then(|url| Url::parse(url).ok())
        .and_then(|url| url.port());
    let port = loopback.port.or(registered_port);
    let offers_document = offers_client_metadata_document(
        options.settings,
        options.client_metadata_documents.as_deref(),
        &loopback.url,
    );
    let listen_options = |port: Option<u16>| OAuthCallbackServerOptions {
        host: Some(loopback.host.clone()),
        redirect_host: Some(loopback.redirect_host.clone()),
        path: Some(loopback.path.clone()),
        // The redirect URI of a server-specific Client ID Metadata Document.
        extra_paths: if offers_document {
            vec![server_specific_path(&loopback.path, options.server_url)]
        } else {
            Vec::new()
        },
        port,
        render_page: options.render_page.clone(),
        timeout_ms: None,
    };
    let callback = match OAuthCallbackServer::listen(listen_options(port)).await {
        Ok(callback) => callback,
        Err(error) => {
            // A configured port is required; the stored client's port is only preferred.
            if loopback.port.is_some() || port.is_none() {
                return Err(OAuthError::Message(error));
            }
            OAuthCallbackServer::listen(listen_options(None))
                .await
                .map_err(OAuthError::Message)?
        }
    };
    Ok(McpLoopbackCallback {
        redirect_url: loopback
            .fixed_redirect_url
            .unwrap_or_else(|| callback.redirect_url.clone()),
        callback,
    })
}

/// Options of [`listen_for_loopback_sign_in`].
#[derive(Clone)]
pub struct LoopbackSignInOptions<'a> {
    pub server_url: &'a str,
    pub store: Arc<dyn McpOAuthServerStore>,
    pub settings: &'a McpOAuthSettings,
    pub client_metadata_documents: Option<Arc<dyn McpClientMetadataDocuments>>,
    /// Renders the browser page after the redirect. Default: plain text.
    pub render_page: Option<crate::oauth::callback::RenderPage>,
}

/// Asks the user for the redirect URL, for when the browser cannot reach the loopback callback.
/// An empty or `None` answer cancels the sign-in.
pub type PromptForRedirectUrl = Arc<
    dyn Fn(AbortSignal) -> futures_util::future::BoxFuture<'static, Result<String, OAuthError>>
        + Send
        + Sync,
>;

/// Options of [`wait_for_loopback_sign_in`].
pub struct WaitForLoopbackOptions {
    pub callback: Arc<OAuthCallbackServer>,
    /// The `redirect` result of `start_mcp_sign_in`.
    pub authorization: McpSignInRedirect,
    /// Ask for the redirect URL, for when the browser cannot reach the loopback callback (for
    /// example over SSH). The signal aborts once the callback arrives or the sign-in is
    /// cancelled. An empty answer cancels the sign-in.
    pub prompt_for_redirect_url: Option<PromptForRedirectUrl>,
    pub signal: Option<AbortSignal>,
}

/// Wait for the authorization response on a loopback callback server, or for a redirect URL the
/// user pasted from the browser address bar, whichever comes first. Aborting `signal` stops the
/// wait with [`OAuthError::SignInCancelled`].
pub async fn wait_for_loopback_sign_in(
    options: WaitForLoopbackOptions,
) -> Result<OAuthCallback, OAuthError> {
    let authorization = options.authorization;
    let controller = crate::abort::AbortController::new();
    let merged = crate::abort::abort_any({
        let mut signals = vec![controller.signal()];
        if let Some(signal) = &options.signal {
            signals.push(signal.clone());
        }
        signals
    });
    let redirect = Url::parse(&authorization.redirect_url)
        .map_err(|_| OAuthError::Message("The redirect URL does not parse".to_string()))?;
    let from_browser = options
        .callback
        .wait_for_callback(&authorization.state, Some(redirect.path()));
    let from_user: futures_util::future::BoxFuture<'static, Result<OAuthCallback, OAuthError>> =
        match options.prompt_for_redirect_url {
            Some(prompt) => {
                let authorization = authorization.clone();
                let prompt = prompt(merged.clone());
                Box::pin(async move {
                    let input = prompt.await?;
                    if input.trim().is_empty() {
                        return Err(OAuthError::SignInCancelled);
                    }
                    response_from_redirect_url(&input, &authorization)
                })
            }
            None => {
                let signal = merged.clone();
                Box::pin(async move {
                    signal.wait_aborted().await;
                    Err(OAuthError::SignInCancelled)
                })
            }
        };
    // The losing side keeps running until its own signal aborts or its server closes; failing
    // with the loss is swallowed, like the catch upstream adds.
    let outcome = {
        let from_browser = from_browser;
        let from_user = from_user;
        let cancelled = options.signal.clone();
        tokio::select! {
            biased;
            response = from_user => response,
            response = from_browser => match response {
                Ok(response) => Ok(response),
                Err(error) => Err(OAuthError::Message(error.to_string())),
            },
            _ = abort_wait(&cancelled) => Err(OAuthError::SignInCancelled),
        }
    };
    controller.abort(None);
    outcome
}

/// `None` that never resolves, so an absent outer signal never cancels anything.
async fn abort_wait(signal: &Option<AbortSignal>) {
    match signal {
        Some(signal) => {
            let _ = signal.clone().wait_aborted().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The authorization response in a redirect URL the user pasted from the browser address bar.
/// Fails when the URL is not on the sign-in's redirect URI, carries another `state`, or reports
/// an error.
pub fn response_from_redirect_url(
    input: &str,
    authorization: &McpSignInRedirect,
) -> Result<OAuthCallback, OAuthError> {
    let url = match Url::parse(input.trim()) {
        Ok(url) => url,
        Err(_) => {
            return Err(OAuthError::Message(
                "Expected the full redirect URL from the browser address bar".to_string(),
            ));
        }
    };
    let redirect_url = Url::parse(&authorization.redirect_url)
        .map_err(|_| OAuthError::Message("The redirect URL does not parse".to_string()))?;
    // A server-specific redirect URI tells authorization servers apart, so it must match exactly.
    if url.origin() != redirect_url.origin() || url.path() != redirect_url.path() {
        return Err(OAuthError::Message(
            "The redirect URL does not match this sign-in's redirect URI".to_string(),
        ));
    }
    let value = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
    };
    if let Some(error) = value("error") {
        return Err(OAuthError::Message(
            value("error_description").unwrap_or(error),
        ));
    }
    let state = value("state").unwrap_or_default();
    if state != authorization.state {
        return Err(OAuthError::Message(
            "The redirect URL belongs to a different sign-in".to_string(),
        ));
    }
    let Some(code) = value("code") else {
        return Err(OAuthError::Message(
            "The redirect URL does not contain an authorization code".to_string(),
        ));
    };
    Ok(OAuthCallback {
        code,
        state,
        iss: value("iss"),
    })
}
