use super::owners::{WorkerBoot, WorkerOwners, host_data_facet};
use super::*;
use crate::node::{
    http::{Http, HttpOptions, HttpServer},
    plugins::{PluginRuntime, PluginScript},
    surfaces::Surface,
    workers::{Control, ProcessLock, WorkerSession},
};
use crate::{
    context::Context,
    contracts::{Contract, Member, schema as s},
    host::{
        Facet, Host, HostOptions, Members, PluginFacet, ServeOptions, create_host, define_facet,
    },
    json::JsonValue,
    remote::{RemoteOptions, RemotePeer},
    sqlite::{Options, SqliteDatabase},
    store::StoreEngine,
};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
pub fn app_info_contract() -> Result<Contract> {
    Contract::local(
        "chord.app.info",
        2,
        vec![],
        IndexMap::from([
            ("dataDir".into(), Member::State(s::str())),
            ("name".into(), Member::State(s::str())),
            ("version".into(), Member::State(s::str())),
            ("owner".into(), Member::State(s::unknown())),
        ]),
    )
}
pub fn worker_info_contract() -> Result<Contract> {
    Contract::local(
        "chord.app.worker-info",
        1,
        vec![],
        IndexMap::from([
            (
                "address".into(),
                Member::State(s::obj([
                    ("kind", s::required(s::str())),
                    ("key", s::required(s::str())),
                    ("params", s::required(s::unknown())),
                ])),
            ),
            ("start".into(), Member::State(s::unknown())),
        ]),
    )
}
fn info_facet(contract: Contract, values: IndexMap<String, Value>) -> Facet {
    let contract = Arc::new(contract);
    define_facet(move |host| {
        let values = values.clone();
        host.serve(
            contract.clone(),
            move |_, _, self_| {
                let mut members = Members::new();
                for (name, value) in &values {
                    members.state(name, self_.writable(JsonValue::from_serde(value.clone()))?)?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    })
}
pub struct AppOptions {
    pub name: String,
    pub version: String,
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub owner: Value,
    pub principal: Value,
    pub core: Vec<PluginFacet>,
    pub plugins: Vec<PluginScript>,
    pub surfaces: Vec<Surface>,
    pub files: FilesCaps,
    pub workers: Vec<AppWorkerKind>,
    /// Application profile schema for the local Users facet. Defaults to any JSON value.
    pub users_profile: JsonValue,
}
impl AppOptions {
    pub fn new(name: &str, data_dir: PathBuf, owner: Value, principal: Value) -> Self {
        Self {
            name: name.into(),
            version: "1".into(),
            data_dir,
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            owner,
            principal,
            core: vec![],
            plugins: vec![],
            surfaces: vec![],
            files: FilesCaps::default(),
            workers: vec![],
            users_profile: s::unknown(),
        }
    }
}
pub struct AppServer {
    pub host: Host,
    pub http: Http,
    pub layout: AppLayout,
    pub store: StoreEngine,
    pub files: Files,
    pub sign_ins: SignIns,
    pub plugins: PluginRuntime,
    pub address: SocketAddr,
    pub owner_link: String,
    listener: Option<HttpServer>,
    _lock: ProcessLock,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) workers: Arc<WorkerOwners>,
}
pub async fn start_server(options: AppOptions) -> Result<AppServer> {
    crate::contracts::check_name(&options.name, "app name")?;
    if options.owner == options.principal || options.owner.is_null() {
        return Err(Error::Invalid(
            "owner must differ from server principal".into(),
        ));
    }
    let layout = AppLayout::create(&options.data_dir)?;
    let lock = ProcessLock::try_acquire(&layout.data_dir.join("server.lock"))?
        .ok_or_else(|| Error::Invalid("server data directory is already in use".into()))?;
    let db = SqliteDatabase::open(&layout.server_store, Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let host = create_host(HostOptions {
        principal: Some(options.principal.clone()),
        address: Some(JsonValue::from_serde(
            json!({"kind":"server","key":"server","params":{}}),
        )),
        store: Some(store.clone()),
        ..Default::default()
    })?;
    let result = async {
        let workers = WorkerOwners::new(
            options.workers,
            layout.clone(),
            options.principal,
            options.files,
            &host,
        )?;
        let sign_ins = SignIns::open(
            store.clone(),
            options.owner.clone(),
            secret_file(&layout.cookie_secret)?,
        )
        .await?;
        sign_ins.owner_token_file(&layout.owner_token).await?;
        let owner_link = sign_ins.owner_link();
        let files = Files::open(store.clone(), layout.server.join("files"), options.files).await?;
        let mut core = options.core;
        core.push(PluginFacet::new("chord.app.files", files.facet()?));
        let (_, users) = define_users(options.users_profile)?;
        core.push(PluginFacet::new("chord.app.users", users));
        core.push(PluginFacet::new("chord.app.sign-ins", sign_ins.facet()?));
        core.push(PluginFacet::new(
            "chord.app.info",
            info_facet(
                app_info_contract()?,
                IndexMap::from([
                    ("dataDir".into(), json!(layout.data_dir)),
                    ("name".into(), json!(options.name)),
                    ("version".into(), json!(options.version)),
                    ("owner".into(), options.owner),
                ]),
            ),
        ));
        for surface in &options.surfaces {
            core.push(PluginFacet::new(
                format!("chord.surface.{}", surface.manifest().base),
                surface.http_facet(false)?,
            ));
        }
        let (plugins, report) =
            PluginRuntime::load(host.clone(), core, options.plugins, &Context::background())
                .await?;
        if !report.failed.is_empty() {
            return Err(Error::Invalid(format!(
                "server facets failed: {:?}",
                report.failed
            )));
        }
        let opened = sign_ins.clone();
        let router = Arc::downgrade(&workers);
        let http = Http::new(
            host.clone(),
            HttpOptions {
                opened: Arc::new(move |request| opened.opened(request)),
                remote: Arc::new(move |principal| match router.upgrade() {
                    Some(router) => router.remote_options(principal),
                    None => RemoteOptions::new(principal),
                }),
                ..Default::default()
            },
        );
        http.contribute(
            sign_ins
                .contribution()
                .map_err(|e| Error::Invalid(e.to_string()))?,
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
        let listener = http
            .listen(options.listen)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok((
            sign_ins, owner_link, files, plugins, http, listener, workers,
        ))
    }
    .await;
    match result {
        Ok((sign_ins, owner_link, files, plugins, http, listener, workers)) => Ok(AppServer {
            host,
            http,
            layout,
            store,
            files,
            sign_ins,
            plugins,
            address: listener.address,
            owner_link,
            listener: Some(listener),
            _lock: lock,
            name: options.name,
            version: options.version,
            workers,
        }),
        Err(error) => {
            let _ = host.close(&Context::background()).await;
            let _ = db.close().await;
            Err(error)
        }
    }
}
impl AppServer {
    pub async fn export_server(&self) -> Result<Vec<u8>> {
        pack_data(
            ArchiveManifest {
                format: 2,
                app: self.name.clone(),
                api: self.version.clone(),
                address: ArchiveAddress {
                    kind: "server".into(),
                    key: "server".into(),
                },
            },
            &self.store,
            self.files.directory(),
            None,
        )
        .await
    }
    pub async fn close(mut self) -> Result<()> {
        let listener = if let Some(listener) = self.listener.take() {
            listener
                .close()
                .await
                .map_err(|e| Error::Invalid(e.to_string()))
        } else {
            Ok(())
        };
        self.workers.close().await;
        let host = self.host.close(&Context::background()).await;
        self.sign_ins.close();
        let store = self.store.database().close().await;
        listener.and(host).and(store)
    }
}
pub struct WorkerAppOptions {
    pub directory: PathBuf,
    pub principal: Value,
    pub server_principal: Value,
    pub core: Vec<PluginFacet>,
    pub files: FilesCaps,
}
pub struct WorkerApp {
    pub host: Host,
    pub store: StoreEngine,
    pub files: Files,
    pub plugins: PluginRuntime,
    pub session: WorkerSession,
    pub parent: RemotePeer,
}
/// Start a worker with the kit configuration sent by AppServer's native supervisor.
pub async fn boot_worker(mut session: WorkerSession, core: Vec<PluginFacet>) -> Result<WorkerApp> {
    let boot: WorkerBoot = serde_json::from_value(std::mem::take(&mut session.start))?;
    session.start = boot.app;
    let (max_file, max_total, max_uploads) = boot.files;
    start_worker(
        session,
        WorkerAppOptions {
            directory: boot.directory,
            principal: boot.principal,
            server_principal: boot.server_principal,
            core,
            files: FilesCaps {
                max_file,
                max_total,
                max_uploads,
            },
        },
    )
    .await
}
pub async fn start_worker(session: WorkerSession, options: WorkerAppOptions) -> Result<WorkerApp> {
    start_worker_with_host(session, options, HostOptions::default()).await
}

/// Start a worker with the application's principal and reference policies.
pub async fn start_worker_with_host(
    session: WorkerSession,
    options: WorkerAppOptions,
    host_options: HostOptions,
) -> Result<WorkerApp> {
    start_worker_with(session, options, host_options, async |_| Ok(true)).await
}

/// Initialize the application's durable state on the worker store before reporting Ready.
pub async fn start_worker_with(
    session: WorkerSession,
    options: WorkerAppOptions,
    host_options: HostOptions,
    before_ready: impl AsyncFnOnce(&StoreEngine) -> Result<bool>,
) -> Result<WorkerApp> {
    start_worker_with_setup(
        session,
        options,
        host_options,
        |_, _, _| Ok(()),
        before_ready,
    )
    .await
}

/// Like `start_worker_with`, with a synchronous setup hook invoked after connecting to the
/// parent peer and opening worker files, before loading plugin facets.
pub async fn start_worker_with_setup(
    mut session: WorkerSession,
    options: WorkerAppOptions,
    host_options: HostOptions,
    before_load: impl FnOnce(&Host, &RemotePeer, &Files) -> Result<()>,
    before_ready: impl AsyncFnOnce(&StoreEngine) -> Result<bool>,
) -> Result<WorkerApp> {
    private_directory(&options.directory)?;
    let path = options.directory.join("store.sqlite");
    private_store(&path)?;
    let db = SqliteDatabase::open(path, Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let host = create_host(HostOptions {
        principal: Some(options.principal),
        address: Some(JsonValue::from_serde(session.address.clone())),
        store: Some(store.clone()),
        ..host_options
    })?;
    let result: Result<_> = async {
        let mut parent_options = RemoteOptions::new(options.server_principal.clone());
        parent_options.trusted = true;
        let parent = host.connect(session.parent.clone(), parent_options)?;
        let files = Files::open(
            store.clone(),
            options.directory.join("files"),
            options.files,
        )
        .await?;
        before_load(&host, &parent, &files)?;
        let mut core = options.core;
        core.push(PluginFacet::new("chord.app.files", files.facet()?));
        core.push(PluginFacet::new(
            "chord.app.data",
            host_data_facet(
                store.clone(),
                files.directory().to_owned(),
                options.server_principal,
                session.address.clone(),
            )?,
        ));
        core.push(PluginFacet::new(
            "chord.app.worker-info",
            info_facet(
                worker_info_contract()?,
                IndexMap::from([
                    ("address".into(), session.address.clone()),
                    ("start".into(), session.start.clone()),
                ]),
            ),
        ));
        let scripts = session
            .scripts
            .iter()
            .map(|s| PluginScript::wasm(&s.name, s.bytes.clone(), false))
            .collect();
        let (plugins, _) =
            PluginRuntime::load(host.clone(), core, scripts, &Context::background()).await?;
        let idle = before_ready(&store).await?;
        let activity = host.activity().value();
        session
            .report(Control::Ready {
                idle: idle && activity.references + activity.jobs + activity.lingering == 0,
            })
            .await?;
        Ok((parent, files, plugins))
    }
    .await;
    match result {
        Ok((parent, files, plugins)) => Ok(WorkerApp {
            host,
            store,
            files,
            plugins,
            session,
            parent,
        }),
        Err(error) => {
            let _ = host.close(&Context::background()).await;
            let _ = db.close().await;
            let _ = session.report_start_failure(error.to_string()).await;
            Err(error)
        }
    }
}
impl WorkerApp {
    pub async fn run(mut self) -> Result<()> {
        self.serve_with_idle(|| true).await?;
        self.host.close(&Context::background()).await?;
        self.store.database().close().await
    }

    /// Drive worker controls while reporting both Chord and application activity.
    pub async fn serve_with_idle(&mut self, is_idle: impl Fn() -> bool) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        let mut idle = None;
        loop {
            tokio::select! {
                control=self.session.controls.recv()=>match control {
                    None|Some(Control::Shutdown)=>break,
                    Some(Control::Reload { scripts }) => {
                        let valid = scripts.iter().all(|script| script.validate().is_ok());
                        let control = if valid {
                            let scripts = scripts.into_iter().map(|script| PluginScript::wasm(script.name, script.bytes, false)).collect();
                            match self.plugins.reload(scripts, &Context::background()).await {
                                Ok(report) if report.failed.is_empty() => Control::Reloaded,
                                _ => Control::ReloadFailed { message: "worker plugin reload failed".into() },
                            }
                        } else {
                            Control::ReloadFailed { message: "worker plugin digest mismatch".into() }
                        };
                        self.session.report(control).await?;
                    },
                    _ => {}
                },
                _ = interval.tick() => {
                    if self.session.parent.closed() {
                        break;
                    }
                    let activity = self.host.activity().value();
                    let next = is_idle() && activity.references + activity.jobs + activity.lingering == 0;
                    if idle != Some(next) {
                        idle = Some(next);
                        self.session.report(Control::Idle { idle: next }).await?;
                    }
                }
            }
        }
        Ok(())
    }
}
