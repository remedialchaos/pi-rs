use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[cfg(unix)]
use super::dev::{DevCommand, DevControl, DevResult, channel};
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
    pub dev: bool,
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
            dev: std::env::var("P2_DEV").as_deref() == Ok("1"),
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
    http: HttpServer,
    natives: Arc<ServerNatives>,
    _lock: ProcessLock,
    #[cfg(unix)]
    dev: Option<(Arc<StandaloneDev>, channel::Channel)>,
}

#[cfg(unix)]
struct StandaloneDev {
    control: DevControl,
    workers: SessionWorkers,
    loading: Arc<tokio::sync::Mutex<RunningPlugins>>,
}

#[cfg(unix)]
impl StandaloneDev {
    async fn execute(&self, command: DevCommand) -> DevResult {
        let DevCommand::Reload { plugins } = command else {
            let result = DevResult {
                ok: false,
                message: "Restart refused: Restart needs the npm run dev supervisor".into(),
                plugins: None,
            };
            self.control.notify(&result, str::to_owned).await;
            return result;
        };
        let result = self
            .control
            .reload(
                plugins,
                Default::default(),
                |names| async move {
                    let mut loading = self.loading.lock().await;
                    let RunningPlugins {
                        sources,
                        runtime,
                        registry,
                    } = &mut *loading;
                    let update = sources
                        .reload_named(runtime, &names, &Context::background())
                        .await
                        .map_err(|error| error.to_string())?;
                    update.log_problems();
                    let report = update.report;
                    self.workers.update_authorizers(
                        registry.session_authorizers(&update.selected),
                        &report.failed,
                    );
                    if let Some(failure) = report.failed.first() {
                        return Err(format!("{} (server): {}", failure.plugin, failure.error));
                    }
                    let changed: Vec<_> = report
                        .changed
                        .into_iter()
                        .chain(report.added)
                        .chain(report.removed)
                        .collect();
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
        self.workers.set_builtin_names(registry.select(None)?);
        self.workers
            .update_authorizers(registry.session_authorizers(&self.plugins), &report.failed);
        Ok(report)
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
        let store = self
            .natives
            .store()
            .database()
            .close()
            .await
            .map_err(Error::from);
        http.and(host).and(store)
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
                telemetry: false,
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
    let natives = Arc::new(ServerNatives::new(
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
    ));
    let mut server = P2Server {
        host,
        address: http.address,
        data_dir: options.data_dir,
        plugins: selected,
        workers,
        loading: None,
        http,
        natives,
        _lock: lock,
        #[cfg(unix)]
        dev: None,
    };
    let load = async {
        install_natives(&server.host, &server.natives)?;
        let telemetry_backend: Arc<dyn crate::plugins::telemetry::TelemetryBackend> = Arc::new(
            crate::plugins::telemetry::InMemoryTelemetryBackend(
                pi_rs_telemetry::InMemoryTelemetryContext::new(),
            ),
        );
        crate::runtime::server::install_service(
            &server.host,
            crate::plugins::telemetry::TELEMETRY_STORE_SERVICE,
            &Arc::new(telemetry_backend),
        )?;
        add_request_services(&mut routes, &server.natives);
        let core = vec![
            PluginFacet::new(
                "@p2/core",
                info_facet(&server.natives, server.host.downgrade())?,
            ),
            PluginFacet::new("p2.core-http", routes.facet()),
            PluginFacet::new("chord.app.files", files.facet()?),
        ];
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
                let dev = Arc::new(StandaloneDev {
                    control: DevControl::default(),
                    workers: server.workers.clone(),
                    loading,
                });
                let handler = dev.clone();
                let channel = channel::serve(&server.data_dir, move |cmd| {
                    let handler = handler.clone();
                    async move { handler.execute(cmd).await }
                })
                .await?;
                let reload_dev = dev.clone();
                let reload_handler: Arc<crate::plugins::dev_tools::ReloadHandler> =
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
                    });
                crate::runtime::server::install_service(
                    &server.host,
                    crate::plugins::dev_tools::PLUGIN_RELOAD_SERVICE,
                    &Arc::new(reload_handler),
                )?;
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
