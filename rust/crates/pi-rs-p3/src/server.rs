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
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    context::Context,
    host::{Host, HostOptions, PluginFacet, create_host},
    node::http::{Http, HttpOptions, HttpServer},
    node::workers::ProcessLock,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
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

/// The route to session workers: whether a session exists, who may reach it, and which runner starts
/// its worker. The local seam for `pi-rs-chord`'s address-level routing.
pub struct SessionRouter {
    sessions: Arc<sessions::SessionsStore>,
    runner: OnceLock<Arc<dyn WorkerRunner>>,
}

impl SessionRouter {
    pub fn new(sessions: Arc<sessions::SessionsStore>) -> Arc<Self> {
        Arc::new(Self {
            sessions,
            runner: OnceLock::new(),
        })
    }

    pub fn set_runner(&self, runner: Arc<dyn WorkerRunner>) {
        let _ = self.runner.set(runner);
    }

    pub fn exists(&self, session: &str) -> bool {
        self.sessions.one(session).is_some()
    }

    /// Admission: a session's worker is reached only while the session exists, and never by another
    /// session's worker.
    pub fn admit(&self, principal: &Principal, session: &str) -> sessions::Admission {
        sessions::admit_session(principal, self.exists(session))
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
    let host = create_host(HostOptions {
        address: Some(pi_rs_chord::json::JsonValue::from_serde(
            crate::api::v1::server_address(),
        )),
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
    let mut closer = Closer {
        http: None,
        host: host.clone(),
        lock: Some(lock),
        data_dir: data_dir.to_path_buf(),
    };

    let links = registries.clone();
    let http_error = options.on_error.clone();
    let http = Http::new(
        host.clone(),
        HttpOptions {
            links_path: Some("/chord".into()),
            opened: Arc::new(move |request: &pi_rs_chord::node::http::HttpRequest| {
                let hook = links.link_hooks().into_iter();
                let mut ends = Vec::new();
                for plugin in hook {
                    if let Some(end) = plugin(request) {
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
