use super::*;
use crate::{
    documents::{Definition, Handle},
    jobs::{JobDefinition, JobImplementation},
    registries::{Registry, RegistryProvider},
    watch::{NativeWatchCall, WatchCall},
};
use std::sync::atomic::AtomicU64;
type Method = Arc<dyn Fn(JsonValue, Ctx) -> SyncFuture<JsonValue> + Send + Sync>;
type EventListener = Arc<dyn Fn(JsonValue) + Send + Sync>;

/// Native event broadcaster for contract `event` members.
#[derive(Clone, Default)]
pub struct EventChannel {
    listeners: Arc<Mutex<IndexMap<u64, EventListener>>>,
    next: Arc<AtomicU64>,
}
impl EventChannel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn emit(&self, payload: JsonValue) {
        let listeners: Vec<_> = lock(&self.listeners).values().cloned().collect();
        for listener in listeners {
            listener(payload.clone());
        }
    }
    pub fn subscribe(
        &self,
        listener: impl Fn(JsonValue) + Send + Sync + 'static,
    ) -> EventSubscription {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        lock(&self.listeners).insert(id, Arc::new(listener));
        EventSubscription {
            listeners: Arc::downgrade(&self.listeners),
            id,
        }
    }
}

/// Drops the listener from its `EventChannel`.
pub struct EventSubscription {
    listeners: Weak<Mutex<IndexMap<u64, EventListener>>>,
    id: u64,
}
impl Drop for EventSubscription {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            lock(&listeners).shift_remove(&self.id);
        }
    }
}

#[derive(Clone)]
pub struct Members {
    pub variant: String,
    pub state: IndexMap<String, Source>,
    pub methods: IndexMap<String, Method>,
    pub events: IndexMap<String, EventChannel>,
}
impl Default for Members {
    fn default() -> Self {
        Self::new()
    }
}
impl Members {
    pub fn new() -> Self {
        Self {
            variant: crate::contracts::LOCAL.into(),
            state: IndexMap::new(),
            methods: IndexMap::new(),
            events: IndexMap::new(),
        }
    }
    pub fn variant(name: impl Into<String>) -> Self {
        Self {
            variant: name.into(),
            ..Self::new()
        }
    }
    fn has_member(&self, name: &str) -> bool {
        self.state.contains_key(name)
            || self.methods.contains_key(name)
            || self.events.contains_key(name)
    }
    pub fn state(&mut self, name: impl Into<String>, source: Source) -> Result<()> {
        let name = name.into();
        if self.has_member(&name) {
            return Err(Error::Invalid(format!("duplicate member {name}")));
        }
        self.state.insert(name, source);
        Ok(())
    }
    pub fn method(
        &mut self,
        name: impl Into<String>,
        method: impl Fn(JsonValue, Ctx) -> SyncFuture<JsonValue> + Send + Sync + 'static,
    ) -> Result<()> {
        let name = name.into();
        if self.has_member(&name) {
            return Err(Error::Invalid(format!("duplicate member {name}")));
        }
        self.methods.insert(name, Arc::new(method));
        Ok(())
    }
    pub fn event(&mut self, name: impl Into<String>, channel: EventChannel) -> Result<()> {
        let name = name.into();
        if self.has_member(&name) {
            return Err(Error::Invalid(format!("duplicate member {name}")));
        }
        self.events.insert(name, channel);
        Ok(())
    }
}
#[derive(Clone)]
pub(super) struct RegistryDeclaration {
    pub(super) token: Registry<JsonValue, JsonValue>,
    pub(super) provider: RegistryProvider<JsonValue, JsonValue>,
}
pub struct FacetHost {
    pub(super) plugin: String,
    pub(super) http: Option<crate::node::http::RegisteredHttp>,
    pub(super) host: Host,
    pub(super) types: IndexMap<String, Arc<Served>>,
    pub(super) registries: IndexMap<String, RegistryDeclaration>,
    pub(super) contributions: IndexMap<String, (Registry<JsonValue, JsonValue>, JsonValue)>,
}
impl FacetHost {
    /// Contribute native handlers to the host's `chord.http` v1 registry.
    pub fn contribute_http(&mut self, value: crate::node::http::HttpContribution) -> Result<()> {
        if self.http.is_some() {
            return Err(Error::Invalid("duplicate chord.http contribution".into()));
        }
        self.http = Some(
            value
                .registered(&self.plugin)
                .map_err(|e| Error::Invalid(e.to_string()))?,
        );
        Ok(())
    }
    pub fn serve(
        &mut self,
        contract: Arc<Contract>,
        factory: impl Fn(&JsonValue, Option<&serde_json::Value>, &mut ObjectSelf) -> Result<Members>
        + Send
        + Sync
        + 'static,
        options: ServeOptions,
    ) -> Result<()> {
        if self.types.contains_key(contract.name()) {
            return Err(Error::Invalid(format!(
                "{} already serves {}",
                self.plugin,
                contract.name()
            )));
        }
        if contract.kind() == ContractKind::Remote
            && (self.host.0.options.principal.is_none() || self.host.0.options.address.is_none())
        {
            return Err(Error::Invalid(
                "serving remote objects requires a host principal and address".into(),
            ));
        }
        let accepted = crate::contracts::check_accepts(&contract, &options.accepts)?;
        self.types.insert(
            contract.name().into(),
            Arc::new(Served {
                plugin: self.plugin.clone(),
                contract,
                factory: Arc::new(factory),
                options,
                accepted,
            }),
        );
        Ok(())
    }
    pub fn serve_registry(
        &mut self,
        token: Registry<JsonValue, JsonValue>,
        provider: RegistryProvider<JsonValue, JsonValue>,
    ) -> Result<()> {
        if self.registries.contains_key(&token.name) {
            return Err(Error::Invalid(format!("duplicate registry {}", token.name)));
        }
        self.registries
            .insert(token.name.clone(), RegistryDeclaration { token, provider });
        Ok(())
    }
    pub fn contribute(
        &mut self,
        token: Registry<JsonValue, JsonValue>,
        value: JsonValue,
    ) -> Result<()> {
        if self.contributions.contains_key(&token.name) {
            return Err(Error::Invalid(format!(
                "duplicate contribution {}",
                token.name
            )));
        }
        self.contributions
            .insert(token.name.clone(), (token, value));
        Ok(())
    }
    pub fn principal(&self) -> Option<&serde_json::Value> {
        self.host.0.options.principal.as_ref()
    }
    pub fn host(&self) -> Host {
        self.host.clone()
    }
}
#[derive(Clone)]
pub(super) enum Request {
    Type(Arc<Contract>, JsonValue),
    Registry(Registry<JsonValue, JsonValue>),
}
#[derive(Clone, Default)]
pub(super) struct Recorded {
    pub(super) documents: Vec<Arc<Definition>>,
    pub(super) tables: Vec<crate::store::TableDefinition>,
    pub(super) jobs: Vec<JobDefinition>,
    pub(super) contributions: Vec<Registry<JsonValue, JsonValue>>,
    pub(super) requests: Vec<Request>,
}
pub(super) struct PendingJob {
    pub(super) definition: JobDefinition,
    pub(super) implementation: JobImplementation,
    pub(super) binding: JobBinding,
}
#[derive(Clone)]
pub struct JobBinding(pub(super) Arc<Mutex<Option<crate::jobs::Job>>>);
impl JobBinding {
    pub fn get(&self) -> Result<crate::jobs::Job> {
        lock(&self.0)
            .clone()
            .ok_or_else(|| ChordError::new("not_ready", "job's object is not live").into())
    }
}
pub(super) type Routine = Arc<dyn Fn(Ctx) -> SyncFuture<()> + Send + Sync>;
pub(super) type Opener = Box<dyn FnOnce(Ctx) -> SyncFuture<()> + Send>;
pub struct ObjectSelf {
    pub(super) host: Host,
    pub(super) object: Arc<Object>,
    pub(super) served: Arc<Served>,
    pub(super) recording: bool,
    pub(super) recorded: Recorded,
    pub(super) attempt: Option<crate::documents::Attempt>,
    pub(super) dependencies: Vec<Held>,
    pub(super) jobs: Vec<PendingJob>,
    pub(super) routines: Vec<Routine>,
    pub(super) openers: Vec<Opener>,
    pub(super) cleanups: Vec<Cleanup>,
    pub(super) contributions: Vec<(Registry<JsonValue, JsonValue>, JsonValue)>,
    pub(super) opens_natives: bool,
    pub(super) admitted: Option<JsonValue>,
}
impl ObjectSelf {
    pub(super) fn new(
        host: Host,
        object: Arc<Object>,
        served: Arc<Served>,
        recording: bool,
    ) -> Self {
        Self {
            attempt: if recording {
                None
            } else {
                Some(object.documents.attempt())
            },
            host,
            object,
            served,
            recording,
            recorded: Recorded::default(),
            dependencies: Vec::new(),
            jobs: Vec::new(),
            routines: Vec::new(),
            openers: Vec::new(),
            cleanups: Vec::new(),
            contributions: Vec::new(),
            opens_natives: false,
            admitted: None,
        }
    }
    pub fn batch(&self) -> Batch {
        self.host.batch()
    }
    pub fn host(&self) -> Host {
        self.host.clone()
    }
    pub fn store(&self) -> Option<&crate::store::StoreEngine> {
        self.host.store()
    }
    pub fn writable(&self, value: JsonValue) -> Result<Source> {
        Source::new(self.host.batch(), value)
    }
    pub fn admitted(&self) -> Option<&JsonValue> {
        self.admitted.as_ref()
    }
    pub fn get(&mut self, contract: Arc<Contract>, params: JsonValue) -> Result<Held> {
        crate::contracts::check_params(contract.params(), &params, "params")?;
        self.recorded
            .requests
            .push(Request::Type(contract.clone(), params.clone()));
        if self.recording {
            return Ok(Held::inert(contract, self.host.batch()));
        }
        let held = self.host.hold_as(
            contract,
            params,
            Caller {
                principal: self
                    .object
                    .principal
                    .clone()
                    .or_else(|| self.host.0.options.principal.clone()),
                incarnation: self.host.incarnation().into(),
            },
            &Context::background(),
        )?;
        self.dependencies.push(held.clone());
        Ok(held)
    }
    pub fn registry(
        &mut self,
        token: Registry<JsonValue, JsonValue>,
    ) -> Result<crate::registries::RegistryRecord<JsonValue, JsonValue>> {
        self.recorded
            .requests
            .push(Request::Registry(token.clone()));
        if self.recording {
            return Ok(crate::registries::RegistryRecord::new(
                token,
                self.host.batch(),
                |_| {},
            ));
        }
        Ok(self.host.registry(token))
    }
    pub fn contribute(
        &mut self,
        token: Registry<JsonValue, JsonValue>,
        value: JsonValue,
    ) -> Result<()> {
        if !self.served.singleton() {
            return Err(Error::Invalid(
                "only local singleton objects contribute to registries".into(),
            ));
        }
        if self
            .recorded
            .contributions
            .iter()
            .any(|t| t.name == token.name)
        {
            return Err(Error::Invalid(format!(
                "duplicate contribution {}",
                token.name
            )));
        }
        self.recorded.contributions.push(token.clone());
        if !self.recording {
            self.contributions.push((token, value));
        }
        Ok(())
    }
    pub fn state(&mut self, definition: Arc<Definition>) -> Result<ObjectDocument> {
        if self
            .recorded
            .documents
            .iter()
            .any(|d| d.name() == definition.name())
        {
            return Err(Error::Invalid(format!(
                "duplicate document {}",
                definition.name()
            )));
        }
        let singleton = definition.is_singleton();
        self.recorded.documents.push(definition.clone());
        let handle = self
            .attempt
            .as_ref()
            .map(|a| a.declare(definition))
            .transpose()?;
        let source = if let Some(handle) = handle.clone().filter(|_| singleton) {
            self.follow(move |target, ctx| {
                Box::pin(async move {
                    let source = handle.source()?;
                    target.set(source.value()?.to_json(), &ctx.context)?;
                    let changed = target.clone();
                    let ended = target.clone();
                    let subscription = source.subscribe(
                        move |call, context| match call {
                            WatchCall::Value { value, .. } => changed.set(value, context),
                            WatchCall::Failed { error } => changed.fail(error, context),
                        },
                        move |_| ended.detach("ended"),
                    );
                    Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                })
            })?
        } else {
            Source::framed(self.host.batch(), "recording document", |_| {})
        };
        Ok(ObjectDocument {
            object: Arc::downgrade(&self.object),
            handle,
            source,
        })
    }
    pub fn table(&mut self, definition: crate::store::TableDefinition) -> Result<()> {
        if !self.served.singleton() || self.host.0.options.store.is_none() {
            return Err(Error::Invalid(
                "tables require a persistent local singleton".into(),
            ));
        }
        if self
            .recorded
            .tables
            .iter()
            .any(|t| t.name == definition.name)
        {
            return Err(Error::Invalid(format!(
                "duplicate table {}",
                definition.name
            )));
        }
        self.recorded.tables.push(definition);
        Ok(())
    }
    pub fn job(
        &mut self,
        definition: JobDefinition,
        implementation: JobImplementation,
    ) -> Result<JobBinding> {
        if self.recorded.jobs.iter().any(|j| j.name == definition.name) {
            return Err(Error::Invalid(format!("duplicate job {}", definition.name)));
        }
        if definition.persist.is_some()
            && (!self.served.singleton() || self.host.0.options.store.is_none())
        {
            return Err(Error::Invalid(
                "persisted jobs require a stored local singleton".into(),
            ));
        }
        self.recorded.jobs.push(definition.clone());
        let binding = JobBinding(Arc::new(Mutex::new(None)));
        self.jobs.push(PendingJob {
            definition,
            implementation,
            binding: binding.clone(),
        });
        Ok(binding)
    }
    pub fn on_live(&mut self, routine: impl Fn(Ctx) -> SyncFuture<()> + Send + Sync + 'static) {
        self.routines.push(Arc::new(routine));
    }
    pub fn on(
        &mut self,
        source: NativeSource<JsonValue>,
        routine: impl Fn(JsonValue, Ctx) -> SyncFuture<()> + Send + Sync + 'static,
    ) {
        let routine = Arc::new(routine);
        self.on_live(move |ctx| {
            let source = source.clone();
            let routine = routine.clone();
            Box::pin(async move {
                let (updates, mut receive) = tokio::sync::mpsc::unbounded_channel();
                let initial = source.current()?;
                let observation = source.observe(
                    &ctx.context,
                    move |call, _| {
                        if let NativeWatchCall::Value { value } = call {
                            let _ = updates.send(value);
                        }
                        Ok(())
                    },
                    |_| {},
                )?;
                (routine)(initial, ctx.clone()).await?;
                loop {
                    let next = ctx
                        .context
                        .await_with(receive.recv())
                        .await
                        .map_err(|e| ChordError::new("aborted", e.to_string()))?;
                    let Some(next) = next else {
                        break;
                    };
                    (routine)(next, ctx.clone()).await?;
                }
                drop(observation);
                Ok(())
            })
        });
    }
    pub fn open<T: Send + Sync + 'static>(
        &mut self,
        open: impl FnOnce(Ctx) -> SyncFuture<T> + Send + 'static,
        close: impl FnOnce(Arc<T>, String) -> SyncFuture<()> + Send + 'static,
    ) -> NativeHandle<T> {
        self.opens_natives = true;
        let handle = NativeHandle(Arc::new(Mutex::new(None)));
        let opened = handle.clone();
        self.openers.push(Box::new(move |ctx| {
            Box::pin(async move {
                *lock(&opened.0) = Some(Arc::new(open(ctx).await?));
                Ok(())
            })
        }));
        let closing = handle.clone();
        self.cleanups.push(Box::new(move |reason| {
            Box::pin(async move {
                let value = lock(&closing.0).take();
                if let Some(value) = value {
                    close(value, reason).await?;
                }
                Ok(())
            })
        }));
        handle
    }
    pub fn follow(
        &mut self,
        attach: impl FnOnce(Source, Ctx) -> SyncFuture<Box<dyn FnOnce() + Send>> + Send + 'static,
    ) -> Result<Source> {
        let followed = Source::framed(self.host.batch(), "followed source", |_| {});
        let target = followed.clone();
        let detach = Arc::new(Mutex::new(None::<Box<dyn FnOnce() + Send>>));
        let attached = detach.clone();
        self.openers.push(Box::new(move |ctx| {
            Box::pin(async move {
                *lock(&attached) = Some(attach(target, ctx).await?);
                Ok(())
            })
        }));
        let closing = followed.clone();
        self.cleanups.push(Box::new(move |_| {
            Box::pin(async move {
                if let Some(detach) = lock(&detach).take() {
                    detach();
                }
                closing.detach("released");
                Ok(())
            })
        }));
        Ok(followed)
    }
    pub fn follow_source(&mut self, source: Source) -> Result<Source> {
        self.follow(move |target, ctx| {
            Box::pin(async move {
                target.set(source.value()?.to_json(), &ctx.context)?;
                let changed = target.clone();
                let ended = target.clone();
                let subscription = source.subscribe(
                    move |call, context| match call {
                        WatchCall::Value { value, .. } => changed.set(value, context),
                        WatchCall::Failed { error } => changed.fail(error, context),
                    },
                    move |_| ended.detach("ended"),
                );
                Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
            })
        })
    }
}
pub struct NativeHandle<T>(Arc<Mutex<Option<Arc<T>>>>);
impl<T> Clone for NativeHandle<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> NativeHandle<T> {
    pub fn value(&self) -> Result<Arc<T>> {
        lock(&self.0)
            .clone()
            .ok_or_else(|| ChordError::new("not_ready", "native resource has not opened").into())
    }
}
#[derive(Clone)]
pub struct ObjectDocument {
    object: Weak<Object>,
    handle: Option<Handle>,
    source: Source,
}
impl ObjectDocument {
    /// Access the document by key once the object is active.
    pub fn keyed_handle(&self) -> Result<Handle> {
        self.handle
            .clone()
            .ok_or_else(|| ChordError::new("not_ready", "recording document is inert").into())
    }
    pub fn source(&self) -> Result<Source> {
        Ok(self.source.clone())
    }
    pub fn value(&self) -> Result<JsonValue> {
        Ok(self
            .handle
            .as_ref()
            .ok_or_else(|| ChordError::new("not_ready", "recording document is inert"))?
            .value()?
            .to_json())
    }
    pub async fn change<T: Send>(
        &self,
        context: &Context,
        change: impl FnOnce(crate::documents::Draft) -> SyncFuture<T> + Send,
    ) -> Result<T> {
        self.change_at(None, context, change).await
    }
    pub async fn change_at<T: Send>(
        &self,
        key: Option<crate::documents::DocumentKey>,
        context: &Context,
        change: impl FnOnce(crate::documents::Draft) -> SyncFuture<T> + Send,
    ) -> Result<T> {
        let object = self.object.upgrade().ok_or(Error::Closed)?;
        let handle = self
            .handle
            .clone()
            .ok_or_else(|| ChordError::new("not_ready", "recording document is inert"))?;
        let token = handle.token().clone();
        object
            .documents
            .transaction(&token, context, move |tx| async move {
                let draft = tx.draft(&handle, key).await?;
                change(draft).await
            })
            .await
    }
}
