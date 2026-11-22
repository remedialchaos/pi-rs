//! The auth provider of an HTTP transport (`packages/mcp/src/auth-provider.ts`): supplies bearer
//! tokens and may refresh them after a 401 response.

use std::sync::Arc;

use http::HeaderMap;
use url::Url;

use crate::abort::AbortSignal;
use crate::fetch::{FetchFuture, McpFetchRequest};

/// A `fetch`-like function, injectable like upstream's `McpFetch`. The platform one (reqwest) is
/// the default; applications inject their own for proxying or custom networking.
#[derive(Clone)]
pub struct McpFetch(pub Arc<dyn Fn(McpFetchRequest) -> FetchFuture + Send + Sync>);

impl McpFetch {
    /// Wraps a function or closure.
    pub fn from_fn<F>(fetch: F) -> Self
    where
        F: Fn(McpFetchRequest) -> FetchFuture + Send + Sync + 'static,
    {
        Self(Arc::new(fetch))
    }

    /// The platform `fetch`, the way omitting `options.fetch` behaves upstream.
    pub fn platform() -> Self {
        Self::from_fn(crate::fetch::platform_fetch)
    }

    pub fn call(&self, request: McpFetchRequest) -> FetchFuture {
        (self.0)(request)
    }
}

impl std::fmt::Debug for McpFetch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("McpFetch")
    }
}

/// The 401 response, or a 403 whose challenge reports `insufficient_scope`, handed to the auth
/// provider so it can refresh or step up the grant.
#[derive(Debug)]
pub struct UnauthorizedContext {
    pub status: u16,
    /// The response's headers, for the `www-authenticate` challenge.
    pub headers: HeaderMap,
    pub server_url: Url,
    pub fetch: McpFetch,
    /// Access token the rejected request carried, if any. A different current token means another
    /// request already refreshed it.
    pub token: Option<String>,
}

impl UnauthorizedContext {
    pub fn www_authenticate(&self) -> Option<String> {
        self.headers
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }
}

/// The failure a token request or an authorization recovery reports.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthError {
    /// The user has to sign in (`McpOAuthAuthorizationRequiredError` upstream).
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    #[error("{0}")]
    Other(String),
}

/// Supplies bearer tokens to an MCP HTTP transport and may refresh them after a 401 response.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    /// Token for the next request. May refresh it over the network first, so the transport calls
    /// it only before requests whose answer it waits for. Closing a session reuses the token of
    /// the last request.
    async fn token(&self) -> Result<Option<String>, AuthError>;

    /// Handles a 401 (or a step-up 403). A failure of the recovery reaches the request. The
    /// default does nothing: without a recovery the 401 reaches the caller.
    async fn on_unauthorized(&self, _context: UnauthorizedContext) -> Result<(), AuthError> {
        Ok(())
    }
}

/// A minimal auth provider with a fixed token, for callers that authorize elsewhere.
#[derive(Debug)]
pub struct StaticToken(pub Option<String>);

#[async_trait::async_trait]
impl AuthProvider for StaticToken {
    async fn token(&self) -> Result<Option<String>, AuthError> {
        Ok(self.0.clone())
    }
}

/// A request initializer the auth provider's `fetch` receives, mirroring `RequestInit`.
#[derive(Debug, Clone, Default)]
pub struct FetchInit {
    pub method: Option<http::Method>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub signal: Option<AbortSignal>,
}
