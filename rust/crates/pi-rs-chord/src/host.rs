//! Portable facet hosting, explicit references and staged atomic reloads.
use crate::{
    Error, Result,
    context::{AbortSubscription, Cancel, Context, lock},
    contracts::{AcceptedVersion, AcceptsTable, Contract, ContractKind},
    data::{ChordError, FailureCause, FailureKind},
    json::JsonValue,
    state::{Batch, NativeSource, Source, Subscription},
    store::StoreEngine,
    sync::SyncFuture,
};
use indexmap::IndexMap;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;
mod declarations;
mod lifecycle;
mod reload_cycle;
mod staging;
pub use crate::jobs::{next_delay, uuid_v7};
pub use declarations::*;
pub use reload_cycle::{CycleWait, ReloadCycles};
#[derive(Clone, Debug, PartialEq)]
pub enum RefState {
    Loading,
    Live,
    Failed {
        cause: FailureCause,
        retry_at: Option<u64>,
    },
    Ended {
        error: ChordError,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivityCounts {
    pub references: usize,
    pub calls: usize,
    pub jobs: usize,
    pub lingering: usize,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Presence {
    pub params: JsonValue,
    pub principal: Option<serde_json::Value>,
    pub incarnation: String,
}
#[derive(Clone)]
pub struct HostOptions {
    pub principal: Option<serde_json::Value>,
    pub address: Option<JsonValue>,
    pub store: Option<StoreEngine>,
    pub close_timeout: Duration,
    pub ready_timeout: Duration,
    pub reload_drain_timeout: Duration,
    pub on_error: Arc<dyn Fn(HostError) + Send + Sync>,
}
impl Default for HostOptions {
    fn default() -> Self {
        Self {
            principal: None,
            address: None,
            store: None,
            close_timeout: Duration::from_secs(30),
            ready_timeout: Duration::from_secs(60),
            reload_drain_timeout: Duration::from_secs(30),
            on_error: Arc::new(|_| {}),
        }
    }
}
#[derive(Clone, Debug)]
pub struct HostError {
    pub kind: String,
    pub plugin: Option<String>,
    pub object: Option<String>,
    pub error: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginFailure {
    pub plugin: String,
    pub error: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadReport {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub failed: Vec<PluginFailure>,
    pub objects: Vec<ObjectFate>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    End,
    Retry,
    Restart,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectFate {
    pub object: String,
    pub kind: Fate,
}
type FacetSetup = dyn Fn(&mut FacetHost) -> Result<()> + Send + Sync;
#[derive(Clone)]
pub struct Facet(Arc<FacetSetup>);
impl Facet {
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
pub fn define_facet(setup: impl Fn(&mut FacetHost) -> Result<()> + Send + Sync + 'static) -> Facet {
    Facet(Arc::new(setup))
}
#[derive(Clone)]
pub struct PluginFacet {
    pub name: String,
    pub facet: Facet,
}
impl PluginFacet {
    pub fn new(name: impl Into<String>, facet: Facet) -> Self {
        Self {
            name: name.into(),
            facet,
        }
    }
}
type Factory = Arc<
    dyn Fn(&JsonValue, Option<&serde_json::Value>, &mut ObjectSelf) -> Result<Members>
        + Send
        + Sync,
>;
pub type Admit = Arc<
    dyn Fn(&JsonValue, &serde_json::Value) -> Result<NativeSource<Admission<JsonValue>>>
        + Send
        + Sync,
>;
#[derive(Clone, Debug, PartialEq)]
pub enum Admission<T> {
    Allow { value: T },
    Deny { reason: String },
}
pub fn allow<T>(value: T) -> Admission<T> {
    Admission::Allow { value }
}
pub fn deny<T>(reason: impl Into<String>) -> Admission<T> {
    Admission::Deny {
        reason: reason.into(),
    }
}
#[derive(Clone, Default)]
pub struct ServeOptions {
    pub accepts: AcceptsTable,
    pub admit: Option<Admit>,
    pub linger: Duration,
}
#[derive(Clone)]
struct Served {
    plugin: String,
    contract: Arc<Contract>,
    factory: Factory,
    options: ServeOptions,
    accepted: Vec<AcceptedVersion>,
}
impl Served {
    fn binds(&self, contract: &Contract) -> bool {
        (self.contract.version() == contract.version()
            && self.contract.fingerprint() == contract.fingerprint())
            || self
                .accepted
                .iter()
                .any(|v| v.version == contract.version() && v.fingerprint == contract.fingerprint())
    }
    fn singleton(&self) -> bool {
        self.contract.kind() == ContractKind::Local && self.contract.params().fields.is_empty()
    }
}
#[derive(Clone)]
struct Plugin {
    facet: Facet,
    types: IndexMap<String, Arc<Served>>,
    registries: IndexMap<String, RegistryDeclaration>,
    contributions: IndexMap<String, (crate::registries::Registry<JsonValue, JsonValue>, JsonValue)>,
}
struct HostState {
    loaded: bool,
    closed: bool,
    plugins: IndexMap<String, Plugin>,
    served: IndexMap<String, Arc<Served>>,
    objects: IndexMap<String, Arc<Object>>,
    registries: IndexMap<String, crate::registries::RegistryRecord<JsonValue, JsonValue>>,
    presence: IndexMap<String, NativeSource<Vec<Presence>>>,
    revoked: BTreeSet<String>,
    links: Vec<crate::remote::RemotePeer>,
}
struct HostInner {
    options: HostOptions,
    batch: Batch,
    state: Mutex<HostState>,
    reload: tokio::sync::Mutex<()>,
    activity: NativeSource<ActivityCounts>,
    cycles: ReloadCycles,
    incarnation: String,
    context: Context,
    cancel: Cancel,
    page_states: crate::surfaces::PageStates,
}
#[derive(Clone)]
pub struct Host(Arc<HostInner>);
#[derive(Clone)]
pub(crate) struct WeakHost(Weak<HostInner>);
impl WeakHost {
    pub(crate) fn upgrade(&self) -> Option<Host> {
        self.0.upgrade().map(Host)
    }
}
pub fn create_host(options: HostOptions) -> Result<Host> {
    if let Some(principal) = &options.principal {
        crate::data::principal_key(principal)?;
    }
    let report = options.on_error.clone();
    let batch = options
        .store
        .as_ref()
        .map(StoreEngine::batch)
        .unwrap_or_else(|| {
            Batch::new(move |e| {
                report(HostError {
                    kind: "batch_failed".into(),
                    plugin: None,
                    object: None,
                    error: format!("{e:?}"),
                })
            })
        });
    let activity = NativeSource::new(batch.clone(), ActivityCounts::default(), |a, b| a == b);
    let (context, cancel) = Context::background().with_cancel();
    Ok(Host(Arc::new(HostInner {
        options,
        batch,
        state: Mutex::new(HostState {
            loaded: false,
            closed: false,
            plugins: IndexMap::new(),
            served: IndexMap::new(),
            objects: IndexMap::new(),
            registries: IndexMap::new(),
            presence: IndexMap::new(),
            revoked: BTreeSet::new(),
            links: Vec::new(),
        }),
        reload: tokio::sync::Mutex::new(()),
        activity,
        cycles: ReloadCycles::default(),
        incarnation: uuid_v7()?,
        context,
        cancel,
        page_states: Default::default(),
    })))
}
struct Object {
    admission: Mutex<Option<crate::state::NativeObservation<Admission<JsonValue>>>>,
    job_store: Mutex<Option<crate::jobs::JobStore>>,
    key: String,
    contract: Arc<Contract>,
    params: JsonValue,
    principal: Option<serde_json::Value>,
    host: Weak<HostInner>,
    documents: crate::documents::Documents,
    state: Mutex<ObjectState>,
    changed: watch::Sender<u64>,
    serial: tokio::sync::Mutex<()>,
    holders: Mutex<Vec<Weak<Binding>>>,
}
struct ObjectState {
    served: Option<Arc<Served>>,
    phase: RefState,
    attempt: Option<Arc<Instance>>,
    old: Option<Arc<Instance>>,
    kept_contributions: Option<KeptContributions>,
    generation: u64,
    activating: bool,
    delay: Option<u64>,
    recorded: Recorded,
    holds: usize,
    linger: u64,
}
struct KeptContributions {
    owner: crate::registries::Contributor,
    tokens: Vec<crate::registries::Registry<JsonValue, JsonValue>>,
}
struct Instance {
    id: String,
    served: Arc<Served>,
    members: Members,
    context: Context,
    cancel: Cancel,
    calls: AtomicUsize,
    changed: watch::Sender<usize>,
    dependencies: Mutex<Vec<Held>>,
    jobs: Vec<crate::jobs::Job>,
    doc_attempt: crate::documents::Attempt,
    cleanups: Mutex<Vec<Cleanup>>,
    recorded: Recorded,
    opens_natives: bool,
}
type Cleanup = Box<dyn FnOnce(String) -> SyncFuture<()> + Send>;
struct Binding {
    batch: Batch,
    object: Mutex<Option<Arc<Object>>>,
    contract: Arc<Contract>,
    state: NativeSource<RefState>,
    released: AtomicBool,
    abort: Mutex<Option<AbortSubscription>>,
    caller: Caller,
    members: Mutex<IndexMap<String, Source>>,
    subscriptions: Mutex<Vec<Subscription>>,
    bound_instance: Mutex<Option<String>>,
}
#[derive(Clone)]
pub struct Held(Arc<Binding>);
#[derive(Clone)]
pub struct Caller {
    pub principal: Option<serde_json::Value>,
    pub incarnation: String,
}
struct CallScope {
    host: Weak<HostInner>,
    held: Mutex<Vec<Held>>,
    settled: AtomicBool,
}
#[derive(Clone)]
pub struct Ctx {
    pub context: Context,
    pub principal: Option<serde_json::Value>,
    pub incarnation: String,
    scope: Arc<CallScope>,
    old_chain: Vec<String>,
}
impl Ctx {
    fn new(host: &Host, context: Context, caller: Caller, old_chain: Vec<String>) -> Self {
        Self {
            context,
            principal: caller.principal,
            incarnation: caller.incarnation,
            scope: Arc::new(CallScope {
                host: Arc::downgrade(&host.0),
                held: Mutex::new(Vec::new()),
                settled: AtomicBool::new(false),
            }),
            old_chain,
        }
    }
    pub fn get(&self, contract: Arc<Contract>, params: JsonValue) -> Result<Held> {
        if self.scope.settled.load(Ordering::Acquire) {
            return Err(released("settled", "call settled"));
        }
        let host = Host(self.scope.host.upgrade().ok_or(Error::Closed)?);
        let held = host.hold_as(
            contract,
            params,
            Caller {
                principal: self.principal.clone(),
                incarnation: self.incarnation.clone(),
            },
            &self.context,
        )?;
        lock(&self.scope.held).push(held.clone());
        Ok(held)
    }
    pub async fn call(&self, held: &Held, method: &str, args: JsonValue) -> Result<JsonValue> {
        held.call_with_chain(method, args, self).await
    }
    pub fn settle(&self) {
        if !self.scope.settled.swap(true, Ordering::AcqRel) {
            for held in std::mem::take(&mut *lock(&self.scope.held)) {
                held.release();
            }
        }
    }
}
impl Drop for CallScope {
    fn drop(&mut self) {
        for held in lock(&self.held).drain(..) {
            held.release();
        }
    }
}
impl Host {
    pub(crate) fn keep_peer(&self, peer: crate::remote::RemotePeer) {
        lock(&self.0.state).links.push(peer);
    }
    pub(crate) fn downgrade(&self) -> WeakHost {
        WeakHost(Arc::downgrade(&self.0))
    }
    pub fn batch(&self) -> Batch {
        self.0.batch.clone()
    }
    pub fn page_state<T: Clone + PartialEq + Send + 'static>(
        &self,
        key: &str,
        initial: T,
    ) -> Result<NativeSource<T>> {
        self.0.page_states.get(key, initial, self.batch())
    }
    pub fn incarnation(&self) -> &str {
        &self.0.incarnation
    }
    pub fn activity(&self) -> NativeSource<ActivityCounts> {
        self.0.activity.clone()
    }
    pub fn root(&self) -> Root {
        Root {
            host: self.clone(),
            held: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }
    pub fn references(&self, name: &str) -> NativeSource<Vec<Presence>> {
        let mut state = lock(&self.0.state);
        state
            .presence
            .entry(name.into())
            .or_insert_with(|| NativeSource::new(self.0.batch.clone(), Vec::new(), |a, b| a == b))
            .clone()
    }
    pub fn hold(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        context: &Context,
    ) -> Result<Held> {
        self.hold_as(
            contract,
            params,
            Caller {
                principal: self.0.options.principal.clone(),
                incarnation: self.0.incarnation.clone(),
            },
            context,
        )
    }
    pub async fn get(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        context: &Context,
    ) -> Result<Held> {
        let held = self.hold(contract, params, context)?;
        if let Err(error) = held.wait(context).await {
            held.release();
            return Err(error);
        }
        Ok(held)
    }
    pub fn hold_as(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        caller: Caller,
        context: &Context,
    ) -> Result<Held> {
        self.0.batch.assert_not_computing("host.hold")?;
        let params_key = crate::contracts::check_params(contract.params(), &params, "params")?;
        let principal = if contract.kind() == ContractKind::Remote {
            Some(caller.principal.clone().ok_or_else(|| {
                ChordError::new("forbidden", "remote reference requires a principal")
            })?)
        } else {
            None
        };
        let principal_key = principal
            .as_ref()
            .map(crate::data::principal_key)
            .transpose()?
            .unwrap_or_default();
        let key =
            crate::data::join_key(&[contract.name().into(), params_key, principal_key.clone()]);
        let (object, activate) = {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            if state.revoked.contains(&principal_key) {
                return Err(revoked().into());
            }
            let served = state.served.get(contract.name()).cloned();
            let object = state
                .objects
                .entry(key.clone())
                .or_insert_with(|| {
                    let (changed, _) = watch::channel(0);
                    Arc::new(Object {
                        admission: Mutex::new(None),
                        job_store: Mutex::new(None),
                        key: key.clone(),
                        contract: contract.clone(),
                        params: params.clone(),
                        principal: principal.clone(),
                        host: Arc::downgrade(&self.0),
                        documents: crate::documents::Documents::new(
                            contract.name(),
                            self.0.batch.clone(),
                            self.0
                                .options
                                .store
                                .clone()
                                .filter(|_| {
                                    contract.kind() == ContractKind::Local
                                        && contract.params().fields.is_empty()
                                })
                                .map(|s| {
                                    crate::documents::DocumentStore::from_engine(s, contract.name())
                                }),
                            |_| {},
                        ),
                        state: Mutex::new(ObjectState {
                            served,
                            phase: RefState::Loading,
                            attempt: None,
                            old: None,
                            kept_contributions: None,
                            generation: 0,
                            activating: false,
                            delay: None,
                            recorded: Recorded::default(),
                            holds: 0,
                            linger: 0,
                        }),
                        changed,
                        serial: tokio::sync::Mutex::new(()),
                        holders: Mutex::new(Vec::new()),
                    })
                })
                .clone();
            (object, state.loaded)
        };
        {
            let mut state = lock(&object.state);
            state.holds += 1;
            state.linger += 1;
        }
        let held = Held(Arc::new(Binding {
            batch: self.batch(),
            state: NativeSource::new(self.0.batch.clone(), object.phase_for(&contract), |a, b| {
                a == b
            }),
            object: Mutex::new(Some(object.clone())),
            contract,
            released: AtomicBool::new(false),
            abort: Mutex::new(None),
            caller,
            members: Mutex::new(IndexMap::new()),
            subscriptions: Mutex::new(Vec::new()),
            bound_instance: Mutex::new(None),
        }));
        lock(&object.holders).push(Arc::downgrade(&held.0));
        if let Some(signal) = context.abort_signal() {
            let weak = Arc::downgrade(&held.0);
            let subscription = signal.subscribe(move |_| {
                if let Some(binding) = weak.upgrade() {
                    Held(binding).release();
                }
            });
            *lock(&held.0.abort) = Some(subscription);
        }
        self.publish_presence(object.contract.name())?;
        self.publish_activity()?;
        if activate {
            self.activate(object)?;
        }
        Ok(held)
    }
    fn publish_activity(&self) -> Result<()> {
        let objects = lock(&self.0.state)
            .objects
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut counts = ActivityCounts::default();
        for object in objects {
            let state = lock(&object.state);
            counts.references += state.holds;
            counts.lingering += usize::from(
                state.holds == 0
                    && !matches!(state.phase, RefState::Ended { .. })
                    && state.served.as_ref().is_some_and(|s| !s.singleton()),
            );
            for instance in state.attempt.iter().chain(state.old.iter()) {
                counts.calls += instance.calls.load(Ordering::Acquire);
                counts.jobs += instance
                    .jobs
                    .iter()
                    .map(|j| {
                        j.running()
                            .iter()
                            .filter(|r| r.phase != crate::jobs::Phase::Blocked)
                            .count()
                    })
                    .sum::<usize>();
            }
        }
        self.0.activity.set(counts, &Context::background())
    }
    fn publish_presence(&self, name: &str) -> Result<()> {
        let objects = lock(&self.0.state)
            .objects
            .values()
            .filter(|o| o.contract.name() == name)
            .cloned()
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        for object in objects {
            for holder in lock(&object.holders).iter().filter_map(Weak::upgrade) {
                if !holder.released.load(Ordering::Acquire) {
                    entries.push(Presence {
                        params: object.params.clone(),
                        principal: if object.contract.kind() == ContractKind::Remote {
                            holder.caller.principal.clone()
                        } else {
                            None
                        },
                        incarnation: holder.caller.incarnation.clone(),
                    });
                }
            }
        }
        self.references(name).set(entries, &Context::background())
    }
    pub fn registry(
        &self,
        token: crate::registries::Registry<JsonValue, JsonValue>,
    ) -> crate::registries::RegistryRecord<JsonValue, JsonValue> {
        let mut state = lock(&self.0.state);
        state
            .registries
            .entry(token.name.clone())
            .or_insert_with(|| {
                crate::registries::RegistryRecord::new(token, self.0.batch.clone(), |_| {})
            })
            .clone()
    }
}
impl Object {
    fn phase_for(&self, contract: &Contract) -> RefState {
        let state = lock(&self.state);
        if let Some(served) = &state.served
            && !served.binds(contract)
            && !matches!(state.phase, RefState::Ended { .. })
        {
            return RefState::Failed {
                cause: FailureCause {
                    kind: FailureKind::Waiting,
                    error: ChordError::new(
                        "contract_mismatch",
                        format!("{} v{} is not served", contract.name(), contract.version()),
                    ),
                },
                retry_at: None,
            };
        }
        state.phase.clone()
    }
    fn moved(&self) -> Result<()> {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        let holders = lock(&self.holders)
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for holder in holders {
            if !holder.released.load(Ordering::Acquire) {
                let phase = self.phase_for(&holder.contract);
                if matches!(phase, RefState::Live) {
                    let instance = lock(&self.state).attempt.clone();
                    if let Some(instance) = instance {
                        holder.forward(&instance)?;
                    }
                } else if let RefState::Ended { error } = &phase {
                    lock(&holder.subscriptions).clear();
                    let sources = lock(&holder.members).values().cloned().collect::<Vec<_>>();
                    for source in sources {
                        source.detach(error.to_string());
                    }
                } else if let RefState::Failed { cause, .. } = &phase {
                    lock(&holder.subscriptions).clear();
                    let sources = lock(&holder.members).values().cloned().collect::<Vec<_>>();
                    for source in sources {
                        source.fail(cause.error.to_string(), &Context::background())?;
                    }
                }
                holder.state.set(phase, &Context::background())?;
            }
        }
        Ok(())
    }
}
impl Held {
    fn inert(contract: Arc<Contract>, batch: Batch) -> Self {
        Self(Arc::new(Binding {
            batch: batch.clone(),
            object: Mutex::new(None),
            state: NativeSource::new(batch, RefState::Loading, |a, b| a == b),
            contract,
            released: AtomicBool::new(false),
            abort: Mutex::new(None),
            caller: Caller {
                principal: None,
                incarnation: String::new(),
            },
            members: Mutex::new(IndexMap::new()),
            subscriptions: Mutex::new(Vec::new()),
            bound_instance: Mutex::new(None),
        }))
    }
    pub fn state(&self) -> NativeSource<RefState> {
        self.0.state.clone()
    }
    pub fn contract(&self) -> Arc<Contract> {
        self.0.contract.clone()
    }
    pub fn release(&self) {
        if self.0.released.swap(true, Ordering::AcqRel) {
            return;
        }
        lock(&self.0.abort).take();
        lock(&self.0.subscriptions).clear();
        let members = lock(&self.0.members).values().cloned().collect::<Vec<_>>();
        for member in members {
            member.detach("released");
        }
        let object = lock(&self.0.object).take();
        let _ = self.0.state.set(
            RefState::Ended {
                error: ChordError::new("released", "reference released"),
            },
            &Context::background(),
        );
        self.0.state.detach("released");
        if let Some(object) = object {
            object.changed.send_modify(|n| *n = n.wrapping_add(1));
            let last = {
                let mut state = lock(&object.state);
                state.holds = state.holds.saturating_sub(1);
                state.holds == 0
            };
            if let Some(inner) = object.host.upgrade() {
                let host = Host(inner);
                let _ = host.publish_presence(object.contract.name());
                let _ = host.publish_activity();
                if last {
                    host.unheld(object);
                }
            }
        }
    }
    fn object(&self) -> Result<Arc<Object>> {
        if self.0.released.load(Ordering::Acquire) {
            return Err(released("ended", "reference released"));
        }
        lock(&self.0.object)
            .clone()
            .ok_or_else(|| ChordError::new("not_ready", "recording handle is inert").into())
    }
    pub async fn wait(&self, context: &Context) -> Result<()> {
        let object = self.object()?;
        let mut changed = object.changed.subscribe();
        let waiting = async {
            loop {
                changed.borrow_and_update();
                if self.0.released.load(Ordering::Acquire) {
                    return Err(released("ended", "reference released"));
                }
                match object.phase_for(&self.0.contract) {
                    RefState::Live => return Ok(()),
                    RefState::Ended { error } => return Err(error.into()),
                    RefState::Failed { cause, .. }
                        if matches!(cause.kind, FailureKind::Broken | FailureKind::Denied) =>
                    {
                        return Err(cause.error.into());
                    }
                    _ => {}
                }
                changed.changed().await.map_err(|_| Error::Closed)?;
            }
        };
        context
            .await_with(waiting)
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?
    }
    pub fn read(&self, name: &str) -> Result<JsonValue> {
        let source = self.member(name)?;
        Ok(source.value()?.to_json())
    }
    pub fn member(&self, name: &str) -> Result<Source> {
        if self.0.released.load(Ordering::Acquire) {
            return Err(released("ended", "reference released"));
        }
        if !self
            .0
            .contract
            .current()
            .variants
            .values()
            .any(|v| v.state.contains_key(name))
        {
            return Err(ChordError::invalid(
                "member",
                format!("unknown state member {name}"),
                None,
            )
            .into());
        }
        let object = lock(&self.0.object).clone();
        if let Some(object) = object {
            let instance = {
                let state = lock(&object.state);
                if matches!(state.phase, RefState::Live) {
                    state.attempt.clone()
                } else {
                    None
                }
            };
            if let Some(instance) = instance {
                self.0.forward(&instance)?;
            }
        }
        Ok(lock(&self.0.members)
            .entry(name.into())
            .or_insert_with(|| Source::framed(self.0.batch.clone(), format!("held:{name}"), |_| {}))
            .clone())
    }
    pub fn variant(&self) -> Result<String> {
        let object = self.object()?;
        let state = lock(&object.state);
        Ok(state
            .attempt
            .as_ref()
            .ok_or_else(|| ChordError::new("not_ready", "object is not live"))?
            .members
            .variant
            .clone())
    }
    pub fn retry(&self) -> Result<()> {
        let object = self.object()?;
        let host = Host(object.host.upgrade().ok_or(Error::Closed)?);
        host.activate(object)
    }
}
impl Binding {
    fn forward(&self, instance: &Instance) -> Result<()> {
        {
            let mut bound = lock(&self.bound_instance);
            if bound.as_deref() == Some(&instance.id) {
                return Ok(());
            }
            *bound = Some(instance.id.clone());
        }
        lock(&self.subscriptions).clear();
        let object = lock(&self.object).clone().ok_or(Error::Closed)?;
        let host = Host(object.host.upgrade().ok_or(Error::Closed)?);
        let mut subscriptions = Vec::new();
        for (name, source) in &instance.members.state {
            let proxy = {
                let mut members = lock(&self.members);
                members
                    .entry(name.clone())
                    .or_insert_with(|| Source::framed(host.batch(), format!("held:{name}"), |_| {}))
                    .clone()
            };
            match source.value() {
                Ok(value) => proxy.set_revision(value, &Context::background())?,
                Err(error) => proxy.fail(error.to_string(), &Context::background())?,
            }
            subscriptions.push(source.subscribe(
                move |call, context| match call {
                    crate::watch::WatchCall::Value { value, .. } => proxy.set(value, context),
                    crate::watch::WatchCall::Failed { error } => proxy.fail(error, context),
                },
                |_| {},
            ));
        }
        *lock(&self.subscriptions) = subscriptions;
        Ok(())
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire)
            && let Some(object) = lock(&self.object).take()
        {
            let last = {
                let mut state = lock(&object.state);
                state.holds = state.holds.saturating_sub(1);
                state.holds == 0
            };
            if let Some(inner) = object.host.upgrade() {
                let host = Host(inner);
                let _ = host.publish_presence(object.contract.name());
                let _ = host.publish_activity();
                if last {
                    host.unheld(object);
                }
            }
        }
    }
}
pub struct Root {
    host: Host,
    held: Mutex<Vec<Held>>,
    closed: AtomicBool,
}
impl Root {
    pub fn hold(&self, contract: Arc<Contract>, params: JsonValue) -> Result<Held> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let held = self.host.hold(contract, params, &Context::background())?;
        lock(&self.held).push(held.clone());
        Ok(held)
    }
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            for held in std::mem::take(&mut *lock(&self.held)) {
                held.release();
            }
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        self.close();
    }
}
fn released(reason: &str, message: &str) -> Error {
    let mut e = ChordError::new("released", message);
    e.reason = Some(reason.into());
    e.into()
}
fn revoked() -> ChordError {
    let mut e = ChordError::new("forbidden", "principal revoked");
    e.reason = Some("revoked".into());
    e
}
fn as_chord(error: Error) -> ChordError {
    match error {
        Error::Chord(error) => *error,
        other => ChordError::new("unavailable", other.to_string()),
    }
}
