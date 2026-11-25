//! Native server plugins contribute through Chord's facet lifecycle.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, Weak},
};

use http::{HeaderMap, HeaderValue, Method, header};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Facet, FacetHost, Host, Root, define_facet},
    json::JsonValue,
    node::http::{
        HttpContribution, HttpError, HttpFuture, HttpRelay, HttpRequest, HttpResult, HttpWrap,
        Provenance, RelayOrigin, RouteOptions, SocketOptions, WrapPhase, text_response,
    },
    registries::{Registry, RegistryProvider, define_registry},
    store::StoreEngine,
};
use uuid::Uuid;

pub use crate::hosts::session_workers::SessionWorkerState;
use crate::{api::v1::Principal, hosts::session_workers::SessionWorkers};

use super::ServerInfo;

const NATIVES: &str = "p2.server-natives";
const RESOLVERS: &str = "p2.request-resolvers";
const AUTHENTICATORS: &str = "p2.authenticators";
const COOKIES: &str = "p2.page-cookies";

/// Services owned by the running server.
pub struct ServerNatives {
    pub info: ServerInfo,
    pub workers: SessionWorkers,
    pub plugin_root: std::path::PathBuf,
    host: Host,
    store: StoreEngine,
}

impl ServerNatives {
    pub(crate) fn new(
        info: ServerInfo,
        workers: SessionWorkers,
        host: Host,
        store: StoreEngine,
    ) -> Self {
        Self {
            plugin_root: crate::runtime::host_dirs::user_plugins_directory(std::path::Path::new(
                &info.data_dir,
            )),
            info,
            workers,
            host,
            store,
        }
    }

    /// The server's Chord host, for HTTP callbacks that hold objects or open transactions.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The root holds objects as the server principal and releases them on drop.
    pub fn root(&self) -> Root {
        self.host.root()
    }

    /// The server's own store, for backup and other coordinated reads.
    pub fn store(&self) -> &StoreEngine {
        &self.store
    }

    /// Resolve the admitted provenance and authenticated principal of a dispatched request.
    pub fn request_access(&self, request: &HttpRequest) -> HttpResult<RequestAccess> {
        let relay = verified_relay(&self.host, request)?;
        Ok(RequestAccess {
            origin: request_origin(request, self.info.port, relay.as_ref()),
            principal: request
                .principal
                .clone()
                .map(serde_json::from_value)
                .transpose()?,
        })
    }
}

#[derive(Clone, Default)]
struct NativeSlot(Weak<ServerNatives>);

impl PartialEq for NativeSlot {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

pub(crate) fn install_natives(host: &Host, natives: &Arc<ServerNatives>) -> Result<()> {
    host.page_state(NATIVES, NativeSlot::default())?
        .set(NativeSlot(Arc::downgrade(natives)), &Context::background())
}

pub fn server_of(host: &Host) -> Result<Arc<ServerNatives>> {
    host.page_state(NATIVES, NativeSlot::default())?
        .value()
        .0
        .upgrade()
        .ok_or_else(|| Error::Invalid("No server natives (the core facet contributes them)".into()))
}

struct ServiceSlot<T>(Weak<T>);

impl<T> Clone for ServiceSlot<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for ServiceSlot<T> {
    fn default() -> Self {
        Self(Weak::new())
    }
}

impl<T> PartialEq for ServiceSlot<T> {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

/// Publish a host-lifetime native service by key so peer plugins can retrieve it via `service_of`.
pub fn install_service<T: Send + Sync + 'static>(
    host: &Host,
    name: &str,
    value: &Arc<T>,
) -> Result<()> {
    host.page_state(name, ServiceSlot::<T>::default())?
        .set(ServiceSlot(Arc::downgrade(value)), &Context::background())
}

/// Retrieve a host-lifetime native service previously published via `install_service`.
pub fn service_of<T: Send + Sync + 'static>(host: &Host, name: &str) -> Result<Arc<T>> {
    host.page_state(name, ServiceSlot::<T>::default())?
        .value()
        .0
        .upgrade()
        .ok_or_else(|| Error::Invalid(format!("Native service {name} is not active")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayUser {
    pub id: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestOrigin {
    pub origin: String,
    pub remote: bool,
    pub port: u16,
    pub relay_shell_origin: Option<String>,
    pub relay_user: Option<RelayUser>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestAccess {
    pub origin: RequestOrigin,
    pub principal: Option<Principal>,
}

/// Proof from a trusted transport. Chord only invokes it for forwarded requests to
/// its loopback listener. Verification must be repeatable for the same headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRequest {
    pub origin: String,
    pub shell_origin: Option<String>,
    pub user: Option<RelayUser>,
    pub principal: Option<Principal>,
}

pub type RequestResolver =
    Arc<dyn Fn(&HeaderMap) -> HttpResult<Option<ResolvedRequest>> + Send + Sync>;
pub type Authenticator =
    Arc<dyn Fn(HttpRequest, RequestOrigin) -> HttpFuture<Option<Principal>> + Send + Sync>;
pub type PageCookie =
    Arc<dyn Fn(&HttpRequest, &RequestAccess) -> Option<HeaderValue> + Send + Sync>;

/// A server facet's HTTP and request-service contributions.
pub struct ServerServices {
    http: HttpContribution,
    pub request_resolvers: IndexMap<String, RequestResolver>,
    pub authenticators: IndexMap<String, Authenticator>,
    pub page_cookies: IndexMap<String, PageCookie>,
}

impl Default for ServerServices {
    fn default() -> Self {
        Self {
            http: HttpContribution::new("p2"),
            request_resolvers: IndexMap::new(),
            authenticators: IndexMap::new(),
            page_cookies: IndexMap::new(),
        }
    }
}

impl ServerServices {
    pub fn route(&mut self, pattern: &str, options: RouteOptions) -> HttpResult<()> {
        self.http.route(pattern, options)
    }

    /// Register an exact, authenticated WebSocket endpoint.
    pub fn upgrade(&mut self, path: &str, mut options: SocketOptions) -> HttpResult<()> {
        if path.split('/').any(|part| part.starts_with([':', '*'])) || path == "/chord" {
            return Err(HttpError::new(500, "upgrade requires an exact plugin path"));
        }
        if self.http.sockets.contains_key(path) {
            return Err(HttpError::new(500, "duplicate upgrade"));
        }
        options.public = false;
        self.http.sockets.insert(path.into(), options);
        Ok(())
    }

    pub fn facet(self) -> Facet {
        self.with_facet(|_| Ok(()))
    }

    /// Declare request services and Chord objects in a single, atomic facet.
    pub fn with_facet(
        self,
        setup: impl Fn(&mut FacetHost) -> Result<()> + Send + Sync + 'static,
    ) -> Facet {
        let callbacks = Arc::new(Callbacks {
            id: Uuid::new_v4().to_string(),
            resolvers: self.request_resolvers,
            authenticators: self.authenticators,
            cookies: self.page_cookies,
        });
        with_callbacks(|entries| entries.insert(callbacks.id.clone(), Arc::downgrade(&callbacks)));
        define_facet(move |facet| {
            setup(facet)?;
            facet.contribute_http(self.http.clone())?;
            for (name, present) in [
                (RESOLVERS, !callbacks.resolvers.is_empty()),
                (AUTHENTICATORS, !callbacks.authenticators.is_empty()),
                (COOKIES, !callbacks.cookies.is_empty()),
            ] {
                if present {
                    facet
                        .contribute(token(name)?, JsonValue::String(callbacks.id.clone().into()))?;
                }
            }
            Ok(())
        })
    }
}

// Chord's general-purpose facet registry carries JSON. The facet owns its native
// callbacks and contributes their handle so selection and reload stay atomic.
struct Callbacks {
    id: String,
    resolvers: IndexMap<String, RequestResolver>,
    authenticators: IndexMap<String, Authenticator>,
    cookies: IndexMap<String, PageCookie>,
}

impl Drop for Callbacks {
    fn drop(&mut self) {
        with_callbacks(|entries| entries.remove(&self.id));
    }
}

fn with_callbacks<T>(f: impl FnOnce(&mut HashMap<String, Weak<Callbacks>>) -> T) -> T {
    static ENTRIES: LazyLock<Mutex<HashMap<String, Weak<Callbacks>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    f(&mut ENTRIES.lock().unwrap_or_else(|error| error.into_inner()))
}

fn token(name: &str) -> Result<Registry<JsonValue, JsonValue>> {
    define_registry(name, 1)
}

fn callbacks<T: Clone>(
    host: &Host,
    name: &str,
    select: impl Fn(&Callbacks) -> &IndexMap<String, T>,
) -> HttpResult<Vec<T>> {
    let JsonValue::Array(ids) = host.registry(token(name)?).value(1)? else {
        return Err(HttpError::new(500, "invalid server service registry"));
    };
    let entries = with_callbacks(|entries| {
        ids.iter()
            .filter_map(|id| {
                let JsonValue::String(id) = id else {
                    return None;
                };
                entries.get(&id.to_utf8().ok()?)?.upgrade()
            })
            .collect::<Vec<_>>()
    });
    let mut seen = HashSet::new();
    Ok(entries
        .iter()
        .flat_map(|entry| select(entry))
        .filter(|(key, _)| seen.insert(*key))
        .map(|(_, value)| value.clone())
        .collect())
}

pub(crate) fn serve_registries(facet: &mut FacetHost) -> Result<()> {
    for name in [RESOLVERS, AUTHENTICATORS, COOKIES] {
        facet.serve_registry(
            token(name)?,
            RegistryProvider::<JsonValue, JsonValue>::new(|entries, _| {
                Ok(JsonValue::Array(
                    entries.iter().map(|entry| entry.value.clone()).collect(),
                ))
            }),
        )?;
    }
    Ok(())
}

fn resolve(host: &Host, headers: &HeaderMap) -> HttpResult<Option<ResolvedRequest>> {
    for resolver in callbacks(host, RESOLVERS, |entry| &entry.resolvers)? {
        if let Some(access) = resolver(headers)? {
            return Ok(Some(access));
        }
    }
    Ok(None)
}

fn request_origin(
    request: &HttpRequest,
    port: u16,
    relay: Option<&ResolvedRequest>,
) -> RequestOrigin {
    RequestOrigin {
        origin: request.origin.clone(),
        remote: !matches!(request.provenance, Provenance::Local),
        port,
        relay_shell_origin: relay.and_then(|access| access.shell_origin.clone()),
        relay_user: relay.and_then(|access| access.user.clone()),
    }
}

fn verified_relay(host: &Host, request: &HttpRequest) -> HttpResult<Option<ResolvedRequest>> {
    if !matches!(&request.provenance, Provenance::Relayed { relay } if relay == RESOLVERS) {
        return Ok(None);
    }
    let access = resolve(host, &request.headers)?
        .filter(|access| access.origin == request.origin)
        .ok_or_else(|| HttpError::new(403, "relay changed during admission"))?;
    Ok(Some(access))
}

async fn authenticate(
    host: Host,
    request: HttpRequest,
    port: u16,
) -> HttpResult<Option<Principal>> {
    if unverified_proxy(&request) {
        return Ok(None);
    }
    let relay = verified_relay(&host, &request)?;
    if let Some(relay) = &relay
        && (relay.principal.is_some() || relay.user.is_none())
    {
        return Ok(relay.principal.clone());
    }
    let origin = request_origin(&request, port, relay.as_ref());
    let mut first = None;
    for authenticate in callbacks(&host, AUTHENTICATORS, |entry| &entry.authenticators)? {
        if let Some(principal) = authenticate(request.clone(), origin.clone()).await? {
            if principal.is_owner() {
                return Ok(Some(principal));
            }
            first.get_or_insert(principal);
        }
    }
    Ok(first)
}

pub(crate) fn add_request_services(routes: &mut HttpContribution, natives: &Arc<ServerNatives>) {
    let mut proxy = HttpWrap::new(
        "p2.request-provenance",
        WrapPhase::Observe,
        |request, next| {
            Box::pin(async move {
                // Health handles this itself to preserve its status, MIME type and headers.
                if request.path != "/health" && unverified_proxy(&request) {
                    return text_response(403, "Localhost only");
                }
                next(request).await
            })
        },
    );
    proxy.all = true;
    routes.wraps.push(proxy);
    let weak = Arc::downgrade(natives);
    routes.relays.push(HttpRelay {
        name: RESOLVERS.into(),
        verify: Arc::new(move |headers| {
            let natives = weak
                .upgrade()
                .ok_or_else(|| HttpError::new(503, "server closed"))?;
            Ok(resolve(&natives.host, headers)?.map(|access| RelayOrigin {
                origin: access.origin,
                framer: access.shell_origin,
            }))
        }),
    });
    let weak = Arc::downgrade(natives);
    routes.authenticators.push(Arc::new(move |request| {
        let weak = weak.clone();
        Box::pin(async move {
            let Some(natives) = weak.upgrade() else {
                return Ok(None);
            };
            authenticate(natives.host.clone(), request, natives.info.port)
                .await?
                .map(serde_json::to_value)
                .transpose()
                .map_err(HttpError::from)
        })
    }));
    for path in ["/", "/new", "/s/*", "/list/*"] {
        let weak = Arc::downgrade(natives);
        let mut cookies = HttpWrap::new(
            "p2.page-cookies",
            WrapPhase::Headers,
            move |request, next| {
                let weak = weak.clone();
                Box::pin(async move {
                    let mut response = next(request.clone()).await?;
                    if let Some(natives) = weak.upgrade()
                        && matches!(request.method, Method::GET | Method::HEAD)
                        && is_app_page(&request.path)
                        && request
                            .principal
                            .as_ref()
                            .is_some_and(|value| value["kind"] == "owner")
                    {
                        let access = natives.request_access(&request)?;
                        for cookie in callbacks(&natives.host, COOKIES, |entry| &entry.cookies)? {
                            if let Some(value) = cookie(&request, &access) {
                                response.headers_mut().append(header::SET_COOKIE, value);
                            }
                        }
                    }
                    Ok(response)
                })
            },
        );
        cookies.route = Some(format!("GET {path}"));
        routes.wraps.push(cookies);
    }
}

fn unverified_proxy(request: &HttpRequest) -> bool {
    matches!(request.provenance, Provenance::Local)
        && crate::runtime::http::forwarded(&request.headers)
}

fn is_app_page(path: &str) -> bool {
    if matches!(path, "/" | "/new") {
        return true;
    }
    if let Some(key) = path.strip_prefix("/list/") {
        return !key.is_empty() && !key.contains('/');
    }
    path.strip_prefix("/s/").is_some_and(|path| {
        let session = if let Some((session, conversation)) = path.split_once("/c/") {
            if !matches!(conversation.as_bytes().first(), Some(b'1'..=b'9'))
                || !conversation.bytes().all(|byte| byte.is_ascii_digit())
            {
                return false;
            }
            session
        } else {
            path
        };
        !session.is_empty()
            && session
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'-'))
    })
}
