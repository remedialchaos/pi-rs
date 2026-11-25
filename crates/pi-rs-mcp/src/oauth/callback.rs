//! The loopback OAuth callback server (`packages/mcp/src/oauth/callback.ts`): serves the redirect
//! URI the browser returns to, and hands the authorization response to whoever waits for it.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use axum::extract::OriginalUri;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use indexmap::IndexMap;
use tokio::sync::oneshot;

/// The authorization response as the redirect carries it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// Outcome shown on the browser page after the redirect.
#[derive(Clone, Debug, PartialEq)]
pub enum OAuthCallbackPage {
    Ok,
    Failed {
        message: String,
        details: Option<String>,
    },
}

/// Renders the browser page as HTML. Default: a plain-text message.
pub type RenderPage = Arc<dyn Fn(&OAuthCallbackPage) -> String + Send + Sync>;

/// Options of [`OAuthCallbackServer::listen`].
#[derive(Clone, Default)]
pub struct OAuthCallbackServerOptions {
    /// Address to listen on. Default: `127.0.0.1`.
    pub host: Option<String>,
    /// Host name in `redirect_url`, for example `localhost` for a client registered with it while
    /// listening on `127.0.0.1`. Default: the listen host.
    pub redirect_host: Option<String>,
    pub port: Option<u16>,
    pub path: Option<String>,
    /// More paths that receive the callback, for example a server-specific path of a redirect URI.
    pub extra_paths: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub render_page: Option<RenderPage>,
}

/// Why waiting for a callback failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CallbackWaitError {
    #[error("OAuth state is already pending")]
    AlreadyPending,
    #[error("OAuth callback timed out")]
    TimedOut,
    #[error("OAuth callback server closed")]
    ServerClosed,
    #[error("{0}")]
    Failed(String),
}

/// The default wait for a response: five minutes.
const DEFAULT_TIMEOUT_MS: u64 = 5 * 60_000;

/// How a wait ended other than with the response.
#[derive(Debug, Clone, PartialEq)]
enum CallbackFailure {
    Failed(String),
    ServerClosed,
}

struct PendingEntry {
    reply: oneshot::Sender<Result<OAuthCallback, CallbackFailure>>,
    /// The path the response must arrive on, when the waiter restricted it.
    path: Option<String>,
}

#[derive(Default)]
struct CallbackState {
    pending: Mutex<IndexMap<String, PendingEntry>>,
    closed: AtomicBool,
}

/// A loopback callback server and the redirect URI it serves.
pub struct OAuthCallbackServer {
    /// The redirect URI, with the bound port.
    pub redirect_url: String,
    state: Arc<CallbackState>,
    serve: Mutex<Option<tokio::task::JoinHandle<()>>>,
    options: OAuthCallbackServerOptions,
}

impl OAuthCallbackServer {
    /// Binds the callback server. Success pages are plain text unless `render_page` is given.
    pub async fn listen(options: OAuthCallbackServerOptions) -> Result<Arc<Self>, String> {
        let host = options
            .host
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let redirect_host = options
            .redirect_host
            .clone()
            .unwrap_or_else(|| host.clone());
        let path = options
            .path
            .clone()
            .unwrap_or_else(|| "/callback".to_string());
        let listener = tokio::net::TcpListener::bind((host.as_str(), options.port.unwrap_or(0)))
            .await
            .map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let hostname = if redirect_host.contains(':') {
            format!("[{redirect_host}]")
        } else {
            redirect_host
        };
        let redirect_url = format!("http://{hostname}:{port}{path}");

        let state = Arc::new(CallbackState::default());
        let serve_state = state.clone();
        let serve_paths = {
            let mut paths = vec![path.clone()];
            paths.extend(options.extra_paths.iter().cloned());
            paths
        };
        let serve_redirect_url = redirect_url.clone();
        let serve_render = options.render_page.clone();
        let serve = tokio::spawn(async move {
            let app = axum::Router::new().fallback(axum::routing::any(move |uri: OriginalUri| {
                let state = serve_state.clone();
                let paths = serve_paths.clone();
                let redirect_url = serve_redirect_url.clone();
                let render_page = serve_render.clone();
                async move {
                    let raw = match uri.0.query() {
                        Some(query) => format!("{}?{query}", uri.0.path()),
                        None => uri.0.path().to_string(),
                    };
                    handle_request(&state, &paths, &redirect_url, &raw, render_page.as_ref()).await
                }
            }));
            // Stopping the task stops the listener; `close` does that deliberately.
            let _ = axum::serve(listener, app).await;
        });
        Ok(Arc::new(Self {
            redirect_url,
            state,
            serve: Mutex::new(Some(serve)),
            options,
        }))
    }

    /// Wait for the authorization response with `state`. With `path`, a response on another path
    /// fails, so a server-specific redirect URI can tell authorization servers apart (RFC 9700
    /// section 4.4.2.2).
    pub async fn wait_for_callback(
        &self,
        state: &str,
        path: Option<&str>,
    ) -> Result<OAuthCallback, CallbackWaitError> {
        let (reply, reply_rx) = oneshot::channel();
        {
            let mut pending = self
                .state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.contains_key(state) {
                return Err(CallbackWaitError::AlreadyPending);
            }
            pending.insert(
                state.to_string(),
                PendingEntry {
                    reply,
                    path: path.map(str::to_string),
                },
            );
        }
        let timeout_ms = self.options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let resolved = tokio::time::timeout(Duration::from_millis(timeout_ms), reply_rx).await;
        match resolved {
            Ok(Ok(result)) => match result {
                Ok(callback) => Ok(callback),
                Err(CallbackFailure::Failed(message)) => Err(CallbackWaitError::Failed(message)),
                Err(CallbackFailure::ServerClosed) => Err(CallbackWaitError::ServerClosed),
            },
            Ok(Err(_closed)) => Err(CallbackWaitError::ServerClosed),
            Err(_elapsed) => {
                self.state
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .shift_remove(state);
                Err(CallbackWaitError::TimedOut)
            }
        }
    }

    /// Stops the server. Pending waits fail with [`CallbackWaitError::ServerClosed`]; open
    /// connections, including an unused keep-alive one a browser keeps, are dropped.
    pub async fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        let pending: Vec<PendingEntry> = {
            let mut pending = self
                .state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.drain(..).map(|(_, entry)| entry).collect()
        };
        for entry in pending {
            let _ = entry.reply.send(Err(CallbackFailure::ServerClosed));
        }
        if let Some(serve) = self
            .serve
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            serve.abort();
        }
    }
}

/// Serves one request: the same routing and replies as upstream's handler.
async fn handle_request(
    state: &Arc<CallbackState>,
    paths: &[String],
    redirect_url: &str,
    raw_url: &str,
    render_page: Option<&RenderPage>,
) -> axum::response::Response {
    if state.closed.load(Ordering::SeqCst) {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            OAuthCallbackPage::Failed {
                message: "Server closed".to_string(),
                details: None,
            },
            render_page,
        );
    }
    let url = match resolve(raw_url, redirect_url) {
        Some(url) => url,
        None => {
            return reply(
                StatusCode::NOT_FOUND,
                OAuthCallbackPage::Failed {
                    message: "Not found".to_string(),
                    details: None,
                },
                render_page,
            );
        }
    };
    let path = url.path().to_string();
    if !paths.contains(&path) {
        return reply(
            StatusCode::NOT_FOUND,
            OAuthCallbackPage::Failed {
                message: "Not found".to_string(),
                details: None,
            },
            render_page,
        );
    }
    let query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let first = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let state_value = first("state");
    let pending = match &state_value {
        Some(value) => state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .shift_remove(value),
        None => None,
    };
    let Some(entry) = pending else {
        return reply(
            StatusCode::BAD_REQUEST,
            OAuthCallbackPage::Failed {
                message: "Invalid or expired OAuth state".to_string(),
                details: None,
            },
            render_page,
        );
    };
    let wrong_path = entry
        .path
        .as_ref()
        .is_some_and(|expected| *expected != path);
    if wrong_path {
        let _ = entry.reply.send(Err(CallbackFailure::Failed(
            "The authorization response arrived on another redirect URI".to_string(),
        )));
        return reply(
            StatusCode::BAD_REQUEST,
            OAuthCallbackPage::Failed {
                message: "Unexpected redirect URI".to_string(),
                details: None,
            },
            render_page,
        );
    }
    if let Some(error) = first("error") {
        let description = first("error_description").unwrap_or_else(|| error.clone());
        let _ = entry
            .reply
            .send(Err(CallbackFailure::Failed(description.clone())));
        return reply(
            StatusCode::OK,
            OAuthCallbackPage::Failed {
                message: "Authorization failed. You may close this window.".to_string(),
                details: Some(description),
            },
            render_page,
        );
    }
    let Some(code) = first("code") else {
        let _ = entry.reply.send(Err(CallbackFailure::Failed(
            "OAuth callback did not include an authorization code".to_string(),
        )));
        return reply(
            StatusCode::BAD_REQUEST,
            OAuthCallbackPage::Failed {
                message: "Missing authorization code".to_string(),
                details: None,
            },
            render_page,
        );
    };
    let callback = OAuthCallback {
        code,
        state: state_value.clone().unwrap_or_default(),
        iss: first("iss"),
    };
    let _ = entry.reply.send(Ok(callback));
    reply(StatusCode::OK, OAuthCallbackPage::Ok, render_page)
}

/// Resolves the request target against the redirect URI, like `new URL(rawUrl, redirectUrl)`.
fn resolve(raw_url: &str, redirect_url: &str) -> Option<url::Url> {
    let base = url::Url::parse(redirect_url).ok()?;
    url::Url::options()
        .base_url(Some(&base))
        .parse(raw_url)
        .ok()
}

/// One reply, plain text by default and HTML through `render_page`, with the exact content types
/// upstream serves.
fn reply(
    status: StatusCode,
    page: OAuthCallbackPage,
    render_page: Option<&RenderPage>,
) -> axum::response::Response {
    let body = match render_page {
        Some(render) => render(&page),
        None => match &page {
            OAuthCallbackPage::Ok => {
                "Authorization complete. You may close this window.".to_string()
            }
            OAuthCallbackPage::Failed { message, details } => match details {
                Some(details) => format!("{message}\n\n{details}"),
                None => message.clone(),
            },
        },
    };
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    match render_page {
        Some(_) => {
            if let Ok(content_type) = "text/html; charset=utf-8".parse() {
                headers.insert(header::CONTENT_TYPE, content_type);
            }
            if let Ok(cache_control) = "no-store".parse() {
                headers.insert(header::CACHE_CONTROL, cache_control);
            }
        }
        None => {
            if let Ok(content_type) = "text/plain; charset=utf-8".parse() {
                headers.insert(header::CONTENT_TYPE, content_type);
            }
        }
    }
    response
}
