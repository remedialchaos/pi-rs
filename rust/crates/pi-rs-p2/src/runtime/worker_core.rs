//! Host-lifetime state shared by native p2 worker facets.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use indexmap::IndexMap;
use pi_rs_ai::registry::Models;
use pi_rs_chord::{
    Error, Result, context::Context, store::StoreEngine, sync::SyncFuture, watch::WatchCall,
};
use pi_rs_durable::{
    ConversationId, ROOT_CONVERSATION_ID,
    harness::{Conversation, EnvTarget, Harness, HarnessOptions, TaskGraph, configure},
    images::NativeImageProcessor,
    session::{Error as DurableError, Observer, Subscription},
};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::runtime::{
    main_conversation::top_level_conversations,
    session_store::durable_storage,
    session_worker::{
        Location, SessionInit, WorkerBoundary, WorkerBoundaryBackend, WorkerInspection,
    },
    worker_environment::{
        ConversationPlacement, EnvironmentProvider, Execution, Machine, SessionLocation,
        WorkerEnvironmentError, conversation_environment,
    },
};

use super::{
    WorkerInit,
    durable::{DurableRegistry, durable_error},
};

pub type InputGate = Arc<dyn Fn(Context) -> SyncFuture<()> + Send + Sync>;
pub type ConversationConfigured =
    Arc<dyn Fn(Conversation, Context) -> SyncFuture<()> + Send + Sync>;
pub type ReadOnlyConversation =
    Arc<dyn Fn(ConversationId, Context) -> SyncFuture<Option<String>> + Send + Sync>;
pub type RelocationCheck =
    Arc<dyn Fn(Harness, Location, Location, Context) -> SyncFuture<Option<String>> + Send + Sync>;

pub struct PlacedConversations {
    pub followers: Vec<ConversationId>,
    pub own: Vec<Location>,
}

pub type PlaceConversations = Arc<
    dyn Fn(Harness, Location, Arc<WorkerInit>, Context) -> SyncFuture<PlacedConversations>
        + Send
        + Sync,
>;

pub struct ConversationLocations {
    pub own: Arc<dyn ConversationPlacement>,
    pub place: PlaceConversations,
}

/// Native worker plugins configure these entries before opening, and may update them while live.
pub struct WorkerRegistries {
    pub models: Models,
    pub settings: watch::Sender<Value>,
    pub extensions: DurableRegistry,
    pub input_gates: Mutex<IndexMap<String, InputGate>>,
    pub conversation_configured: Mutex<IndexMap<String, ConversationConfigured>>,
    pub read_only: Mutex<IndexMap<String, ReadOnlyConversation>>,
    pub relocation_checks: Mutex<IndexMap<String, RelocationCheck>>,
    pub conversation_locations: Mutex<Option<Arc<ConversationLocations>>>,
    pub environments: Arc<WorkerEnvironments>,
}

impl WorkerRegistries {
    pub fn new(models: Models) -> Self {
        Self {
            models,
            settings: watch::channel(json!({})).0,
            extensions: DurableRegistry::default(),
            input_gates: Mutex::default(),
            conversation_configured: Mutex::default(),
            read_only: Mutex::default(),
            relocation_checks: Mutex::default(),
            conversation_locations: Mutex::default(),
            environments: Arc::default(),
        }
    }

    pub async fn wait_for_input(&self, context: &Context) -> Result<()> {
        let gates = entries(&self.input_gates);
        for gate in gates {
            gate(context.clone()).await?;
        }
        Ok(())
    }

    pub async fn configured(&self, conversation: &Conversation, context: &Context) -> Result<()> {
        let callbacks = entries(&self.conversation_configured);
        for callback in callbacks {
            callback(conversation.clone(), context.clone()).await?;
        }
        Ok(())
    }

    pub async fn read_only_reason(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> Result<Option<String>> {
        let checks = entries(&self.read_only);
        for check in checks {
            if let Some(reason) = check(id, context.clone()).await? {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }

    fn placement(&self) -> Option<Arc<ConversationLocations>> {
        self.conversation_locations
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

fn entries<T: Clone>(registry: &Mutex<IndexMap<String, T>>) -> Vec<T> {
    registry
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .values()
        .cloned()
        .collect()
}

#[derive(Default)]
pub struct WorkerEnvironments(Mutex<Option<Arc<dyn EnvironmentProvider>>>);

impl WorkerEnvironments {
    pub fn set_remote(&self, provider: Option<Arc<dyn EnvironmentProvider>>) {
        *self.0.lock().unwrap_or_else(|err| err.into_inner()) = provider;
    }

    fn remote(&self) -> std::result::Result<Arc<dyn EnvironmentProvider>, WorkerEnvironmentError> {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
            .ok_or_else(|| {
                WorkerEnvironmentError::Provider("No remote environment provider installed".into())
            })
    }
}

impl EnvironmentProvider for WorkerEnvironments {
    fn open(&self, name: &str) -> std::result::Result<Machine, WorkerEnvironmentError> {
        self.remote()?.open(name)
    }

    fn sandbox(&self, name: &str) -> std::result::Result<bool, WorkerEnvironmentError> {
        self.remote()?.sandbox(name)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LivenessCounts {
    pub active_calls: usize,
    pub viewers: usize,
    pub durable_tasks: usize,
    pub cache_warming: usize,
}

impl LivenessCounts {
    pub fn idle(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Copy)]
pub enum Activity {
    Viewer,
    CacheWarming,
}

/// Holds a viewer or cache warmer until the caller drops its guard.
pub struct ActivityGuard(Arc<WorkerProcess>, Activity);

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.activity(self.1).fetch_sub(1, Ordering::AcqRel);
    }
}

struct CallGuard<'a>(&'a AtomicUsize);
impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct WorkerProcess {
    pub init: Arc<WorkerInit>,
    pub location: Arc<SessionLocation>,
    pub registries: Arc<WorkerRegistries>,
    pub images: NativeImageProcessor,
    active_calls: AtomicUsize,
    viewers: AtomicUsize,
    cache_warming: AtomicUsize,
    stopping: AtomicBool,
    harness: watch::Sender<Option<Arc<HarnessParts>>>,
}

impl WorkerProcess {
    pub(super) fn new(init: WorkerInit, session: &SessionInit, models: Models) -> Self {
        Self {
            init: Arc::new(init),
            location: Arc::new(SessionLocation::new(session.location())),
            registries: Arc::new(WorkerRegistries::new(models)),
            images: NativeImageProcessor::default(),
            active_calls: AtomicUsize::default(),
            viewers: AtomicUsize::default(),
            cache_warming: AtomicUsize::default(),
            stopping: AtomicBool::default(),
            harness: watch::channel(None).0,
        }
    }

    pub fn harness(&self) -> Result<Arc<HarnessParts>> {
        self.harness
            .borrow()
            .clone()
            .ok_or_else(|| Error::Invalid("The session's Harness is not open".into()))
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub(super) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub fn hold_activity(self: &Arc<Self>, activity: Activity) -> ActivityGuard {
        self.activity(activity).fetch_add(1, Ordering::AcqRel);
        ActivityGuard(self.clone(), activity)
    }

    fn activity(&self, activity: Activity) -> &AtomicUsize {
        match activity {
            Activity::Viewer => &self.viewers,
            Activity::CacheWarming => &self.cache_warming,
        }
    }

    pub async fn call<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        let parts = self.harness()?;
        parts
            .boundary
            .liveness()
            .call(async {
                self.active_calls.fetch_add(1, Ordering::AcqRel);
                let _call = CallGuard(&self.active_calls);
                work.await
            })
            .await
    }

    pub fn liveness(&self) -> LivenessCounts {
        let harness = self.harness.borrow();
        LivenessCounts {
            active_calls: self.active_calls.load(Ordering::Acquire)
                + usize::from(
                    harness
                        .as_ref()
                        .is_some_and(|parts| parts.boundary.liveness().relocating()),
                ),
            viewers: self.viewers.load(Ordering::Acquire),
            durable_tasks: harness
                .as_ref()
                .map_or(0, |parts| parts.active.load(Ordering::Acquire)),
            cache_warming: self.cache_warming.load(Ordering::Acquire),
        }
    }

    pub(super) async fn open(
        &self,
        store: &StoreEngine,
        session: &SessionInit,
        boundary: Arc<WorkerBoundary>,
        context: &Context,
    ) -> Result<()> {
        let mut execution =
            Execution::new(self.location.value(), self.registries.environments.clone());
        let registries = Arc::downgrade(&self.registries);
        execution.set_placement_lookup(move || {
            registries
                .upgrade()?
                .placement()
                .map(|entry| entry.own.clone())
        });
        execution
            .at(&self.location.value())
            .map_err(durable_error)?;
        let environments = Arc::new(AsyncMutex::new(execution));
        let storage = match durable_storage(store) {
            Ok(storage) => storage,
            Err(error) => {
                environments.lock().await.close().await;
                return Err(durable_error(error));
            }
        };
        let envs = environments.clone();
        let settings = self.registries.settings.subscribe();
        let options = HarnessOptions {
            env: Some(Arc::new(move |target, context| {
                let envs = envs.clone();
                Box::pin(async move {
                    let EnvTarget {
                        conversation_id,
                        read,
                        ..
                    } = target;
                    let mut execution = envs.lock().await;
                    conversation_environment(&mut execution, conversation_id, context, Some(&read))
                        .await
                        .map(|(_, env)| Some(env))
                        .map_err(|error| DurableError::Callback(Box::new(error)))
                })
            })),
            models: self.registries.models.clone(),
            settings: Arc::new(move || settings.borrow().clone()),
            registry: Arc::new(self.registries.extensions.reader(&Context::background())?),
            ..Default::default()
        };
        let durable = match Harness::open_with(Arc::new(storage), options, context).await {
            Ok(durable) => durable,
            Err(error) => {
                environments.lock().await.close().await;
                return Err(durable_error(error));
            }
        };
        environments
            .lock()
            .await
            .set_reader(Some(Arc::new(durable.session.clone())));
        let mut parts = HarnessParts {
            durable,
            environments,
            boundary,
            active: Arc::default(),
            _task_graph: None,
            store: store.clone(),
        };
        if let Err(error) = self.initialize(&parts, session, context).await {
            let _ = parts.close().await;
            return Err(error);
        }
        if session.resume != Some(false)
            && let Err(error) = parts.observe_tasks(context).await
        {
            let _ = parts.close().await;
            return Err(error);
        }
        let parts = Arc::new(parts);
        self.harness.send_replace(Some(parts.clone()));
        if session.resume != Some(false)
            && let Err(error) = parts.durable.resume()
        {
            let _ = self.close().await;
            return Err(durable_error(error));
        }
        Ok(())
    }

    async fn initialize(
        &self,
        parts: &HarnessParts,
        session: &SessionInit,
        context: &Context,
    ) -> Result<()> {
        let location = self.location.value();
        let initial = Map::from_iter([
            ("model".into(), serde_json::to_value(&session.model)?),
            ("cwd".into(), json!(location.cwd)),
            ("thinkingLevel".into(), json!(session.thinking_level)),
        ]);
        parts
            .durable
            .commit(
                async |tx| {
                    if tx.conversation(ROOT_CONVERSATION_ID).await?.is_none() {
                        if parts
                            .durable
                            .models
                            .get_model(&session.model.provider, &session.model.model_id)
                            .is_none()
                        {
                            return Err(DurableError::invalid("Unknown model"));
                        }
                        tx.create_root_conversation().await?;
                        configure(tx, ROOT_CONVERSATION_ID, &initial).await?;
                    }
                    Ok(())
                },
                context,
            )
            .await
            .map_err(durable_error)?;
        self.follow_session(parts, &location, context).await?;
        let root = parts
            .durable
            .conversation(ROOT_CONVERSATION_ID, context)
            .await
            .map_err(durable_error)?
            .ok_or_else(|| Error::Invalid("Missing root conversation".into()))?;
        let agent = root.agent(context).await.map_err(durable_error)?;
        let model = agent.state["model"]["provider"]
            .as_str()
            .zip(agent.state["model"]["modelId"].as_str())
            .and_then(|(provider, id)| parts.durable.models.get_model(provider, id));
        if model.is_none() {
            return Err(Error::Invalid("Unknown model".into()));
        }
        Ok(())
    }

    pub(super) async fn close(&self) -> Result<()> {
        self.stop();
        if let Some(parts) = self.harness.send_replace(None) {
            parts.close().await
        } else {
            Ok(())
        }
    }

    async fn follow_session(
        &self,
        parts: &HarnessParts,
        location: &Location,
        context: &Context,
    ) -> Result<Vec<Location>> {
        let mut ids = top_level_conversations(&parts.durable.session, context)
            .await
            .map_err(durable_error)?;
        let own = if let Some(placement) = self.registries.placement() {
            let placed = (placement.place)(
                parts.durable.clone(),
                location.clone(),
                self.init.clone(),
                context.clone(),
            )
            .await?;
            ids.extend(placed.followers);
            placed.own
        } else {
            Vec::new()
        };
        ids.sort_unstable();
        ids.dedup();
        let mut stale = Vec::new();
        for id in ids {
            if let Some(conversation) = parts
                .durable
                .conversation(id, context)
                .await
                .map_err(durable_error)?
                && conversation
                    .agent(context)
                    .await
                    .map_err(durable_error)?
                    .state["cwd"]
                    .as_str()
                    != Some(location.cwd.as_str())
            {
                stale.push(id);
            }
        }
        if !stale.is_empty() {
            let cwd = Map::from_iter([("cwd".into(), json!(location.cwd))]);
            parts
                .durable
                .commit(
                    async |tx| {
                        for id in stale {
                            configure(tx, id, &cwd).await?;
                        }
                        Ok(())
                    },
                    context,
                )
                .await
                .map_err(durable_error)?;
        }
        Ok(own)
    }
}

pub struct HarnessParts {
    pub durable: Harness,
    pub environments: Arc<AsyncMutex<Execution>>,
    pub boundary: Arc<WorkerBoundary>,
    pub store: StoreEngine,
    active: Arc<AtomicUsize>,
    _task_graph: Option<Subscription>,
}

impl HarnessParts {
    async fn observe_tasks(&mut self, context: &Context) -> Result<()> {
        let active = self.active.clone();
        let graph = self
            .durable
            .observe_task_graph(
                Observer::new(
                    move |change: &WatchCall<Arc<TaskGraph>>, _| {
                        if let WatchCall::Value { value, .. } = change {
                            active.store(value.tasks.len(), Ordering::Release);
                        }
                        Ok(())
                    },
                    |_| {},
                ),
                context,
            )
            .await
            .map_err(durable_error)?;
        self.active
            .store(graph.value.tasks.len(), Ordering::Release);
        self._task_graph = Some(graph.subscription);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let result = self
            .durable
            .close(&Context::background())
            .await
            .map_err(durable_error);
        self.environments.lock().await.close().await;
        result
    }
}

pub(super) struct BoundaryBackend(pub std::sync::Weak<WorkerProcess>);

impl WorkerBoundaryBackend for BoundaryBackend {
    fn inspect(&self, context: Context) -> SyncFuture<WorkerInspection> {
        let process = self.0.clone();
        Box::pin(async move {
            let process = process.upgrade().ok_or(Error::Closed)?;
            let inspection = process
                .harness()?
                .durable
                .inspect(&context)
                .await
                .map_err(durable_error)?;
            Ok(WorkerInspection {
                tasks: (inspection.tasks.len() + inspection.submissions.len()) as u64,
                busy: inspection
                    .tasks
                    .into_iter()
                    .filter(|task| !task.record.background)
                    .map(|task| task.record.conversation_id)
                    .collect(),
            })
        })
    }

    fn check_relocation(
        &self,
        from: Location,
        to: Location,
        context: Context,
    ) -> SyncFuture<Option<String>> {
        let process = self.0.clone();
        Box::pin(async move {
            let process = process.upgrade().ok_or(Error::Closed)?;
            let parts = process.harness()?;
            for check in entries(&process.registries.relocation_checks) {
                if let Some(refusal) = check(
                    parts.durable.clone(),
                    from.clone(),
                    to.clone(),
                    context.clone(),
                )
                .await?
                {
                    return Ok(Some(refusal));
                }
            }
            Ok(None)
        })
    }

    fn relocate(&self, location: Location, context: Context) -> SyncFuture<()> {
        let process = self.0.clone();
        Box::pin(async move {
            let process = process.upgrade().ok_or(Error::Closed)?;
            if process.location.value() == location {
                return Ok(());
            }
            let parts = process.harness()?;
            {
                let mut environments = parts.environments.lock().await;
                environments.at(&location).map_err(durable_error)?;
                environments.switch(location.clone());
            }
            let mut used = process.follow_session(&parts, &location, &context).await?;
            process.location.set(location.clone());
            used.push(location);
            parts.environments.lock().await.retire(&used).await;
            Ok(())
        })
    }
}
