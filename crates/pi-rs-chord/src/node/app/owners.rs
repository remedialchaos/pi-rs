//! Address-scoped worker ownership, dispatch and app data transfer.
use super::*;
use crate::{
    context::{Context, lock},
    contracts::{Address, Contract, HostKind, Member, schema as s},
    data::ChordError,
    host::{Caller, Facet, Host, Members, ServeOptions, WeakHost, define_facet},
    json::JsonValue,
    node::workers::{DeliveredPlugin, ProcessLock, WorkerHandle, WorkerOptions, Workers},
    remote::{LinkRole, RemoteOptions, RemotePeer, RemoteReference},
    sqlite::{Options, SqliteDatabase},
    store::{HostDataExisting, StoreEngine},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

type WorkerStart = Arc<
    dyn Fn(Address, Context) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync,
>;
type Authorize = Arc<dyn Fn(&Value, &Address) -> bool + Send + Sync>;
type WorkerPrincipal = Arc<dyn Fn(&Address) -> Value + Send + Sync>;

/// Startup, routing, and authorization of one application-defined worker kind.
pub struct AppWorkerKind {
    pub kind: HostKind,
    pub process: WorkerOptions,
    pub start: WorkerStart,
    pub authorize: Authorize,
    pub principal: WorkerPrincipal,
    pub contracts: Vec<Arc<Contract>>,
    pub plugins: Vec<DeliveredPlugin>,
}
impl AppWorkerKind {
    pub fn new(
        kind: HostKind,
        process: WorkerOptions,
        principal: impl Fn(&Address) -> Value + Send + Sync + 'static,
        authorize: impl Fn(&Value, &Address) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            process,
            start: Arc::new(|_, _| Box::pin(async { Ok(Value::Null) })),
            authorize: Arc::new(authorize),
            principal: Arc::new(principal),
            contracts: vec![],
            plugins: vec![],
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct WorkerBoot {
    pub directory: PathBuf,
    pub principal: Value,
    pub server_principal: Value,
    pub files: (u64, u64, usize),
    pub app: Value,
}
struct Kind {
    config: AppWorkerKind,
    supervisor: Workers,
}
struct Running {
    handle: Arc<WorkerHandle>,
    peer: RemotePeer,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.peer.close();
    }
}
type AddressSlot = tokio::sync::Mutex<Option<Running>>;

pub(super) struct WorkerOwners {
    kinds: BTreeMap<String, Kind>,
    slots: Mutex<BTreeMap<String, Weak<AddressSlot>>>,
    // Keep live ownership across calls until idle exit or explicit retirement.
    active: Mutex<BTreeMap<String, Arc<AddressSlot>>>,
    layout: AppLayout,
    server: Value,
    files: FilesCaps,
    host: WeakHost,
    contracts: BTreeMap<String, Arc<Contract>>,
    closed: AtomicBool,
}
impl WorkerOwners {
    pub(super) fn new(
        configs: Vec<AppWorkerKind>,
        layout: AppLayout,
        server: Value,
        files: FilesCaps,
        host: &Host,
    ) -> Result<Arc<Self>> {
        let mut kinds = BTreeMap::new();
        let mut contracts: BTreeMap<String, Arc<Contract>> = BTreeMap::new();
        let locks = layout.worker_bases.join("locks");
        private_directory(&locks)?;
        for mut config in configs {
            let name = config.kind.kind.clone();
            safe_relative(&name)?;
            if name == "server" || name.contains('/') || kinds.contains_key(&name) {
                return Err(Error::Invalid(format!(
                    "invalid or duplicate worker kind {name}"
                )));
            }
            config.process.lock_dir = locks.join(&name);
            let supervisor = Workers::new(config.process.clone())?;
            for contract in &config.contracts {
                let name = contract.name();
                if name == "chord.app.data"
                    || contract.kind() != crate::contracts::ContractKind::Remote
                {
                    return Err(Error::Invalid(format!("contract {name} is not routable")));
                }
                if let Some(previous) = contracts.insert(name.into(), contract.clone())
                    && previous.fingerprint() != contract.fingerprint()
                {
                    return Err(Error::Invalid(format!(
                        "conflicting worker contract for {name}"
                    )));
                }
            }
            kinds.insert(name, Kind { config, supervisor });
        }
        Ok(Arc::new(Self {
            kinds,
            slots: Mutex::default(),
            active: Mutex::default(),
            layout,
            server,
            files,
            host: host.downgrade(),
            contracts,
            closed: AtomicBool::new(false),
        }))
    }
    fn locate(&self, address: &Address) -> Result<(&Kind, Address)> {
        let kind = self
            .kinds
            .get(&address.kind)
            .ok_or_else(|| Error::Invalid(format!("no worker kind {}", address.kind)))?;
        if kind
            .config
            .kind
            .key_of_address(&serialized(address))?
            .as_deref()
            != Some(&address.key)
        {
            return Err(Error::Invalid("invalid worker address".into()));
        }
        Ok((kind, kind.config.kind.address_of_key(&address.key)?))
    }
    fn slot(&self, address: &Address) -> Arc<AddressSlot> {
        let mut slots = lock(&self.slots);
        if let Some(slot) = slots.get(&address.key).and_then(Weak::upgrade) {
            return slot;
        }
        slots.retain(|_, slot| slot.strong_count() != 0);
        let slot = Arc::new(AddressSlot::default());
        slots.insert(address.key.clone(), Arc::downgrade(&slot));
        slot
    }
    pub(super) fn contract(&self, kind: &str, name: &str) -> Option<Arc<Contract>> {
        self.kinds
            .get(kind)?
            .config
            .contracts
            .iter()
            .find(|contract| contract.name() == name)
            .cloned()
    }
    pub(super) fn remote_options(self: &Arc<Self>, principal: Value) -> RemoteOptions {
        let mut options = RemoteOptions::new(principal);
        options.route_contracts.clone_from(&self.contracts);
        let weak = Arc::downgrade(self);
        options.route = Some(Arc::new(move |request| {
            let owners = weak.upgrade();
            Box::pin(async move {
                if request.address_kind == "server" {
                    return Ok(None);
                }
                let owners = owners.ok_or(Error::Closed)?;
                let contract = owners
                    .contract(&request.address_kind, request.contract.name())
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
                owners
                    .hold(contract, request.params, request.caller, &request.context)
                    .await
                    .map(Some)
            })
        }));
        options
    }
    pub(super) async fn hold(
        self: &Arc<Self>,
        contract: Arc<Contract>,
        params: JsonValue,
        caller: Caller,
        ctx: &Context,
    ) -> Result<RemoteReference> {
        crate::contracts::check_params(contract.params(), &params, "params")?;
        let (kind, address) = self.locate(&contract.host_address(&params)?)?;
        let principal = caller
            .principal
            .as_ref()
            .ok_or_else(|| ChordError::new("forbidden", "worker requires a principal"))?;
        if principal != &self.server && !(kind.config.authorize)(principal, &address) {
            return Err(ChordError::new("forbidden", "worker address denied").into());
        }
        let slot = self.slot(&address);
        let mut current = slot.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if current
            .as_ref()
            .is_none_or(|running| running.handle.link.closed())
        {
            let worker_principal = (kind.config.principal)(&address);
            crate::data::principal_key(&worker_principal)?;
            if worker_principal == self.server {
                return Err(Error::Invalid(
                    "worker principal cannot be the server principal".into(),
                ));
            }
            let start = WorkerBoot {
                directory: self.layout.worker_directory(&address.kind, &address.key)?,
                principal: worker_principal.clone(),
                server_principal: self.server.clone(),
                files: (
                    self.files.max_file,
                    self.files.max_total,
                    self.files.max_uploads,
                ),
                app: (kind.config.start)(address.clone(), ctx.clone()).await?,
            };
            let handle = kind
                .supervisor
                .demand(
                    serialized(&address).to_serde()?,
                    serde_json::to_value(start)?,
                    kind.config.plugins.clone(),
                )
                .await?;
            if handle.link.closed() {
                handle.wait_ready().await?;
                return Err(Error::Closed);
            }
            let mut options = self.remote_options(worker_principal);
            options.trusted = true;
            options.role = LinkRole::Child;
            let peer = RemotePeer::serve(
                self.host.upgrade().ok_or(Error::Closed)?,
                handle.link.clone(),
                options,
            )?;
            *current = Some(Running {
                handle: handle.clone(),
                peer,
            });
            lock(&self.active).insert(address.key.clone(), slot.clone());
            let weak = Arc::downgrade(self);
            let key = address.key.clone();
            let exited = slot.clone();
            tokio::spawn(async move {
                handle.wait_exit().await;
                let mut current = exited.lock().await;
                if current
                    .as_ref()
                    .is_some_and(|running| Arc::ptr_eq(&running.handle, &handle))
                {
                    *current = None;
                    if let Some(owners) = weak.upgrade() {
                        lock(&owners.active).remove(&key);
                    }
                }
            });
        }
        let running = current.as_ref().ok_or(Error::Closed)?;
        running.handle.wait_ready().await?;
        running.peer.hold_as(contract, params, caller, ctx)
    }
    async fn retire(
        &self,
        kind: &Kind,
        address: &Address,
        current: &mut Option<Running>,
    ) -> Result<ProcessLock> {
        current.take();
        lock(&self.active).remove(&address.key);
        let wire = serialized(address).to_serde()?;
        kind.supervisor.retire(&wire).await?;
        ProcessLock::acquire(&kind.supervisor.lock_path(&wire)?, Duration::from_secs(3)).await
    }
    pub(super) async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for kind in self.kinds.values() {
            kind.supervisor.close().await;
        }
        lock(&self.active).clear();
    }
}
fn serialized(address: &Address) -> JsonValue {
    JsonValue::Object(indexmap::IndexMap::from([
        (
            "kind".into(),
            JsonValue::String(address.kind.as_str().into()),
        ),
        ("params".into(), address.params.clone()),
        ("key".into(), JsonValue::String(address.key.as_str().into())),
    ]))
}

/// Worker-only control contract. The server principal is checked on hold and call.
pub fn host_data_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(Contract::remote(
        "chord.app.data",
        1,
        vec![("address".into(), s::str())],
        indexmap::IndexMap::from([(
            "main".into(),
            indexmap::IndexMap::from([(
                "exportTo".into(),
                Member::Method {
                    args: s::obj([
                        ("path", s::required(s::str())),
                        ("manifest", s::required(s::str())),
                        ("server", s::required(s::str())),
                    ]),
                    result: s::unknown(),
                },
            )]),
        )]),
        Arc::new(|params| {
            let params = params.to_serde()?;
            address_from_value(serde_json::from_str(
                params["address"]
                    .as_str()
                    .ok_or_else(|| Error::Invalid("missing address".into()))?,
            )?)
        }),
    )?))
}
fn address_from_value(value: Value) -> Result<Address> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("missing address kind".into()))?;
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("missing address key".into()))?;
    let params = value
        .get("params")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::Invalid("missing address params".into()))?;
    Ok(Address {
        kind: kind.into(),
        key: key.into(),
        params: JsonValue::from_serde(params.clone()),
    })
}
pub(super) fn host_data_facet(
    store: StoreEngine,
    files: PathBuf,
    server: Value,
    address: Value,
) -> Result<Facet> {
    let contract = host_data_contract()?;
    Ok(define_facet(move |host| {
        let (store, files, server, address) = (
            store.clone(),
            files.clone(),
            server.clone(),
            address.clone(),
        );
        host.serve(
            contract.clone(),
            move |params, who, _| {
                if !crate::data::same_principal(who, &server)? {
                    return Err(ChordError::new("forbidden", "HostData is server-only").into());
                }
                if contract_address(params)? != address_from_value(address.clone())? {
                    return Err(Error::Invalid("HostData address mismatch".into()));
                }
                let mut members = Members::variant("main");
                let (store, files, server) = (store.clone(), files.clone(), server.clone());
                members.method("exportTo", move |args, ctx| {
                    let (store, files, server) = (store.clone(), files.clone(), server.clone());
                    Box::pin(async move {
                        if !crate::data::same_principal(ctx.principal.as_ref(), &server)? {
                            return Err(
                                ChordError::new("forbidden", "HostData is server-only").into()
                            );
                        }
                        #[derive(Deserialize)]
                        struct Args {
                            path: PathBuf,
                            manifest: String,
                            server: PathBuf,
                        }
                        let args: Args = serde_json::from_value(args.to_serde()?)?;
                        pack_data_to(
                            serde_json::from_str(&args.manifest)?,
                            &store,
                            &files,
                            &args.server,
                            &args.path,
                        )
                        .await?;
                        Ok(JsonValue::Null)
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
fn contract_address(params: &JsonValue) -> Result<Address> {
    host_data_contract()?.host_address(params)
}

/// A private directory on the same filesystem as its eventual destination.
struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn occupied(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(Error::Invalid(
            "worker data is not a private directory".into(),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

impl AppServer {
    pub async fn hold_worker(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        context: &Context,
    ) -> Result<RemoteReference> {
        let caller = Caller {
            principal: Some(self.workers.server.clone()),
            incarnation: self.host.incarnation().into(),
        };
        self.workers.hold(contract, params, caller, context).await
    }
    pub async fn export_data(&self, address: &Address, target: &Path, ctx: &Context) -> Result<()> {
        let (kind, address) = self.workers.locate(address)?;
        let slot = self.workers.slot(&address);
        let current = slot.lock().await;
        let directory = self.layout.worker_directory(&address.kind, &address.key)?;
        let manifest = ArchiveManifest {
            format: 2,
            app: self.name.clone(),
            api: self.version.clone(),
            address: ArchiveAddress {
                kind: address.kind.clone(),
                key: address.key.clone(),
            },
        };
        let target = std::path::absolute(target)?;
        let parent = target
            .parent()
            .ok_or_else(|| Error::Invalid("archive target has no parent".into()))?;
        let scratch = Stage(parent.join(format!(".chord-server-{}", new_token(16))));
        private_directory(&scratch.0)?;
        let server = scratch.0.join("server.archive");
        private_store(&server)?;
        std::fs::write(&server, self.host.export_host_data(&address).await?)?;
        if let Some(running) = current
            .as_ref()
            .filter(|running| !running.handle.link.closed())
        {
            running.handle.wait_ready().await?;
            let params = JsonValue::from_serde(json!({"address":serialized(&address).to_json()?}));
            let reference = running.peer.hold(host_data_contract()?, params, ctx)?;
            let args = JsonValue::from_serde(
                json!({"path":target,"manifest":serde_json::to_string(&manifest)?,"server":server}),
            );
            let result = async {
                reference.wait(ctx).await?;
                reference.call("exportTo", args, ctx).await?;
                Ok(())
            }
            .await;
            reference.release();
            return result;
        }
        let _process = ProcessLock::acquire(
            &kind
                .supervisor
                .lock_path(&serialized(&address).to_serde()?)?,
            Duration::from_secs(3),
        )
        .await?;
        if !occupied(&directory)? || !directory.join("store.sqlite").is_file() {
            return Err(Error::Invalid("worker address has no data".into()));
        }
        let db = SqliteDatabase::open(directory.join("store.sqlite"), Options::default()).await?;
        let result = async {
            let store = StoreEngine::open(db.clone()).await?;
            pack_data_to(manifest, &store, &directory.join("files"), &server, &target).await
        }
        .await;
        let closed = db.close().await;
        result.and(closed)
    }
    /// Stage and validate a format-2 archive, then retire the active owner and replace the address.
    pub async fn import_data(
        &self,
        archive: &Path,
        address: &Address,
        existing: HostDataExisting,
        limits: ArchiveLimits,
    ) -> Result<()> {
        let (kind, address) = self.workers.locate(address)?;
        let directory = self.layout.worker_directory(&address.kind, &address.key)?;
        let parent = directory
            .parent()
            .ok_or_else(|| Error::Invalid("worker directory has no parent".into()))?;
        private_directory(parent)?;
        let stage = Stage(parent.join(format!(".chord-import-{}", new_token(16))));
        let manifest =
            unpack_staging_from(archive, &self.name, &address.kind, &stage.0, limits).await?;
        let source = kind.config.kind.address_of_key(&manifest.address.key)?;
        let server_archive = stage.0.join("server.archive");
        let prepared =
            self.host
                .prepare_host_data(&source, &address, &std::fs::read(&server_archive)?)?;
        std::fs::remove_file(server_archive)?;
        let slot = self.workers.slot(&address);
        let mut current = slot.lock_owned().await;
        if existing == HostDataExisting::Refuse && (occupied(&directory)? || current.is_some()) {
            return Err(Error::Invalid("worker address already owns data".into()));
        }
        let _process = self.workers.retire(kind, &address, &mut current).await?;
        if existing == HostDataExisting::Refuse && occupied(&directory)? {
            return Err(Error::Invalid("worker address already owns data".into()));
        }
        // Own the lock and both rollback paths in the task even if the caller drops the future.
        let host = self.host.clone();
        tokio::spawn(async move {
            let (_process, _permit) = (_process, current);
            let aside = parent_path(&directory)?.join(format!(".chord-aside-{}", new_token(16)));
            let had_data = occupied(&directory)?;
            if had_data {
                std::fs::rename(&directory, &aside)?;
            }
            if let Err(error) = std::fs::rename(&stage.0, &directory) {
                if had_data {
                    std::fs::rename(&aside, &directory)?;
                }
                return Err(error.into());
            }
            if let Err(error) = host.import_host_data(prepared, existing).await {
                std::fs::rename(&directory, &stage.0)?;
                if had_data {
                    std::fs::rename(&aside, &directory)?;
                }
                return Err(error);
            }
            if had_data {
                let _ = std::fs::remove_dir_all(aside);
            }
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
    }
    /// Cancel just this address's worker and remove its server and worker data.
    pub async fn delete_data(&self, address: &Address) -> Result<()> {
        let (kind, address) = self.workers.locate(address)?;
        let slot = self.workers.slot(&address);
        let mut current = slot.clone().lock_owned().await;
        let process = self.workers.retire(kind, &address, &mut current).await?;
        let directory = self.layout.worker_directory(&address.kind, &address.key)?;
        let host = self.host.clone();
        tokio::spawn(async move {
            let (_process, _permit) = (process, current);
            let has_data = occupied(&directory)?;
            let aside = parent_path(&directory)?.join(format!(".chord-deleted-{}", new_token(16)));
            if has_data {
                std::fs::rename(&directory, &aside)?;
            }
            if let Err(error) = host.delete_host_data(&address).await {
                if has_data {
                    std::fs::rename(&aside, &directory)?;
                }
                return Err(error);
            }
            if has_data {
                let _ = std::fs::remove_dir_all(aside);
            }
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
    }
}
fn parent_path(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::Invalid("worker directory has no parent".into()))
}
