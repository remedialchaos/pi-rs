//! Native HTTP routing, admission, authentication, response scopes and WebSocket links.
use crate::{host::Host, link::Link, remote::RemoteOptions};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, FromRequestParts, State, ws::WebSocketUpgrade},
    response::IntoResponse,
};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use indexmap::IndexMap;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
mod cookies;
mod file;
mod registry;
mod websocket;
pub use cookies::*;
pub use file::*;
pub(crate) use registry::{
    HttpRegistry, HttpTable, RegisteredHttp, new_registry, registry_provider,
};
use tokio::sync::watch;
pub use websocket::connect_websocket;

pub type HttpResponse = Response<Body>;
pub type HttpResult<T> = std::result::Result<T, HttpError>;
pub type HttpFuture<T> = Pin<Box<dyn Future<Output = HttpResult<T>> + Send + 'static>>;
pub type Next = Arc<dyn Fn(HttpRequest) -> HttpFuture<HttpResponse> + Send + Sync>;
pub type Authenticator =
    Arc<dyn Fn(HttpRequest) -> HttpFuture<Option<serde_json::Value>> + Send + Sync>;
pub type SocketHandler =
    Arc<dyn Fn(axum::extract::ws::WebSocket, HttpRequest) -> HttpFuture<()> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
#[error("HTTP {status}: {message}")]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}
impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        Self::new(
            match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => 404,
                std::io::ErrorKind::PermissionDenied => 403,
                _ => 500,
            },
            e.to_string(),
        )
    }
}
impl From<serde_json::Error> for HttpError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(400, e.to_string())
    }
}
impl From<http::header::InvalidHeaderValue> for HttpError {
    fn from(e: http::header::InvalidHeaderValue) -> Self {
        Self::new(500, e.to_string())
    }
}
impl From<crate::Error> for HttpError {
    fn from(e: crate::Error) -> Self {
        Self::new(500, e.to_string())
    }
}
impl IntoResponse for HttpError {
    fn into_response(self) -> HttpResponse {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            if self.status >= 500 {
                "Internal Server Error".into()
            } else {
                self.message
            },
        )
            .into_response()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    Local,
    Remote { host: String },
    Relayed { relay: String },
}
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub path: String,
    pub query: String,
    pub params: IndexMap<String, String>,
    pub headers: HeaderMap,
    pub origin: String,
    pub provenance: Provenance,
    pub nonce: String,
    pub remote_address: Option<SocketAddr>,
    pub principal: Option<serde_json::Value>,
    body: Arc<tokio::sync::Mutex<Option<Body>>>,
}
impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    pub async fn body(&self, limit: usize) -> HttpResult<bytes::Bytes> {
        if self
            .header("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|n| n > limit)
        {
            return Err(HttpError::new(413, "body too large"));
        }
        let body = self
            .body
            .lock()
            .await
            .take()
            .ok_or_else(|| HttpError::new(500, "request body read twice"))?;
        to_bytes(body, limit).await.map_err(|e| {
            use std::error::Error;
            let limited = e
                .source()
                .is_some_and(|s| s.to_string().contains("length limit"));
            HttpError::new(
                if limited { 413 } else { 400 },
                "body could not be read whole",
            )
        })
    }
    pub async fn json(&self, limit: usize) -> HttpResult<serde_json::Value> {
        Ok(serde_json::from_slice(&self.body(limit).await?)?)
    }
}

#[derive(Clone)]
pub struct RouteOptions {
    pub handler: Next,
    pub public: bool,
    pub page: bool,
    pub cross_site: bool,
}
impl RouteOptions {
    pub fn new(
        handler: impl Fn(HttpRequest) -> HttpFuture<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
            public: false,
            page: false,
            cross_site: false,
        }
    }
    pub fn public(
        handler: impl Fn(HttpRequest) -> HttpFuture<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            public: true,
            ..Self::new(handler)
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WrapPhase {
    Observe,
    Headers,
    Transform,
}
type WrapHandler = Arc<dyn Fn(HttpRequest, Next) -> HttpFuture<HttpResponse> + Send + Sync>;
#[derive(Clone)]
pub struct HttpWrap {
    pub name: String,
    pub phase: WrapPhase,
    pub all: bool,
    pub route: Option<String>,
    pub handler: WrapHandler,
}
impl HttpWrap {
    pub fn new(
        name: impl Into<String>,
        phase: WrapPhase,
        handler: impl Fn(HttpRequest, Next) -> HttpFuture<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            phase,
            all: false,
            route: None,
            handler: Arc::new(handler),
        }
    }
}
#[derive(Clone)]
pub struct SocketOptions {
    pub handler: SocketHandler,
    pub public: bool,
    pub max_payload: usize,
}
#[derive(Clone)]
pub struct RelayOrigin {
    pub origin: String,
    pub framer: Option<String>,
}
type RelayVerifier = Arc<dyn Fn(&HeaderMap) -> HttpResult<Option<RelayOrigin>> + Send + Sync>;
#[derive(Clone)]
pub struct HttpRelay {
    pub name: String,
    pub verify: RelayVerifier,
}
#[derive(Clone)]
pub struct HttpContribution {
    pub plugin: String,
    routes: Vec<(Method, String, RouteOptions)>,
    pub wraps: Vec<HttpWrap>,
    pub sockets: IndexMap<String, SocketOptions>,
    pub authenticators: Vec<Authenticator>,
    pub hosts: Vec<String>,
    pub framers: Vec<String>,
    pub relays: Vec<HttpRelay>,
    pub csp: IndexMap<String, Vec<String>>,
    pub scripts: Vec<String>,
}
impl HttpContribution {
    /// Declare this contribution under the owning `PluginFacet` name.
    pub fn facet(self) -> crate::host::Facet {
        crate::host::define_facet(move |host| host.contribute_http(self.clone()))
    }
    pub fn new(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            routes: Vec::new(),
            wraps: Vec::new(),
            sockets: IndexMap::new(),
            authenticators: Vec::new(),
            hosts: Vec::new(),
            framers: Vec::new(),
            relays: Vec::new(),
            csp: IndexMap::new(),
            scripts: Vec::new(),
        }
    }
    pub fn route(&mut self, pattern: &str, options: RouteOptions) -> HttpResult<()> {
        let (method, path) = pattern
            .split_once(' ')
            .ok_or_else(|| HttpError::new(500, "route requires METHOD /pattern"))?;
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|e| HttpError::new(500, e.to_string()))?;
        if matches!(method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
            return Err(HttpError::new(500, "uncarried HTTP method"));
        }
        validate_pattern(path)?;
        if self.routes.iter().any(|(m, p, _)| m == method && p == path) {
            return Err(HttpError::new(500, "duplicate route"));
        }
        self.routes.push((method, path.into(), options));
        Ok(())
    }
    fn validate(&self) -> HttpResult<()> {
        for (directive, sources) in &self.csp {
            if directive.is_empty()
                || !directive
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-')
                || sources.iter().any(|s| {
                    s.is_empty()
                        || s.bytes().any(|b| {
                            b.is_ascii_whitespace() || b == b';' || !(32..127).contains(&b)
                        })
                })
            {
                return Err(HttpError::new(500, "invalid CSP contribution"));
            }
        }
        for (pattern, socket) in &self.sockets {
            validate_pattern(pattern)?;
            if socket.max_payload == 0 {
                return Err(HttpError::new(500, "invalid socket payload limit"));
            }
        }
        Ok(())
    }
}
pub type OpenedLink = Arc<dyn Fn(&HttpRequest) -> Option<Box<dyn FnOnce() + Send>> + Send + Sync>;
struct OpenLease(Option<Box<dyn FnOnce() + Send>>);
impl Drop for OpenLease {
    fn drop(&mut self) {
        if let Some(close) = self.0.take() {
            close();
        }
    }
}
#[derive(Clone)]
pub struct HttpOptions {
    pub links_path: Option<String>,
    pub opened: OpenedLink,
    /// Build Chord link options from the principal admitted by HTTP authentication.
    pub remote: Arc<dyn Fn(serde_json::Value) -> RemoteOptions + Send + Sync>,
    pub max_payload: usize,
    pub max_buffered: usize,
    pub close_timeout: Duration,
    pub on_error: Arc<dyn Fn(String) + Send + Sync>,
}
impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            links_path: Some("/_chord/link".into()),
            opened: Arc::new(|_| None),
            remote: Arc::new(RemoteOptions::new),
            max_payload: 1024 * 1024,
            max_buffered: 8 * 1024 * 1024,
            close_timeout: Duration::from_secs(30),
            on_error: Arc::new(|_| {}),
        }
    }
}
struct Inner {
    host: Host,
    options: HttpOptions,
    contributions: Mutex<Vec<RegisteredHttp>>,
    links: Mutex<Vec<Weak<dyn Link>>>,
}
#[derive(Clone)]
pub struct Http(Arc<Inner>);
impl Http {
    pub fn new(host: Host, options: HttpOptions) -> Self {
        Self(Arc::new(Inner {
            host,
            options,
            contributions: Mutex::new(Vec::new()),
            links: Mutex::new(Vec::new()),
        }))
    }
    pub fn contribute(&self, contribution: HttpContribution) -> HttpResult<()> {
        crate::contracts::check_name(&contribution.plugin, "HTTP plugin")?;
        let name = contribution.plugin.clone();
        let contribution = contribution.registered(&name)?;
        let mut contributions = self
            .0
            .contributions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        contributions.retain(|c| {
            if c.plugin == name {
                c.retire();
            }
            c.plugin != name
        });
        contributions.push(contribution);
        contributions.sort_by(|a, b| a.plugin.cmp(&b.plugin));
        // Closing all upgraded links on a router replacement is conservative and prevents stale grants.
        self.close_links();
        Ok(())
    }
    pub fn withdraw(&self, plugin: &str) {
        self.0
            .contributions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| {
                if c.plugin == plugin {
                    c.retire();
                }
                c.plugin != plugin
            });
        self.close_links();
    }
    pub fn close_links(&self) {
        for link in self
            .0
            .links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter_map(|l| l.upgrade())
        {
            link.close();
        }
    }
    pub async fn listen(&self, address: SocketAddr) -> HttpResult<HttpServer> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (sockets, socket_stop) = watch::channel(false);
        let state = (self.clone(), address, socket_stop);
        let app = Router::new().fallback(dispatch).with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
        });
        Ok(HttpServer {
            address,
            http: self.clone(),
            sockets,
            stop: Some(stop),
            task: Some(task),
        })
    }
    async fn answer(
        &self,
        mut request: Request<Body>,
        local: SocketAddr,
        remote: SocketAddr,
        mut socket_stop: watch::Receiver<bool>,
    ) -> HttpResponse {
        let nonce = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            rand::random::<[u8; 16]>(),
        );
        let mut contributions = self
            .0
            .contributions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(registry) = self.0.host.http_table() {
            contributions.splice(0..0, registry.0);
            contributions.sort_by(|a, b| a.plugin.cmp(&b.plugin));
        }
        let admitted = admit(request.headers(), local, &contributions);
        let mut origin = None;
        let mut framer = None;
        let result = async {
            let (provenance, own_origin, own_framer) = admitted?;
            origin = Some(own_origin.clone());
            framer = own_framer;
            if matches!(request.method().as_str(), "CONNECT" | "TRACE" | "TRACK") {
                return Err(HttpError::new(501, "method not supported"));
            }
            let path = request.uri().path().to_string();
            // Reject malformed escapes even if no route matches.
            percent_decode(&path)?;
            let is_link = self.0.options.links_path.as_deref() == Some(path.as_str());
            let socket = contributions
                .iter()
                .flat_map(|c| c.sockets.iter().map(move |(p, s)| (c, p, s)))
                .filter_map(|(c, p, s)| {
                    match_path(p, &path)
                        .transpose()
                        .map(|m| m.map(|params| (p.clone(), s.clone(), params, c.retirement())))
                })
                .collect::<HttpResult<Vec<_>>>()?
                .into_iter()
                .rev()
                .max_by_key(|(p, _, _, _)| specificity(p));
            let is_upgrade = request
                .headers()
                .get("upgrade")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| h.eq_ignore_ascii_case("websocket"));
            let matched = contributions
                .iter()
                .flat_map(|c| {
                    c.routes
                        .iter()
                        .filter(|(m, _, _)| {
                            m == request.method()
                                || request.method() == Method::HEAD && *m == Method::GET
                        })
                        .map(move |(m, p, r)| (c, m, p, r))
                })
                .filter_map(|(c, m, p, r)| {
                    match_path(p, &path)
                        .transpose()
                        .map(|found| found.map(|params| (c, m, p, r.clone(), params)))
                })
                .collect::<HttpResult<Vec<_>>>()?
                .into_iter()
                .rev()
                .max_by_key(|(_, m, p, _, _)| (*m == request.method(), specificity(p)));
            let (route_plugin, pattern, route, params) =
                if is_upgrade && (is_link || socket.is_some()) {
                    let params = socket.as_ref().map(|s| s.2.clone()).unwrap_or_default();
                    (String::new(), path.clone(), None, params)
                } else if let Some((c, method, p, r, params)) = matched {
                    (c.plugin.clone(), format!("{method} {p}"), Some(r), params)
                } else {
                    return Err(HttpError::new(404, "Not Found"));
                };
            let foreign = is_foreign(request.headers());
            let navigation = request.method() == Method::GET
                && header(request.headers(), "sec-fetch-mode") == Some("navigate")
                && matches!(
                    header(request.headers(), "sec-fetch-dest"),
                    Some("document" | "empty")
                );
            let cross_site = route.as_ref().is_some_and(|r| r.cross_site);
            if !cross_site
                && ((foreign && !(navigation && route.as_ref().is_some_and(|r| r.page)))
                    || !origin_accepted(request.method(), request.headers(), &own_origin))
            {
                return Err(HttpError::new(403, "Forbidden"));
            }
            let upgrade = if is_upgrade {
                let (mut parts, body) = request.into_parts();
                let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
                    .await
                    .map_err(|_| HttpError::new(400, "invalid WebSocket upgrade"))?;
                request = Request::from_parts(parts, body);
                Some(upgrade)
            } else {
                None
            };
            let (parts, body) = request.into_parts();
            let mut req = HttpRequest {
                method: parts.method,
                path,
                query: parts.uri.query().unwrap_or("").into(),
                params,
                headers: parts.headers,
                origin: own_origin,
                provenance,
                nonce: nonce.clone(),
                remote_address: Some(remote),
                principal: None,
                body: Arc::new(tokio::sync::Mutex::new(Some(body))),
            };
            if !cross_site && (!foreign || navigation && route.as_ref().is_some_and(|r| r.page)) {
                'auth: for contribution in &contributions {
                    for authenticate in &contribution.authenticators {
                        match authenticate(req.clone()).await {
                            Ok(Some(principal)) => {
                                req.principal = Some(principal);
                                break 'auth;
                            }
                            Ok(None) => {}
                            Err(e) => (self.0.options.on_error)(format!(
                                "authenticate {}: {e}",
                                contribution.plugin
                            )),
                        }
                    }
                }
            }
            if let Some(upgrade) = upgrade {
                if is_link {
                    let principal = req
                        .principal
                        .clone()
                        .ok_or_else(|| HttpError::new(401, "Unauthorized"))?;
                    let this = self.clone();
                    return Ok(upgrade
                        .max_message_size(self.0.options.max_payload)
                        .max_frame_size(self.0.options.max_payload)
                        .on_upgrade(move |socket| async move {
                            let (link, run) = websocket::server_link(
                                socket,
                                this.0.options.max_payload,
                                this.0.options.max_buffered,
                            );
                            this.0
                                .links
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(Arc::downgrade(&link));
                            match this
                                .0
                                .host
                                .accept(link.clone(), (this.0.options.remote)(principal))
                            {
                                Ok(peer) => {
                                    let _lease = OpenLease((this.0.options.opened)(&req));
                                    run.await;
                                    peer.close();
                                }
                                Err(e) => {
                                    (this.0.options.on_error)(e.to_string());
                                    link.close();
                                }
                            }
                        })
                        .into_response());
                }
                if let Some((_, options, _, mut retired)) = socket {
                    if !options.public && req.principal.is_none() {
                        return Err(HttpError::new(401, "Unauthorized"));
                    }
                    let report = self.0.options.on_error.clone();
                    return Ok(upgrade
                        .max_message_size(options.max_payload)
                        .max_frame_size(options.max_payload)
                        .on_upgrade(move |socket| async move {
                            tokio::select! { biased;
                                _ = retired.wait_for(|retired| *retired) => {},
                                _ = socket_stop.wait_for(|closed| *closed) => {},
                                result = async { (options.handler)(socket, req).await } => {
                                    if let Err(e) = result { report(e.to_string()); }
                                }
                            }
                        })
                        .into_response());
                }
                return Err(HttpError::new(404, "Not Found"));
            }
            if is_link && route.is_none() {
                return Err(HttpError::new(426, "Upgrade Required"));
            }
            let route = route.ok_or_else(|| HttpError::new(404, "Not Found"))?;
            if !route.public && !route.cross_site && req.principal.is_none() {
                return Err(HttpError::new(401, "Unauthorized"));
            }
            let mut wraps: Vec<_> = contributions
                .iter()
                .flat_map(|c| {
                    c.wraps
                        .iter()
                        .filter(|w| {
                            (w.all || c.plugin == route_plugin)
                                && w.route.as_ref().is_none_or(|r| r == &pattern)
                        })
                        .cloned()
                })
                .collect();
            wraps.sort_by_key(|w| w.phase);
            let mut next = route.handler;
            for wrap in wraps.into_iter().rev() {
                let inner = next;
                next = Arc::new(move |req| (wrap.handler)(req, inner.clone()));
            }
            let head = req.method == Method::HEAD;
            let mut response = next(req).await?;
            if head {
                *response.body_mut() = Body::empty();
            }
            Ok(response)
        }
        .await;
        let mut response = match result {
            Ok(response) => response,
            Err(error) => {
                if error.status >= 500 {
                    (self.0.options.on_error)(error.to_string());
                }
                error.into_response()
            }
        };
        defaults(
            response.headers_mut(),
            &contributions,
            origin.as_deref(),
            framer.as_deref(),
            &nonce,
        );
        response
    }
}
async fn dispatch(
    State((http, local, socket_stop)): State<(Http, SocketAddr, watch::Receiver<bool>)>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> HttpResponse {
    http.answer(request, local, remote, socket_stop).await
}

pub struct HttpServer {
    pub address: SocketAddr,
    http: Http,
    sockets: watch::Sender<bool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl HttpServer {
    pub async fn close(mut self) -> HttpResult<()> {
        self.sockets.send_replace(true);
        self.http.close_links();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(self.http.0.options.close_timeout, &mut task).await {
                Ok(result) => result.map_err(|e| HttpError::new(500, e.to_string()))??,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    return Err(HttpError::new(503, "HTTP close timed out"));
                }
            }
        }
        Ok(())
    }
}
impl Drop for HttpServer {
    fn drop(&mut self) {
        self.sockets.send_replace(true);
        self.http.close_links();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub fn text_response(status: u16, text: impl Into<String>) -> HttpResult<HttpResponse> {
    let status = StatusCode::from_u16(status).map_err(|e| HttpError::new(500, e.to_string()))?;
    let text = text.into();
    let mut response = HttpResponse::new(if matches!(status.as_u16(), 204 | 205 | 304) {
        Body::empty()
    } else {
        Body::from(text.clone())
    });
    *response.status_mut() = status;
    if !matches!(status.as_u16(), 204 | 205 | 304) {
        response.headers_mut().insert(
            "content-length",
            HeaderValue::from_str(&text.len().to_string())?,
        );
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
    }
    Ok(response)
}
pub fn json_response(value: &serde_json::Value) -> HttpResult<HttpResponse> {
    let mut r = text_response(200, serde_json::to_string(value)?)?;
    r.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(r)
}
pub fn html_response(body: impl Into<String>) -> HttpResult<HttpResponse> {
    let mut r = text_response(200, body)?;
    r.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(r)
}
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|h| h.to_str().ok())
}
fn admit(
    headers: &HeaderMap,
    local: SocketAddr,
    contributions: &[RegisteredHttp],
) -> HttpResult<(Provenance, String, Option<String>)> {
    if headers.get_all("host").iter().count() != 1 {
        return Err(HttpError::new(403, "Forbidden"));
    }
    let host = header(headers, "host").unwrap_or("").to_ascii_lowercase();
    if host.is_empty() || host.contains(',') {
        return Err(HttpError::new(403, "Forbidden"));
    }
    let local_host = ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| {
        host == format!("{name}:{}", local.port()) || (local.port() == 80 && host == *name)
    });
    if local_host {
        if !local.ip().is_loopback() {
            return Err(HttpError::new(403, "Forbidden"));
        }
        if !headers
            .keys()
            .any(|name| name == "forwarded" || name.as_str().starts_with("x-forwarded-"))
        {
            return Ok((Provenance::Local, format!("http://{host}"), None));
        }
        for relay in contributions.iter().flat_map(|c| &c.relays) {
            if let Some(verified) = (relay.verify)(headers)? {
                for origin in std::iter::once(&verified.origin).chain(verified.framer.iter()) {
                    let uri: http::Uri = origin
                        .parse()
                        .map_err(|_| HttpError::new(403, "bad relay origin"))?;
                    if !matches!(uri.scheme_str(), Some("http" | "https"))
                        || uri.authority().is_none()
                        || uri.path_and_query().is_some_and(|p| p.as_str() != "/")
                        || origin.ends_with('/')
                    {
                        return Err(HttpError::new(403, "bad relay origin"));
                    }
                }
                return Ok((
                    Provenance::Relayed {
                        relay: relay.name.clone(),
                    },
                    verified.origin,
                    verified.framer,
                ));
            }
        }
    } else if contributions
        .iter()
        .flat_map(|c| &c.hosts)
        .any(|h| h.to_ascii_lowercase() == host)
    {
        return Ok((
            Provenance::Remote { host: host.clone() },
            format!("https://{host}"),
            None,
        ));
    }
    Err(HttpError::new(403, "Forbidden"))
}
fn is_foreign(headers: &HeaderMap) -> bool {
    let site = header(headers, "sec-fetch-site");
    site.is_some_and(|site| !matches!(site, "same-origin" | "none"))
        || (site != Some("same-origin")
            && matches!(
                header(headers, "sec-fetch-dest"),
                Some("iframe" | "frame" | "embed" | "object" | "fencedframe")
            ))
}
fn origin_accepted(method: &Method, headers: &HeaderMap, origin: &str) -> bool {
    match header(headers, "origin") {
        Some(sent) => sent == origin,
        None => {
            matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
                || !headers.contains_key("sec-fetch-site")
        }
    }
}
fn defaults(
    headers: &mut HeaderMap,
    contributions: &[RegisteredHttp],
    origin: Option<&str>,
    framer: Option<&str>,
    nonce: &str,
) {
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    let framers: Vec<_> = contributions
        .iter()
        .flat_map(|c| c.framers.iter().map(String::as_str))
        .chain(framer)
        .collect();
    if framers.is_empty() {
        headers
            .entry("x-frame-options")
            .or_insert(HeaderValue::from_static("DENY"));
    }
    let mut directives = IndexMap::<String, Vec<String>>::from([
        ("default-src".into(), vec!["'none'".into()]),
        (
            "script-src".into(),
            vec!["'self'".into(), format!("'nonce-{nonce}'")],
        ),
        (
            "connect-src".into(),
            std::iter::once("'self'".into())
                .chain(origin.map(|o| o.replacen("http", "ws", 1)))
                .collect(),
        ),
        ("base-uri".into(), vec!["'none'".into()]),
        ("form-action".into(), vec!["'self'".into()]),
        (
            "frame-ancestors".into(),
            if framers.is_empty() {
                vec!["'none'".into()]
            } else {
                framers.iter().map(|f| (*f).into()).collect()
            },
        ),
    ]);
    let scripts: Vec<_> = contributions.iter().flat_map(|c| &c.scripts).collect();
    if let Some(origin) = origin.filter(|_| !scripts.is_empty()) {
        directives.insert(
            "script-src".into(),
            scripts
                .into_iter()
                .map(|p| format!("{origin}{p}"))
                .chain(std::iter::once(format!("'nonce-{nonce}'")))
                .collect(),
        );
    }
    for (directive, sources) in contributions.iter().flat_map(|c| &c.csp) {
        let entry = directives.entry(directive.clone()).or_default();
        if entry.first().is_some_and(|s| s == "'none'") {
            entry.clear();
        }
        entry.extend(sources.iter().cloned());
    }
    let csp = directives
        .iter()
        .map(|(k, v)| format!("{k} {}", v.join(" ")))
        .collect::<Vec<_>>()
        .join("; ");
    let value = HeaderValue::from_str(&csp).unwrap_or_else(|_| {
        HeaderValue::from_static(
            "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        )
    });
    headers.entry("content-security-policy").or_insert(value);
}
fn validate_pattern(path: &str) -> HttpResult<()> {
    if !path.starts_with('/')
        || path.contains(['?', '#'])
        || path
            .split('/')
            .enumerate()
            .any(|(i, p)| p.contains('*') && (p != "*" || i != path.split('/').count() - 1))
    {
        return Err(HttpError::new(500, "invalid route pattern"));
    }
    Ok(())
}
fn specificity(path: &str) -> Vec<u8> {
    path.split('/')
        .map(|p| {
            if p == "*" {
                0
            } else if p.starts_with(':') {
                1
            } else {
                2
            }
        })
        .chain(std::iter::once(3))
        .collect()
}
fn match_path(pattern: &str, path: &str) -> HttpResult<Option<IndexMap<String, String>>> {
    let mut params = IndexMap::new();
    let mut parts = path.split('/');
    for piece in pattern.split('/') {
        if piece == "*" {
            params.insert(
                "*".into(),
                percent_decode(&parts.collect::<Vec<_>>().join("/"))?,
            );
            return Ok(Some(params));
        }
        let Some(next) = parts.next() else {
            return Ok(None);
        };
        let next = percent_decode(next)?;
        if let Some(name) = piece.strip_prefix(':') {
            params.insert(name.into(), next);
        } else if piece != next {
            return Ok(None);
        }
    }
    Ok(parts.next().is_none().then_some(params))
}
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
pub fn percent_decode(value: &str) -> HttpResult<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = input.next().and_then(|b| char::from(b).to_digit(16));
            let lo = input.next().and_then(|b| char::from(b).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) => bytes.push((hi * 16 + lo) as u8),
                _ => return Err(HttpError::new(400, "bad path")),
            }
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| HttpError::new(400, "bad UTF-8 path"))
}
