//! The p3 server's bootstrap: everything that must exist before plugins load, and nothing else. It
//! takes the data directory's lock, creates the host, Chord's HTTP component (the `/_p3` routes and
//! the `/chord` link endpoint), registers the route to session workers (through [`SessionRouter`]),
//! loads the server facets, listens, and writes `running.json` (pid, port). Everything else is a
//! plugin.
//!
//! The route to a session's worker is the local seam for `pi-rs-chord`'s missing address-level
//! routing (`2.1b-3`): the router asks the sessions' rows whether the session exists, refuses another
//! session's worker, and calls the [`WorkerRunner`] the runner plugin contributed.

use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Caller, Host, HostOptions, PluginFacet, create_host},
    json::JsonValue,
    link::Link,
    node::http::{Http, HttpOptions, HttpServer},
    node::workers::ProcessLock,
    remote::{LinkRole, RemoteOptions, RemotePeer, RemoteReference},
};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    api::v1::{server_chord_address, session_host_kind},
    files::{open_data_dir, private_directory},
    install::{WorkerSpec, plugins_dir},
    plugins::{control, runner_server, sessions, sign_in},
    principals::Principal,
    records::{ServerRecord, remove_record, server_lock, write_record},
    registry::ServerRegistries,
    runner::{
        LocalRunner, PluginScript, ProcessLauncher, RolloutHost, RolloutResult, RolloutStep,
        RunnerFuture, RunnerOptions, WorkerEnvironment, WorkerLink, WorkerRunner,
    },
    server_core,
};

/// What the server's start takes.
#[derive(Clone)]
pub struct ServerOptions {
    pub data_dir: PathBuf,
    /// Where it listens: `127.0.0.1` and this port (0: a free one).
    pub port: u16,
    /// How a session's worker process starts.
    pub worker: WorkerSpec,
    /// How long a worker idles before it stops itself: 5 minutes by default.
    pub worker_idle_ms: u64,
    /// p3's version, for `running.json`.
    pub version: String,
    /// What the owner's stop does (the command's close).
    pub on_stop: Arc<dyn Fn() + Send + Sync>,
    /// What the server reports in the background.
    pub on_error: Arc<dyn Fn(ServerError) + Send + Sync>,
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("data_dir", &self.data_dir)
            .field("port", &self.port)
            .field("worker", &self.worker)
            .field("worker_idle_ms", &self.worker_idle_ms)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// What the server reports in the background.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("p3: {0}")]
    Message(String),
    #[error("p3: host: {0}")]
    Host(String),
    #[error("p3: http: {0}")]
    Http(String),
    #[error("p3: plugins: {0}")]
    Plugins(String),
    #[error(transparent)]
    Files(#[from] crate::files::FileError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How long a start waits for the data directory's lock before it is refused.
const LOCK_MS: u64 = 1000;

/// How long a worker idles before it stops itself, by default.
pub const IDLE_MS: u64 = 5 * 60_000;

/// A route operation's future.
pub type RouteFuture<T> = Pin<Box<dyn Future<Output = Result<T, RouteError>> + Send + 'static>>;

struct CachedWorkerPeer {
    link: Arc<dyn Link>,
    peer: RemotePeer,
}

/// The route to session workers: whether a session exists, who may reach it, which runner starts
/// its worker, and address-level Chord routing onto the worker's multiplexed IPC link.
pub struct SessionRouter {
    sessions: Arc<sessions::SessionsStore>,
    runner: OnceLock<Arc<dyn WorkerRunner>>,
    host: OnceLock<Host>,
    contracts: BTreeMap<String, Arc<Contract>>,
    peers: Mutex<BTreeMap<String, CachedWorkerPeer>>,
    slots: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
}

impl SessionRouter {
    pub fn new(sessions: Arc<sessions::SessionsStore>) -> Arc<Self> {
        let mut contracts = BTreeMap::new();
        for build in [
            sessions::session_worker_contract,
            sessions::session_start_contract,
        ] {
            if let Ok(contract) = build() {
                contracts.insert(contract.name().to_string(), contract);
            }
        }
        Arc::new(Self {
            sessions,
            runner: OnceLock::new(),
            host: OnceLock::new(),
            contracts,
            peers: Mutex::new(BTreeMap::new()),
            slots: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn set_runner(&self, runner: Arc<dyn WorkerRunner>) {
        let _ = self.runner.set(runner);
    }

    pub fn set_host(&self, host: Host) {
        let _ = self.host.set(host);
    }

    pub fn close_peers(&self) {
        let peers = std::mem::take(
            &mut *self
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, cached) in peers {
            cached.peer.close();
            cached.link.close();
        }
    }

    pub fn exists(&self, session: &str) -> bool {
        self.sessions.one(session).is_some()
    }

    /// Admission: a session's worker is reached only while the session exists, and never by another
    /// session's worker.
    pub fn admit(&self, principal: &Principal, session: &str) -> sessions::Admission {
        sessions::admit_session(principal, self.exists(session))
    }

    fn slot(&self, session: &str) -> Arc<AsyncMutex<()>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Builds Chord `RemoteOptions` that route `session`-scoped holds to the supervised worker
    /// process while enforcing principal authorization and session ownership.
    pub fn remote_options(self: &Arc<Self>, principal: serde_json::Value) -> RemoteOptions {
        let mut options = RemoteOptions::new(principal);
        options.route_contracts.clone_from(&self.contracts);
        let weak = Arc::downgrade(self);
        options.route = Some(Arc::new(move |request| {
            let router = weak.upgrade();
            Box::pin(async move {
                if request.address_kind == "server" {
                    return Ok(None);
                }
                let router = router.ok_or(pi_rs_chord::Error::Closed)?;
                let contract = router
                    .contracts
                    .get(request.contract.name())
                    .cloned()
                    .ok_or_else(|| {
                        ChordError::new("unavailable", "type not served at worker kind")
                    })?;
                let address = contract.host_address(&request.params)?;
                if request.address_kind != address.kind || request.address_params != address.params
                {
                    return Err(ChordError::invalid(
                        "address",
                        "does not match the contract's destination",
                        None,
                    )
                    .into());
                }
                let kind = session_host_kind()?;
                if kind
                    .key_of_address(&JsonValue::Object(indexmap::IndexMap::from([
                        (
                            "kind".into(),
                            JsonValue::String(address.kind.as_str().into()),
                        ),
                        ("params".into(), address.params.clone()),
                    ])))?
                    .as_deref()
                    != Some(address.key.as_str())
                {
                    return Err(
                        ChordError::invalid("address", "invalid worker address", None).into(),
                    );
                }
                let session_id = address
                    .params
                    .get("id")
                    .and_then(|v| v.to_serde().ok())
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .ok_or_else(|| ChordError::invalid("address", "missing session id", None))?;
                let caller_principal = request
                    .caller
                    .principal
                    .as_ref()
                    .and_then(Principal::from_value)
                    .ok_or_else(|| ChordError::new("forbidden", "worker requires a principal"))?;
                if let sessions::Admission::Deny(reason) =
                    router.admit(&caller_principal, &session_id)
                {
                    return Err(ChordError::new("forbidden", reason).into());
                }
                if contract.name() == sessions::SESSION_START_CONTRACT {
                    if caller_principal != Principal::Server {
                        return Err(ChordError::new(
                            "forbidden",
                            "sessions: the server's waker only",
                        )
                        .into());
                    }
                } else if !(caller_principal.is_owner() || caller_principal == Principal::Server) {
                    return Err(ChordError::new("forbidden", "sessions: the owner's").into());
                }
                router
                    .hold_worker(
                        &session_id,
                        contract,
                        request.params,
                        request.caller,
                        &request.context,
                    )
                    .await
                    .map(Some)
            })
        }));
        options
    }

    /// Starts (or reuses) `session`'s worker process and holds `contract` over its Chord link.
    pub async fn hold_worker(
        self: &Arc<Self>,
        session: &str,
        contract: Arc<Contract>,
        params: JsonValue,
        caller: Caller,
        context: &Context,
    ) -> pi_rs_chord::Result<RemoteReference> {
        let slot = self.slot(session);
        let _guard = slot.lock().await;
        let worker = self
            .start(session, context)
            .await
            .map_err(|err| ChordError::new("unavailable", err.to_string()))?;
        let link = worker
            .link
            .ok_or_else(|| ChordError::new("unavailable", "worker link unavailable"))?;
        let host = self.host.get().cloned().ok_or(pi_rs_chord::Error::Closed)?;
        let peer = {
            let mut peers = self
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = peers
                .get(session)
                .filter(|cached| Arc::ptr_eq(&cached.link, &link) && !cached.link.closed())
            {
                cached.peer.clone()
            } else {
                let mut child_options = self.remote_options(Principal::worker(session).value());
                child_options.role = LinkRole::Child;
                child_options.trusted = true;
                let peer = RemotePeer::serve(host, link.clone(), child_options)?;
                if let Some(previous) = peers.insert(
                    session.to_string(),
                    CachedWorkerPeer {
                        link,
                        peer: peer.clone(),
                    },
                ) {
                    previous.peer.close();
                }
                peer
            }
        };
        peer.hold_as(contract, params, caller, context)
    }

    /// Starts session `id`'s worker through the runner.
    pub fn start(&self, session: &str, context: &Context) -> RouteFuture<WorkerLink> {
        let session = session.to_string();
        let context = context.clone();
        let runner = self.runner.get().cloned();
        let sessions = self.sessions.clone();
        Box::pin(async move {
            if sessions.one(&session).is_none() {
                return Err(RouteError::Denied("no such session".into()));
            }
            let Some(runner) = runner else {
                return Err(RouteError::Denied("no runner for session workers".into()));
            };
            runner
                .start(&session, &context)
                .await
                .map_err(|error| RouteError::Runner(error.to_string()))
        })
    }

    pub fn retry(&self, session: &str) {
        if let Some(runner) = self.runner.get() {
            runner.retry(session);
        }
    }
}

/// What reaching a session's worker was refused with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("p3: {0}")]
    Denied(String),
    #[error("p3: {0}")]
    Runner(String),
}

/// The server's process values, for plugins: what only the base has.
pub struct ServerRuntime {
    data_dir: PathBuf,
    host: Host,
    router: Arc<SessionRouter>,
    sessions_store: Arc<sessions::SessionsStore>,
    sign_ins: Arc<sign_in::SignIns>,
    worker: WorkerSpec,
    worker_idle_ms: u64,
    listening: Mutex<Option<SocketAddr>>,
    on_stop: Arc<dyn Fn() + Send + Sync>,
    registries: ServerRegistries,
    executor: OnceLock<FacetExecutor>,
    lives: Arc<crate::api::v1::contribute::PluginLives>,
}

type FacetExecutor = Arc<dyn Fn() -> Vec<PluginFacet> + Send + Sync>;

impl ServerRuntime {
    /// The Pi's data directory (a real path, private).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The route to session workers.
    pub fn sessions(&self) -> &Arc<SessionRouter> {
        &self.router
    }

    /// The server's sessions store.
    pub fn sessions_store(&self) -> &Arc<sessions::SessionsStore> {
        &self.sessions_store
    }

    /// The server's sign-in store.
    pub fn sign_ins(&self) -> &Arc<sign_in::SignIns> {
        &self.sign_ins
    }

    /// How a session's worker process starts.
    pub fn worker(&self) -> &WorkerSpec {
        &self.worker
    }

    /// How long a worker idles before it stops itself.
    pub fn worker_idle_ms(&self) -> u64 {
        self.worker_idle_ms
    }

    /// The worker plugins' current scripts: what a worker starts with, and what a reload ships.
    pub fn worker_scripts(&self) -> Vec<PluginScript> {
        // Native facets are compiled into the binary: nothing is shipped to a worker.
        Vec::new()
    }

    /// Where the server listens, once it does.
    pub fn listening(&self) -> Option<SocketAddr> {
        *self
            .listening
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn registries(&self) -> &ServerRegistries {
        &self.registries
    }

    /// Builds the changed plugins and rolls them out: the server, then the running workers.
    pub fn apply(&self, context: &Context) -> RouteFuture<RolloutResult> {
        let context = context.clone();
        let host = self.host.clone();
        let registries = self.registries.clone();
        let executor = self.executor.get().cloned();
        Box::pin(async move {
            let mut steps = Vec::new();
            let mut stopped = None;
            let facets = executor.as_ref().map(|executor| executor());
            let mut server = RolloutStep {
                name: "server".into(),
                hosts: Vec::new(),
            };
            if let Some(facets) = facets {
                match host.reload(facets, &context).await {
                    Ok(report) => {
                        for plugin in report.changed {
                            server.hosts.push(RolloutHost::Reloaded {
                                host: plugin,
                                failed: Vec::new(),
                            });
                        }
                        for failure in report.failed {
                            server.hosts.push(RolloutHost::Failed {
                                host: failure.plugin,
                                error: failure.error,
                            });
                        }
                    }
                    Err(error) => {
                        server.hosts.push(RolloutHost::Failed {
                            host: "server".into(),
                            error: error.to_string(),
                        });
                        stopped = Some("server".to_string());
                    }
                }
            }
            steps.push(server);
            if stopped.is_none()
                && let Some(runner) = registries.runner()
            {
                let scripts = Vec::new();
                match runner.rollout(&scripts, &context).await {
                    Ok(step) => steps.push(step),
                    Err(error) => {
                        steps.push(RolloutStep {
                            name: "workers".into(),
                            hosts: vec![RolloutHost::Failed {
                                host: "workers".into(),
                                error: error.to_string(),
                            }],
                        });
                        stopped = Some("workers".to_string());
                    }
                }
            }
            Ok(RolloutResult { steps, stopped })
        })
    }

    /// Stops the server, as `SIGTERM` does.
    pub fn stop(&self) {
        (self.on_stop)();
    }

    fn set_listening(&self, address: SocketAddr) {
        *self
            .listening
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(address);
    }

    fn set_executor(&self, executor: FacetExecutor) {
        let _ = self.executor.set(executor);
    }
}

impl WorkerEnvironment for ServerRuntime {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn worker_spec(&self) -> &WorkerSpec {
        &self.worker
    }

    fn worker_idle_ms(&self) -> u64 {
        self.worker_idle_ms
    }

    fn worker_scripts(&self, _context: &Context) -> RunnerFuture<Vec<PluginScript>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

/// A running server.
pub struct RunningServer {
    pub data_dir: PathBuf,
    pub port: u16,
    pub runtime: Arc<ServerRuntime>,
    closer: AsyncMutex<Option<Closer>>,
}

struct Closer {
    http: Option<HttpServer>,
    host: Host,
    lock: Option<ProcessLock>,
    data_dir: PathBuf,
}

impl RunningServer {
    /// Closes everything it opened, removes `running.json` and releases the data directory's lock.
    pub async fn close(&self, context: &Context) -> Result<(), ServerError> {
        let Some(mut closer) = self.closer.lock().await.take() else {
            return Ok(());
        };
        if let Some(runner) = self.runtime.registries.runner() {
            let _ = runner.close().await;
        }
        self.runtime.router.close_peers();
        self.runtime.lives.end_all();
        if let Some(http) = closer.http.take() {
            let _ = http.close().await;
        }
        closer
            .host
            .close(context)
            .await
            .map_err(|error| ServerError::Host(error.to_string()))?;
        remove_record(&closer.data_dir);
        drop(closer.lock.take());
        Ok(())
    }
}

/// Starts the server: resolves once its plugins are loaded and it listens; a failure closes what it
/// opened.
pub async fn start_server(
    options: ServerOptions,
    context: &Context,
) -> Result<RunningServer, ServerError> {
    let data_dir = open_data_dir(&options.data_dir)?;
    let lock = ProcessLock::acquire(&server_lock(&data_dir), Duration::from_millis(LOCK_MS))
        .await
        .map_err(|_| {
            ServerError::Message(format!("another server runs on {}", data_dir.display()))
        })?;
    start_with(&options, &data_dir, lock, context).await
}

async fn start_with(
    options: &ServerOptions,
    data_dir: &Path,
    lock: ProcessLock,
    context: &Context,
) -> Result<RunningServer, ServerError> {
    private_directory(&data_dir.join("server"))?;
    private_directory(&plugins_dir(data_dir))?;
    private_directory(&data_dir.join("cache"))?;
    let sessions_path = data_dir.join("sessions.json");
    let sessions = sessions::SessionsStore::open(sessions_path.clone())
        .map_err(|error| ServerError::Plugins(error.to_string()))?;
    let sign_ins = sign_in::SignIns::open(data_dir)
        .map_err(|error| ServerError::Plugins(error.to_string()))?;
    let owner_link = Arc::new(sign_in::OwnerLink::default());
    let router = SessionRouter::new(sessions.clone());
    let registries = ServerRegistries::default();

    let on_error = options.on_error.clone();
    let server_addr =
        server_chord_address().map_err(|error| ServerError::Host(error.to_string()))?;
    let host = create_host(HostOptions {
        address: Some(JsonValue::Object(indexmap::IndexMap::from([
            ("kind".into(), JsonValue::String(server_addr.kind.into())),
            ("params".into(), server_addr.params),
            ("key".into(), JsonValue::String(server_addr.key.into())),
        ]))),
        principal: Some(crate::principals::SERVER.value()),
        on_error: Arc::new(move |error: pi_rs_chord::host::HostError| {
            on_error(ServerError::Host(format!(
                "{}: {}",
                error.kind, error.error
            )));
        }),
        ..HostOptions::default()
    })
    .map_err(|error| ServerError::Host(error.to_string()))?;
    router.set_host(host.clone());
    let mut closer = Closer {
        http: None,
        host: host.clone(),
        lock: Some(lock),
        data_dir: data_dir.to_path_buf(),
    };

    let links = registries.clone();
    let http_error = options.on_error.clone();
    let route_sessions = router.clone();
    let http = Http::new(
        host.clone(),
        HttpOptions {
            links_path: Some("/chord".into()),
            remote: Arc::new(move |principal| route_sessions.remote_options(principal)),
            opened_link: Arc::new(move |request, close| {
                let hook = links.link_hooks().into_iter();
                let mut ends = Vec::new();
                for plugin in hook {
                    if let Some(end) = plugin(request, close.clone()) {
                        ends.push(end);
                    }
                }
                if ends.is_empty() {
                    None
                } else {
                    Some(Box::new(move || {
                        for end in ends {
                            end();
                        }
                    }))
                }
            }),
            on_error: Arc::new(move |message| {
                http_error(ServerError::Http(message));
            }),
            ..HttpOptions::default()
        },
    );

    let idle_ms = options.worker_idle_ms.max(1);
    let lives = crate::api::v1::contribute::PluginLives::new();
    let runtime = Arc::new(ServerRuntime {
        data_dir: data_dir.to_path_buf(),
        host: host.clone(),
        router: router.clone(),
        sessions_store: sessions.clone(),
        sign_ins: sign_ins.clone(),
        worker: options.worker.clone(),
        worker_idle_ms: idle_ms,
        listening: Mutex::new(None),
        on_stop: options.on_stop.clone(),
        registries: registries.clone(),
        executor: OnceLock::new(),
        lives: lives.clone(),
    });
    let runner = Arc::new(LocalRunner::new(
        Arc::new(ProcessLauncher),
        RunnerOptions::default(),
    ));
    runner.set_environment(runtime.clone());
    router.set_runner(runner.clone());

    let facets = vec![
        server_core::facet(runtime.clone(), registries.clone(), &lives),
        control::facet(runtime.clone(), http.clone(), &lives),
        runner_server::facet(runner.clone(), registries.clone(), &lives),
        sessions::server_facet(sessions.clone(), router.clone(), &lives),
        PluginFacet::new(
            "p3-sign-in",
            sign_in::server_facet(
                sign_ins.clone(),
                owner_link.clone(),
                http.clone(),
                runtime.clone(),
                registries.clone(),
                &lives,
            ),
        ),
    ];
    let stored = Arc::new(facets.clone());
    let executor: FacetExecutor = Arc::new(move || (*stored).clone());
    runtime.set_executor(executor);
    let report = host
        .load(facets.clone(), context)
        .await
        .map_err(|error| ServerError::Plugins(error.to_string()))?;
    let failed = report
        .failed
        .iter()
        .map(|failure| failure.plugin.clone())
        .collect::<Vec<_>>();
    if !failed.is_empty() {
        let _ = host.close(context).await;
        return Err(ServerError::Plugins(format!(
            "plugins failed to load: {}",
            failed.join(", ")
        )));
    }

    let address = SocketAddr::from(([127, 0, 0, 1], options.port));
    let http_server = http
        .listen(address)
        .await
        .map_err(|error| ServerError::Http(listen_failed(options.port, &error)))?;
    let port = http_server.address.port();
    runtime.set_listening(http_server.address);

    let record = ServerRecord {
        pid: std::process::id(),
        port,
        version: options.version.clone(),
        started_at: now_ms(),
    };
    write_record(data_dir, &record)?;
    closer.http = Some(http_server);
    closer.host = host;

    Ok(RunningServer {
        data_dir: data_dir.to_path_buf(),
        port,
        runtime,
        closer: AsyncMutex::new(Some(closer)),
    })
}

/// The error of a listen on `port` that failed with `error`: `AddrInUse` names the port.
pub fn listen_failed(port: u16, error: &pi_rs_chord::node::http::HttpError) -> String {
    let why = error.message.to_ascii_lowercase();
    if why.contains("in use") || why.contains("address already in use") {
        format!("cannot listen on 127.0.0.1:{port}: the port is in use")
    } else {
        format!("cannot listen on 127.0.0.1:{port}: {}", error.message)
    }
}

/// Whether the server on `data_dir` runs: its lock is held and `running.json` is there.
pub fn running(data_dir: &Path) -> Option<ServerRecord> {
    crate::records::running(data_dir)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
