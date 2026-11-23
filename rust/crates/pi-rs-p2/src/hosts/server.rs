use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use pi_rs_chord::{
    context::Context,
    host::{Host, HostOptions, PluginFacet, ReloadReport, create_host},
    json::JsonValue,
    node::{
        app::{Files, FilesCaps, private_directory},
        http::{
            Http, HttpContribution, HttpError, HttpOptions, HttpServer, HttpWrap, Provenance,
            RouteOptions, WrapPhase, json_response,
        },
        workers::ProcessLock,
    },
    store::StoreEngine,
};
use serde_json::json;

use crate::{
    api::v1::{FacetKind, PluginRegistry},
    runtime::{
        host_dirs,
        server::{ServerInfo, info_facet},
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
    pub allowed_hosts: Vec<String>,
    pub registry: PluginRegistry,
    pub plugins: Option<Vec<String>>,
}

impl ServerOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            app_dir: None,
            port: 8788,
            fake: false,
            allowed_hosts: Vec::new(),
            registry: PluginRegistry::new(),
            plugins: None,
        }
    }
}

pub struct P2Server {
    pub host: Host,
    pub address: SocketAddr,
    pub data_dir: PathBuf,
    pub plugins: Vec<String>,
    core: Vec<PluginFacet>,
    http: HttpServer,
    store: StoreEngine,
    _lock: ProcessLock,
}

impl P2Server {
    /// Replace the collection of native server facets. Chord retains a previous
    /// facet if its replacement fails to activate and reports that failure.
    pub async fn reload(
        &mut self,
        registry: &PluginRegistry,
        requested: Option<&[String]>,
    ) -> Result<ReloadReport> {
        let selected = registry.select(requested)?;
        let mut facets = self.core.clone();
        facets.extend(registry.facets(FacetKind::Server, &selected));
        let report = self.host.reload(facets, &Context::background()).await?;
        self.plugins.retain(|name| {
            selected.contains(name) || report.failed.iter().any(|f| &f.plugin == name)
        });
        self.plugins.extend(
            selected
                .into_iter()
                .filter(|name| !report.failed.iter().any(|f| &f.plugin == name)),
        );
        self.plugins.sort();
        self.plugins.dedup();
        Ok(report)
    }

    pub async fn close(self) -> Result<()> {
        let http = self.http.close().await.map_err(Error::from);
        let host = self
            .host
            .close(&Context::background())
            .await
            .map_err(Error::from);
        let store = self.store.database().close().await.map_err(Error::from);
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
        address: Some(JsonValue::from_serde(
            crate::runtime::address::server_address(),
        )),
        principal: Some(json!({"kind":"server"})),
        store: Some(store.clone()),
        on_error: Arc::new(|error| eprintln!("p2 server: {}", error.error)),
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
    let http = Http::new(
        host.clone(),
        HttpOptions {
            links_path: Some("/chord".into()),
            max_payload: 1024 * 1024,
            max_buffered: 4 * 1024 * 1024,
            close_timeout: Duration::from_secs(8),
            on_error: Arc::new(|error| eprintln!("p2 server: {error}")),
            ..Default::default()
        },
    );
    let http = http
        .listen(SocketAddr::from(([127, 0, 0, 1], options.port)))
        .await?;
    let mut server = P2Server {
        host,
        address: http.address,
        data_dir: options.data_dir,
        plugins: selected,
        core: Vec::new(),
        http,
        store,
        _lock: lock,
    };
    let load = async {
        let info = ServerInfo {
            app: options
                .app_dir
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            data_dir: server.data_dir.to_string_lossy().into_owned(),
            fake: options.fake,
            allowed_hosts: options.allowed_hosts,
            port: server.address.port(),
        };
        server.core = vec![
            PluginFacet::new("@p2/core", info_facet(info)?),
            PluginFacet::new("p2.core-http", routes.facet()),
            PluginFacet::new("chord.app.files", files.facet()?),
        ];
        let mut facets = server.core.clone();
        facets.extend(options.registry.facets(FacetKind::Server, &server.plugins));
        let report = server.host.load(facets, &Context::background()).await?;
        if let Some(failure) = report.failed.into_iter().next() {
            return Err(Error::Activation {
                plugin: failure.plugin,
                reason: failure.error,
            });
        }
        Ok(())
    }
    .await;
    match load {
        Ok(()) => Ok(server),
        Err(error) => {
            if let Err(close) = server.close().await {
                eprintln!("p2 server: {close}");
            }
            Err(error)
        }
    }
}
