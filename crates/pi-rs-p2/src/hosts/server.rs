use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use super::dev::{
    DEV_REBUILD_CMD_ENV, DEV_SUPERVISOR_ENV, DevCommand, DevControl, DevRebuildHook,
    DevRebuildRequest, DevResult, channel, shell_rebuild_hook,
};
#[cfg(not(unix))]
use super::dev::{DEV_REBUILD_CMD_ENV, DEV_SUPERVISOR_ENV, DevRebuildHook, shell_rebuild_hook};
use pi_rs_chord::{
    context::Context,
    contracts::Contract,
    host::{Host, HostOptions, PluginFacet, ReloadReport, create_host},
    json::JsonValue,
    node::{
        app::{Files, FilesCaps, host_data_contract, private_directory},
        http::{
            Http, HttpContribution, HttpError, HttpOptions, HttpServer, HttpWrap, Provenance,
            RouteOptions, WrapPhase, json_response,
        },
        workers::{ProcessLock, WorkerOptions},
    },
};
use serde_json::{Map, json};

use super::{
    session_workers::{SessionWorkers, catalog_session},
    worker::WorkerInit,
};

use crate::{
    api::v1::{FacetKind, PluginRegistry},
    runtime::{
        host_dirs,
        server::{ServerInfo, ServerNatives, add_request_services, info_facet, install_natives},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Chord(#[from] pi_rs_chord::Error),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Another p2 server uses {}", .directory.display())]
    Locked {
        directory: PathBuf,
        #[source]
        source: pi_rs_chord::Error,
    },
    #[error("p2 plugin {plugin}: {reason}")]
    Activation { plugin: String, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct ServerOptions {
    pub data_dir: PathBuf,
    pub app_dir: Option<PathBuf>,
    pub port: u16,
    pub fake: bool,
    pub telemetry: Option<bool>,
    pub default_model: Option<(String, String)>,
    pub dev: bool,
    pub dev_supervised: bool,
    pub dev_rebuild: Option<DevRebuildHook>,
    pub allowed_hosts: Vec<String>,
    pub registry: PluginRegistry,
    pub plugins: Option<Vec<String>>,
    /// Additional session RPC contracts supplied by plugins outside the built-in catalog.
    pub worker_contracts: Vec<Arc<Contract>>,
    /// Defaults to this application with the native `--worker` entry point.
    pub worker_process: Option<WorkerOptions>,
}

impl ServerOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            app_dir: None,
            port: 8788,
            fake: false,
            telemetry: None,
            default_model: crate::plugins::models::resources::env_default_model(),
            dev: std::env::var("P2_DEV").as_deref() == Ok("1"),
            dev_supervised: std::env::var(DEV_SUPERVISOR_ENV).as_deref() == Ok("1"),
            dev_rebuild: std::env::var(DEV_REBUILD_CMD_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(shell_rebuild_hook),
            allowed_hosts: Vec::new(),
            registry: PluginRegistry::new(),
            plugins: None,
            worker_contracts: Vec::new(),
            worker_process: None,
        }
    }
}

struct RunningPlugins {
    sources: super::plugins::PluginSources,
    runtime: pi_rs_chord::node::plugins::PluginRuntime,
    registry: PluginRegistry,
}

pub struct P2Server {
    pub host: Host,
    pub address: SocketAddr,
    pub data_dir: PathBuf,
    pub plugins: Vec<String>,
    pub workers: SessionWorkers,
    loading: Option<Arc<tokio::sync::Mutex<RunningPlugins>>>,
    tui_bundles: super::tui_bundles::SharedTuiBundles,
    http: HttpServer,
    natives: Arc<ServerNatives>,
    _telemetry: Option<Arc<Arc<dyn crate::plugins::telemetry::TelemetryBackend>>>,
    telemetry_store: Option<pi_rs_chord::telemetry::SqliteTelemetry>,
    #[cfg(unix)]
    _reload: Option<Arc<Arc<crate::plugins::dev_tools::ReloadHandler>>>,
    _lock: ProcessLock,
    #[cfg(unix)]
    dev: Option<(Arc<StandaloneDev>, channel::Channel)>,
    #[cfg(unix)]
    restart: Option<tokio::sync::watch::Receiver<bool>>,
}

#[cfg(unix)]
struct StandaloneDev {
    control: DevControl,
    workers: SessionWorkers,
    data_dir: PathBuf,
    tui_bundles: super::tui_bundles::SharedTuiBundles,
    loading: Arc<tokio::sync::Mutex<RunningPlugins>>,
    rebuild: Option<DevRebuildHook>,
    supervised: bool,
}

#[cfg(unix)]
impl StandaloneDev {
    async fn execute(&self, command: DevCommand) -> DevResult {
        let DevCommand::Reload { plugins } = command else {
            if !self.supervised {
                let result = DevResult {
                    ok: false,
                    message: "Restart refused: Restart needs the npm run dev supervisor".into(),
                    plugins: None,
                };
                self.control.notify(&result, str::to_owned).await;
                return result;
            }
            return match self
                .control
                .restart(|| async { Ok(()) }, str::to_owned)
                .await
            {
                Ok(()) => DevResult {
                    ok: true,
                    message: "Restarting server".into(),
                    plugins: None,
                },
                Err(error) => DevResult {
                    ok: false,
                    message: format!("Restart refused: {error}"),
                    plugins: None,
                },
            };
        };
        let (dirty, reserved) = {
            let loading = self.loading.lock().await;
            let dirty = loading
                .sources
                .dirty_plugins(&loading.runtime)
                .unwrap_or_default();
            let reserved: BTreeSet<String> = loading
                .registry
                .select(None)
                .unwrap_or_default()
                .into_iter()
                .collect();
            (dirty, reserved)
        };
        let mut dirty = dirty;
        let prev_tui = self
            .tui_bundles
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .manifest()
            .plugins
            .iter()
            .map(|entry| (entry.name.clone(), entry.digest.clone()))
            .collect::<BTreeMap<_, _>>();
        if let Ok(scanned_tui) =
            super::tui_bundles::TuiBundleIndex::build(&self.data_dir, &reserved)
        {
            let next_tui = scanned_tui
                .manifest()
                .plugins
                .iter()
                .map(|entry| (entry.name.clone(), entry.digest.clone()))
                .collect::<BTreeMap<_, _>>();
            for (name, digest) in &next_tui {
                if prev_tui.get(name) != Some(digest) {
                    dirty.insert(name.clone());
                }
            }
            for name in prev_tui.keys() {
                if !next_tui.contains_key(name) {
                    dirty.insert(name.clone());
                }
            }
        }
        let requested_non_empty = plugins.as_ref().is_some_and(|p| !p.is_empty());
        let result = self
            .control
            .reload(
                plugins,
                dirty,
                |names| async move {
                    if let Some(rebuild) = &self.rebuild {
                        rebuild(DevRebuildRequest {
                            data_dir: self.data_dir.clone(),
                            plugins_dir: host_dirs::user_plugins_directory(&self.data_dir),
                            plugins: names.clone(),
                        })
                        .await?;
                    }
                    let mut loading = self.loading.lock().await;
                    let RunningPlugins {
                        sources,
                        runtime,
                        registry,
                    } = &mut *loading;
                    let reload_targets = if requested_non_empty {
                        names
                    } else {
                        Vec::new()
                    };
                    let update = sources
                        .reload_named(runtime, &reload_targets, &Context::background())
                        .await
                        .map_err(|error| error.to_string())?;
                    update.log_problems();
                    let report = update.report;
                    let reserved: BTreeSet<String> = registry
                        .select(None)
                        .map_err(|error| error.to_string())?
                        .into_iter()
                        .collect();
                    let next_bundles =
                        super::tui_bundles::TuiBundleIndex::build(&self.data_dir, &reserved)
                            .map_err(|error| error.to_string())?;
                    let next_tui = next_bundles
                        .manifest()
                        .plugins
                        .iter()
                        .map(|entry| (entry.name.clone(), entry.digest.clone()))
                        .collect::<BTreeMap<_, _>>();
                    *self.tui_bundles.write().unwrap_or_else(|e| e.into_inner()) = next_bundles;
                    self.workers.update_authorizers(
                        registry.session_authorizers(&update.selected),
                        &report.failed,
                    );
                    if let Some(failure) = report.failed.first() {
                        return Err(format!("{} (server): {}", failure.plugin, failure.error));
                    }
                    let mut changed: BTreeSet<String> = report
                        .changed
                        .into_iter()
                        .chain(report.added)
                        .chain(report.removed)
                        .collect();
                    for (name, digest) in &next_tui {
                        if prev_tui.get(name) != Some(digest)
                            && (reload_targets.is_empty() || reload_targets.contains(name))
                        {
                            changed.insert(name.clone());
                        }
                    }
                    for name in prev_tui.keys() {
                        if !next_tui.contains_key(name)
                            && (reload_targets.is_empty() || reload_targets.contains(name))
                        {
                            changed.insert(name.clone());
                        }
                    }
                    let changed: Vec<String> = changed.into_iter().collect();
                    Ok((changed.clone(), changed))
                },
                str::to_owned,
            )
            .await;
        match result {
            Ok(plugins) => DevResult {
                ok: true,
                message: if plugins.is_empty() {
                    "No dirty plugins".into()
                } else {
                    format!("Reloaded {}", plugins.join(", "))
                },
                plugins: Some(plugins),
            },
            Err(error) => DevResult {
                ok: false,
                message: format!("Reload refused: {error}"),
                plugins: None,
            },
        }
    }
}

impl P2Server {
    /// Replace the collection of native server facets. Chord retains a previous
    /// facet if its replacement fails to activate and reports that failure.
    pub async fn reload(
        &mut self,
        registry: &PluginRegistry,
        requested: Option<&[String]>,
    ) -> Result<ReloadReport> {
        let mut loading = self
            .loading
            .as_ref()
            .ok_or(pi_rs_chord::Error::Closed)?
            .lock()
            .await;
        let RunningPlugins {
            sources,
            runtime,
            registry: active,
        } = &mut *loading;
        sources.replace_registry(registry, requested)?;
        let update = sources.reload(runtime, &Context::background()).await?;
        update.log_problems();
        self.plugins = update.selected;
        let report = update.report;
        *active = registry.clone();
        let reserved: BTreeSet<String> = registry.select(None)?.into_iter().collect();
        let next_bundles = super::tui_bundles::TuiBundleIndex::build(&self.data_dir, &reserved)?;
        *self.tui_bundles.write().unwrap_or_else(|e| e.into_inner()) = next_bundles;
        self.workers.set_builtin_names(registry.select(None)?);
        self.workers
            .update_authorizers(registry.session_authorizers(&self.plugins), &report.failed);
        Ok(report)
    }

    pub async fn wait_for_restart(&self) {
        #[cfg(unix)]
        if let Some(mut restart) = self.restart.clone() {
            while !*restart.borrow_and_update() {
                if restart.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            return;
        }
        std::future::pending::<()>().await;
    }

    pub async fn close(self) -> Result<()> {
        #[cfg(unix)]
        if let Some((dev, channel)) = self.dev {
            dev.control.close().await;
            channel.close().await;
        }
        let http = self.http.close().await.map_err(Error::from);
        self.workers.close().await;
        let host = self
            .host
            .close(&Context::background())
            .await
            .map_err(Error::from);
        let telemetry = if let Some(telemetry) = self.telemetry_store {
            let flush = telemetry.close().await.map_err(Error::from);
            let db = telemetry.database().close().await.map_err(Error::from);
            flush.and(db)
        } else {
            Ok(())
        };
        let store = self
            .natives
            .store()
            .database()
            .close()
            .await
            .map_err(Error::from);
        http.and(host).and(telemetry).and(store)
    }
}

pub async fn start_server(options: ServerOptions) -> Result<P2Server> {
    let selected = options.registry.select(options.plugins.as_deref())?;
    private_directory(&options.data_dir)?;
    let lock = ProcessLock::acquire(
        &options.data_dir.join("server.lock"),
        Duration::from_secs(5),
    )
    .await
    .map_err(|source| Error::Locked {
        directory: options.data_dir.clone(),
        source,
    })?;
    private_directory(&host_dirs::user_plugins_directory(&options.data_dir))?;
    private_directory(&options.data_dir.join("sessions"))?;
    host_dirs::assert_migrated(&options.data_dir)?;
    let directory = host_dirs::server_directory(&options.data_dir);
    let store = host_dirs::open_host_store(&directory).await?;
    let host = create_host(HostOptions {
        principal_policy: Some(crate::runtime::principals::Principal::POLICY),
        address: Some(JsonValue::from_serde(
            crate::runtime::address::server_address(),
        )),
        principal: Some(json!({"kind":"server"})),
        store: Some(store.clone()),
        on_error: Arc::new(|error| server_error(&error.error)),
        ..Default::default()
    })?;
    let files = Files::open(
        store.clone(),
        host_dirs::host_files(&directory),
        FilesCaps::default(),
    )
    .await?;
    let mut routes = HttpContribution::new("p2.core-http");
    routes.hosts.clone_from(&options.allowed_hosts);
    let nonce = uuid::Uuid::new_v4().to_string();
    super::web::add_routes(&mut routes, nonce.clone())?;
    let reserved: BTreeSet<String> = options.registry.select(None)?.into_iter().collect();
    let tui_bundles = Arc::new(std::sync::RwLock::new(
        super::tui_bundles::TuiBundleIndex::build(&options.data_dir, &reserved)?,
    ));
    super::tui_bundles::add_routes(&mut routes, tui_bundles.clone())?;
    let mut headers = HttpWrap::new(
        "p2.response-headers",
        WrapPhase::Headers,
        move |request, next| {
            let csp = (!matches!(request.provenance, Provenance::Relayed { .. }))
                .then(|| crate::runtime::http::content_security_policy(&nonce, &request.origin));
            Box::pin(async move {
                let mut response = next(request).await?;
                if let Some(csp) = csp {
                    response
                        .headers_mut()
                        .entry(http::header::CONTENT_SECURITY_POLICY)
                        .or_insert(http::HeaderValue::from_str(&csp)?);
                }
                Ok(response)
            })
        },
    );
    headers.all = true;
    routes.wraps.push(headers);
    let mut health = RouteOptions::public(|request| {
        Box::pin(async move {
            use crate::runtime::http::{forwarded, same_origin};
            use http::{HeaderValue, header};
            use pi_rs_chord::node::http::text_response;

            let mut response =
                if matches!(request.provenance, Provenance::Local) && forwarded(&request.headers) {
                    text_response(403, "Localhost only")?
                } else if !same_origin(&request.headers, &request.origin) {
                    text_response(403, "Invalid origin")?
                } else {
                    json_response(&json!({"ok":true,"name":"p2"}))?
                };
            let headers = response.headers_mut();
            let mime = if headers[header::CONTENT_TYPE] == "application/json; charset=utf-8" {
                "application/json"
            } else {
                "text/plain"
            };
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            Ok(response)
        })
    });
    health.cross_site = true;
    routes.route("GET /health", health)?;
    let mut process = match options.worker_process {
        Some(process) => process,
        None => {
            let mut process =
                WorkerOptions::new(std::env::current_exe()?, directory.join("worker-locks"));
            process.args.push("--worker".into());
            if let Ok(idle) = std::env::var("P2_IDLE_MS")
                && let Ok(idle) = idle.parse::<u64>()
            {
                process.idle_timeout = Duration::from_millis(idle);
            }
            process
        }
    };
    process.lock_dir = directory.join("worker-locks");
    let mut worker_contracts = BTreeMap::new();
    for name in crate::api::v1::p2_contracts::SESSION_CONTRACTS {
        let contract = crate::api::v1::p2_contracts::lookup(name)?;
        worker_contracts.insert((*name).to_owned(), Arc::new(contract));
    }
    for contract in options
        .worker_contracts
        .into_iter()
        .chain([host_data_contract()?])
    {
        let name = contract.name().to_owned();
        if worker_contracts.insert(name.clone(), contract).is_some() {
            return Err(
                pi_rs_chord::Error::Invalid(format!("Duplicate worker contract: {name}")).into(),
            );
        }
    }
    let catalog = Arc::new(crate::api::v1::p2_contracts::lookup("p2.catalog")?);
    let catalog_host = host.clone();
    let data_dir = std::fs::canonicalize(&options.data_dir)?;
    let app = options.app_dir.clone().unwrap_or_default();
    let fake = options.fake;
    let telemetry_on =
        crate::plugins::telemetry::telemetry_enabled(&options.data_dir, options.telemetry);
    let sqlite_telemetry = if telemetry_on {
        let tel_path = host_dirs::telemetry_store(&options.data_dir);
        pi_rs_chord::node::app::private_store(&tel_path)?;
        let tel_db = pi_rs_chord::sqlite::SqliteDatabase::open(
            &tel_path,
            pi_rs_chord::sqlite::Options::default(),
        )
        .await?;
        let retention_days = std::env::var("P2_TELEMETRY_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(7.0);
        let retention_ms = retention_days * 86_400_000.0;
        let opened = match pi_rs_chord::telemetry::SqliteTelemetry::open(
            tel_db.clone(),
            retention_ms,
            10_000,
        )
        .await
        {
            Ok(opened) => opened,
            Err(err) => {
                let _ = tel_db.close().await;
                return Err(err.into());
            }
        };
        Some(opened)
    } else {
        None
    };
    let workers = SessionWorkers::new(&host, process, move |session, generation, context| {
        let (host, catalog, data_dir, app) = (
            catalog_host.clone(),
            catalog.clone(),
            data_dir.clone(),
            app.clone(),
        );
        Box::pin(async move {
            let values = if let Ok(provider) =
                crate::api::v1::server::service_of::<super::session_workers::WorkerInitProvider>(
                    &host,
                    super::session_workers::WORKER_INIT_PROVIDER_SERVICE,
                ) {
                (*provider)(session, generation, context).await?
            } else {
                let value = catalog_session(&host, catalog, &session, &context).await?;
                Map::from_iter([("session".into(), value)])
            };
            Ok(WorkerInit {
                app,
                data_dir,
                fake,
                telemetry: telemetry_on,
                generation,
                values,
            })
        })
    })?;
    let route_workers = workers.clone();
    let http = Http::new(
        host.clone(),
        HttpOptions {
            links_path: Some("/chord".into()),
            remote: Arc::new(move |principal| {
                route_workers.remote_options(principal, worker_contracts.clone())
            }),
            prefer_principal: |principal| principal["kind"] == "owner",
            admit_link: crate::runtime::http::browser_links(super::web::browser_base()),
            max_payload: 1024 * 1024,
            max_buffered: 4 * 1024 * 1024,
            close_timeout: Duration::from_secs(8),
            on_error: Arc::new(|error| server_error(&error)),
            ..Default::default()
        },
    );
    let http = http
        .listen(SocketAddr::from(([127, 0, 0, 1], options.port)))
        .await?;
    let natives = Arc::new(
        ServerNatives::new(
            ServerInfo {
                app: options
                    .app_dir
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                data_dir: options.data_dir.to_string_lossy().into_owned(),
                fake: options.fake,
                allowed_hosts: options.allowed_hosts,
                port: http.address.port(),
            },
            workers.clone(),
            host.clone(),
            store,
        )
        .with_default_model(options.default_model),
    );
    let telemetry_backend: Option<Arc<Arc<dyn crate::plugins::telemetry::TelemetryBackend>>> =
        sqlite_telemetry.as_ref().map(|sqlite| {
            Arc::new(Arc::new(crate::plugins::telemetry::SqliteTelemetryBackend(
                sqlite.clone(),
            ))
                as Arc<dyn crate::plugins::telemetry::TelemetryBackend>)
        });
    let mut server = P2Server {
        host,
        address: http.address,
        data_dir: options.data_dir,
        plugins: selected,
        workers,
        loading: None,
        tui_bundles: tui_bundles.clone(),
        http,
        natives,
        _telemetry: telemetry_backend.clone(),
        telemetry_store: sqlite_telemetry.clone(),
        #[cfg(unix)]
        _reload: None,
        _lock: lock,
        #[cfg(unix)]
        dev: None,
        #[cfg(unix)]
        restart: None,
    };
    let load = async {
        install_natives(&server.host, &server.natives)?;
        if let Some(ref telemetry_backend) = telemetry_backend {
            crate::runtime::server::install_service(
                &server.host,
                crate::plugins::telemetry::TELEMETRY_STORE_SERVICE,
                telemetry_backend,
            )?;
        }
        add_request_services(&mut routes, &server.natives);
        let mut core = vec![
            PluginFacet::new(
                "@p2/core",
                info_facet(&server.natives, server.host.downgrade())?,
            ),
            PluginFacet::new("p2.core-http", routes.facet()),
            PluginFacet::new("chord.app.files", files.facet()?),
        ];
        if let Some(ref sqlite) = sqlite_telemetry {
            core.push(PluginFacet::new(
                "chord.telemetry",
                crate::plugins::telemetry::ingest_facet(sqlite.clone())?,
            ));
        }
        let mut sources = super::plugins::PluginSources::new(
            &options.registry,
            options.plugins.as_deref(),
            &host_dirs::user_plugins_directory(&server.data_dir),
            FacetKind::Server,
        )?;
        let (runtime, update) = sources
            .load(server.host.clone(), core, &Context::background())
            .await?;
        update.log_problems();
        if let Some(failure) = update.report.failed.iter().find(|f| {
            matches!(
                f.plugin.as_str(),
                "@p2/core" | "p2.core-http" | "chord.app.files"
            )
        }) {
            return Err(Error::Activation {
                plugin: failure.plugin.clone(),
                reason: failure.error.clone(),
            });
        }
        server.plugins = update.selected;
        let loading = Arc::new(tokio::sync::Mutex::new(RunningPlugins {
            sources,
            runtime,
            registry: options.registry.clone(),
        }));
        server.loading = Some(loading.clone());
        server
            .workers
            .set_builtin_names(options.registry.select(None)?);
        server
            .workers
            .update_authorizers(options.registry.session_authorizers(&server.plugins), &[]);
        if options.dev {
            #[cfg(unix)]
            {
                let (restart_tx, restart_rx) = tokio::sync::watch::channel(false);
                let dev = Arc::new(StandaloneDev {
                    control: DevControl::default(),
                    workers: server.workers.clone(),
                    data_dir: server.data_dir.clone(),
                    tui_bundles: server.tui_bundles.clone(),
                    loading,
                    rebuild: options.dev_rebuild,
                    supervised: options.dev_supervised,
                });
                let handler = dev.clone();
                let channel = channel::serve_with_after(
                    &server.data_dir,
                    move |cmd| {
                        let handler = handler.clone();
                        async move { handler.execute(cmd).await }
                    },
                    move |cmd, result| {
                        if matches!(cmd, DevCommand::Restart) && result.ok {
                            restart_tx.send_replace(true);
                        }
                    },
                )
                .await?;
                server.restart = Some(restart_rx);
                let reload_dev = dev.clone();
                let reload_handler: Arc<Arc<crate::plugins::dev_tools::ReloadHandler>> = Arc::new(
                    Arc::new(move |plugins| {
                        let reload_dev = reload_dev.clone();
                        Box::pin(async move {
                            let res = reload_dev.execute(DevCommand::Reload { plugins }).await;
                            Ok(JsonValue::from_serde(json!({
                                "refused": if res.ok { serde_json::Value::Null } else { json!(res.message) },
                                "plugins": res.plugins.unwrap_or_default(),
                                "logs": []
                            })))
                        })
                    }),
                );
                crate::runtime::server::install_service(
                    &server.host,
                    crate::plugins::dev_tools::PLUGIN_RELOAD_SERVICE,
                    &reload_handler,
                )?;
                server._reload = Some(reload_handler);
                server.dev = Some((dev, channel));
            }
            #[cfg(not(unix))]
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Private dev control requires Unix sockets",
            )
            .into());
        }
        Ok(())
    }
    .await;
    match load {
        Ok(()) => Ok(server),
        Err(error) => {
            if let Err(close) = server.close().await {
                server_error(&close);
            }
            Err(error)
        }
    }
}

fn server_error(error: &impl std::fmt::Display) {
    use crate::runtime::logs::{LogLevel, process_logs};
    let message = format!("p2 server: {error}");
    eprintln!("{message}");
    process_logs().append("server", LogLevel::Stderr, &message);
}
