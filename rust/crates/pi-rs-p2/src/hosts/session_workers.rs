//! Route a session hold to one native worker, retaining the hold across restarts.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::{Cancel, Context},
    contracts::{Contract, check_params},
    data::ChordError,
    host::{Caller, Host, PluginFailure},
    json::JsonValue,
    node::workers::{WorkerHandle, WorkerOptions, WorkerState, Workers},
    remote::{LinkRole, RemoteOptions, RemotePeer, RemoteReference},
    sync::SyncFuture,
};
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::Instant;

use super::worker::WorkerInit;
use crate::api::v1::{Principal, SessionAuthorizer, session_address};

type Start = dyn Fn(String, u64, Context) -> SyncFuture<WorkerInit> + Send + Sync;
type Slot = tokio::sync::Mutex<Session>;

const CRASH_RESTARTS: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

struct Session {
    peer: RemotePeer,
    active: bool,
    handle: Option<Arc<WorkerHandle>>,
    stopping: Option<watch::Sender<bool>>,
    failure: Option<ChordError>,
    crashes: Vec<Instant>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[derive(Clone)]
pub struct SessionWorkers(Arc<Supervisor>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionWorkerState {
    pub running: bool,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Supervisor {
    host: Host,
    processes: Workers,
    start: Box<Start>,
    generation: AtomicU64,
    sessions: Mutex<BTreeMap<String, Arc<Slot>>>,
    states: watch::Sender<IndexMap<String, SessionWorkerState>>,
    authorizers: Mutex<Arc<BTreeMap<String, SessionAuthorizer>>>,
    context: Context,
    cancel: Cancel,
}

impl SessionWorkers {
    pub fn new(
        host: &Host,
        process: WorkerOptions,
        start: impl Fn(String, u64, Context) -> SyncFuture<WorkerInit> + Send + Sync + 'static,
    ) -> Result<Self> {
        let (context, cancel) = Context::background().with_cancel();
        Ok(Self(Arc::new(Supervisor {
            host: host.clone(),
            processes: Workers::new(process)?,
            start: Box::new(start),
            generation: AtomicU64::new(0),
            sessions: Mutex::default(),
            states: watch::channel(IndexMap::new()).0,
            authorizers: Mutex::default(),
            context,
            cancel,
        })))
    }

    pub fn observe(&self) -> watch::Receiver<IndexMap<String, SessionWorkerState>> {
        self.0.states.subscribe()
    }

    pub fn generation(&self, session: &str) -> u64 {
        self.0
            .states
            .borrow()
            .get(session)
            .map_or(0, |state| state.generation)
    }

    pub fn running(&self) -> Vec<String> {
        self.0
            .states
            .borrow()
            .iter()
            .filter(|(_, state)| state.running)
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// Stop the current child and clear sticky failure. Existing holds survive and
    /// demand a new generation once the old process has exited.
    pub async fn stop(&self, session: &str) -> Result<()> {
        let Some(slot) = self
            .0
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(session)
            .cloned()
        else {
            return Ok(());
        };
        let (mut stopped, handle) = {
            let mut state = slot.lock().await;
            if !state.active {
                state.failure = None;
                state.crashes.clear();
                self.publish(session, self.generation(session), false, None, None);
                self.supervise(session, &slot, &mut state);
                return Ok(());
            }
            let stopped = state
                .stopping
                .get_or_insert_with(|| watch::channel(false).0)
                .subscribe();
            (stopped, state.handle.clone())
        };
        if let Some(handle) = handle {
            handle.shutdown().await?;
        }
        stopped
            .wait_for(|done| *done)
            .await
            .map_err(|_| Error::Closed)?;
        Ok(())
    }

    fn publish(
        &self,
        session: &str,
        generation: u64,
        running: bool,
        pid: Option<u32>,
        error: Option<String>,
    ) {
        let next = SessionWorkerState {
            running,
            generation,
            pid,
            error,
        };
        self.0.states.send_if_modified(|states| {
            if states.get(session) == Some(&next) {
                return false;
            }
            states.insert(session.into(), next);
            true
        });
    }

    pub fn remote_options(
        &self,
        principal: serde_json::Value,
        contracts: BTreeMap<String, Arc<Contract>>,
    ) -> RemoteOptions {
        let mut options = RemoteOptions::new(principal);
        options.route_contracts = contracts;
        let workers = Arc::downgrade(&self.0);
        options.route = Some(Arc::new(move |request| {
            let workers = workers.clone();
            Box::pin(async move {
                let address = request.contract.host_address(&request.params)?;
                if request.address_kind != address.kind || request.address_params != address.params
                {
                    return Err(ChordError::invalid(
                        "host_address",
                        "does not match the contract's destination",
                        None,
                    )
                    .into());
                }
                match address.kind.as_str() {
                    "server" => Ok(None),
                    "session" => Ok(Some(
                        Self(workers.upgrade().ok_or(Error::Closed)?)
                            .hold(
                                request.contract,
                                request.params,
                                request.caller,
                                &request.context,
                            )
                            .await?,
                    )),
                    _ => Err(ChordError::new("unavailable", "unknown worker host kind").into()),
                }
            })
        }));
        options
    }

    /// Hold a worker contract as a caller already identified by the server.
    pub async fn hold(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        caller: Caller,
        context: &Context,
    ) -> Result<RemoteReference> {
        check_params(contract.params(), &params, "params")?;
        let address = contract.host_address(&params)?;
        let wire_params = address.params.to_serde()?;
        let session = wire_params["id"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing session id".into()))?;
        let wire = session_address(session);
        if address.kind != "session" || address.key != wire["key"] || wire_params != wire["params"]
        {
            return Err(Error::Invalid("invalid session address".into()));
        }
        let principal = caller
            .principal
            .as_ref()
            .and_then(|value| serde_json::from_value::<Principal>(value.clone()).ok());
        let allowed = match principal {
            Some(Principal::Server) => true,
            _ if matches!(contract.name(), "p2.worker-boundary" | "chord.app.data") => false,
            Some(Principal::Host | Principal::Owner { .. }) => true,
            Some(guest @ Principal::Guest { .. }) => {
                self.authorize_guest(guest, session, context).await
            }
            _ => false,
        };
        if !allowed {
            return Err(ChordError::new("forbidden", "worker address denied").into());
        }
        // Validate before allocating state for this address.
        crate::runtime::host_dirs::session_directory(std::path::Path::new(""), session)?;
        let slot = self.slot(session)?;
        let mut state = slot.lock().await;
        let held = state.peer.hold_as(contract, params, caller, context)?;
        if let Some(failure) = &state.failure {
            state.peer.fail_child(failure.clone())?;
        } else {
            self.supervise(session, &slot, &mut state);
        }
        Ok(held)
    }

    pub(crate) fn update_authorizers(
        &self,
        mut next: BTreeMap<String, SessionAuthorizer>,
        failed: &[PluginFailure],
    ) {
        let mut current = self
            .0
            .authorizers
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        for failure in failed {
            next.remove(&failure.plugin);
            // Chord retains the last active facet when its replacement fails.
            if let Some(authorize) = current.get(&failure.plugin) {
                next.insert(failure.plugin.clone(), authorize.clone());
            }
        }
        *current = Arc::new(next);
    }

    async fn authorize_guest(&self, guest: Principal, session: &str, context: &Context) -> bool {
        let authorizers = self
            .0
            .authorizers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        for authorize in authorizers.values() {
            match authorize(
                self.0.host.clone(),
                guest.clone(),
                session.into(),
                context.clone(),
            )
            .await
            {
                Ok(true) => {
                    // A plugin removed or replaced during the check cannot authorize the hold.
                    let current = self
                        .0
                        .authorizers
                        .lock()
                        .unwrap_or_else(|err| err.into_inner());
                    return Arc::ptr_eq(&authorizers, &current);
                }
                Ok(false) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Clear a failed start or exhausted crash budget. Existing holds will rebind.
    pub async fn retry(&self, session: &str) -> Result<()> {
        let slot = self.slot(session)?;
        let mut state = slot.lock().await;
        if !state.active {
            state.failure = None;
            state.crashes.clear();
            self.supervise(session, &slot, &mut state);
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.0.cancel.cancel("session workers closed");
        self.0.processes.close().await;
        let sessions = std::mem::take(
            &mut *self
                .0
                .sessions
                .lock()
                .unwrap_or_else(|err| err.into_inner()),
        );
        for slot in sessions.into_values() {
            let peer = slot.lock().await.peer.clone();
            peer.close();
        }
    }

    fn slot(&self, session: &str) -> Result<Arc<Slot>> {
        let mut sessions = self
            .0
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if self.0.context.is_aborted() {
            return Err(Error::Closed);
        }
        if let Some(slot) = sessions.get(session) {
            return Ok(slot.clone());
        }
        let weak = Arc::downgrade(&self.0);
        let id = session.to_owned();
        let mut options = RemoteOptions::new(serde_json::to_value(Principal::Worker {
            session: id.clone(),
            generation: 0,
        })?);
        options.role = LinkRole::Child;
        options.trusted = true;
        let peer = RemotePeer::supervised_child(self.0.host.clone(), options, move || {
            if let Some(workers) = weak.upgrade() {
                let id = id.clone();
                tokio::spawn(async move {
                    let _ = Self(workers).retry(&id).await;
                });
            }
        })?;
        let slot = Arc::new(Slot::new(Session {
            peer,
            active: false,
            handle: None,
            stopping: None,
            failure: None,
            crashes: Vec::new(),
        }));
        sessions.insert(session.to_owned(), slot.clone());
        Ok(slot)
    }

    fn supervise(&self, session: &str, slot: &Arc<Slot>, state: &mut Session) {
        if state.active || !state.peer.has_holds() {
            return;
        }
        state.active = true;
        tokio::spawn(Self::run(
            Arc::downgrade(&self.0),
            session.to_owned(),
            slot.clone(),
            state.peer.clone(),
        ));
    }

    async fn run(workers: Weak<Supervisor>, session: String, slot: Arc<Slot>, peer: RemotePeer) {
        while let Some(supervisor) = workers.upgrade() {
            let context = supervisor.context.clone();
            let supervisor = Self(supervisor);
            let generation = supervisor.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
            supervisor.publish(&session, generation, false, None, None);
            let outcome = context
                .await_with(supervisor.start_child(&session, generation, &slot, &peer))
                .await;
            drop(supervisor);
            let result = if let Ok(Ok(Some(handle))) = &outcome {
                handle.wait_exit().await
            } else {
                WorkerState::Stopped
            };
            let Some(supervisor) = workers.upgrade().map(Self) else {
                return;
            };
            let mut state = slot.lock().await;
            state.handle = None;
            let closed = context.is_aborted();
            let stopping = state.stopping.take();
            if stopping.is_some() {
                state.crashes.clear();
                state.failure = None;
            } else if !closed && matches!(outcome, Ok(Err(_))) {
                let failure = ChordError::new("unavailable", "worker failed to start");
                state.failure = Some(failure.clone());
                let _ = peer.fail_child(failure);
            } else if matches!(result, WorkerState::Failed(_)) {
                let now = Instant::now();
                state
                    .crashes
                    .retain(|at| now.duration_since(*at) < CRASH_WINDOW);
                if state.crashes.len() >= CRASH_RESTARTS {
                    let failure = ChordError::new("unavailable", "worker exited");
                    state.failure = Some(failure.clone());
                    let _ = peer.fail_child(failure);
                } else {
                    state.crashes.push(now);
                }
            }
            supervisor.publish(
                &session,
                generation,
                false,
                None,
                state
                    .failure
                    .as_ref()
                    .map(|failure| failure.message.clone())
                    .or_else(|| {
                        (matches!(result, WorkerState::Failed(_)) && stopping.is_none() && !closed)
                            .then(|| "worker exited".into())
                    }),
            );
            if let Some(stopping) = stopping {
                stopping.send_replace(true);
            }
            if closed || state.failure.is_some() || !peer.has_holds() {
                state.active = false;
                return;
            }
            // A hold can arrive just as a pending start is abandoned or a child stops.
        }
    }

    async fn start_child(
        &self,
        session: &str,
        generation: u64,
        slot: &Slot,
        peer: &RemotePeer,
    ) -> Result<Option<Arc<WorkerHandle>>> {
        let mut init =
            (self.0.start)(session.to_owned(), generation, self.0.context.clone()).await?;
        init.generation = generation;
        if init.session()?.id != session {
            return Err(Error::Invalid(
                "Session parameters do not match worker address".into(),
            ));
        }
        crate::runtime::host_dirs::session_directory(&init.data_dir, session)?;
        if !peer.has_holds() || slot.lock().await.stopping.is_some() {
            return Ok(None);
        }
        let wire = session_address(session);
        self.0.processes.retry(&wire).await?;
        let demand = peer.clone();
        let handle = self
            .0
            .processes
            .demand_with_idle_guard(wire, serde_json::to_value(init)?, vec![], move || {
                !demand.has_holds()
            })
            .await?;
        let stopped = {
            let mut state = slot.lock().await;
            state.handle = Some(handle.clone());
            self.publish(session, generation, false, Some(handle.pid), None);
            state.stopping.is_some()
        };
        if stopped {
            handle.shutdown().await?;
            return Ok(Some(handle));
        }
        if let Err(error) = peer.connect_child(
            handle.link.clone(),
            serde_json::to_value(Principal::Worker {
                session: session.to_owned(),
                generation,
            })?,
        ) {
            handle.shutdown().await?;
            handle.wait_exit().await;
            return Err(error);
        }
        if let Err(error) = handle.wait_ready().await {
            handle.shutdown().await?;
            handle.wait_exit().await;
            return Err(error);
        }
        let state = slot.lock().await;
        if state.stopping.is_some() {
            return Ok(Some(handle));
        }
        if let Err(error) = peer.child_ready() {
            drop(state);
            if handle.link.closed() {
                return Ok(Some(handle));
            }
            handle.shutdown().await?;
            handle.wait_exit().await;
            return Err(error);
        }
        self.publish(session, generation, true, Some(handle.pid), None);
        Ok(Some(handle))
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cancel.cancel("session workers dropped");
    }
}

pub(crate) async fn catalog_session(
    host: &Host,
    catalog: Arc<Contract>,
    id: &str,
    context: &Context,
) -> Result<serde_json::Value> {
    use serde_json::{Value, json};

    if host.served_contract(catalog.name()).is_none() {
        return Err(ChordError::new(
            "unavailable",
            "Session parameters unavailable; refusing worker start",
        )
        .into());
    }
    let held = host
        .get(catalog, JsonValue::from_serde(json!({})), context)
        .await?;
    let sessions = held.member("sessions")?;
    let (changed, mut rows) = tokio::sync::watch::channel(());
    let _subscription = sessions.subscribe(
        move |_, _| {
            changed.send_replace(());
            Ok(())
        },
        |_| {},
    );
    // A client-chosen ID may be held before the catalog create commits.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            rows.borrow_and_update();
            let snapshot = sessions.value()?.to_json().to_serde()?;
            if snapshot
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["id"] == id))
            {
                break;
            }
            rows.changed().await.map_err(|_| Error::Closed)?;
        }
        Ok::<_, Error>(())
    })
    .await;
    let row = held
        .call("get", JsonValue::from_serde(json!({"id":id})), context)
        .await?
        .to_serde()?;
    if row["id"] != id {
        return Err(Error::Invalid(
            "Catalog returned a different session".into(),
        ));
    }
    if !row["archivedAt"].is_null() {
        return Err(ChordError::new("forbidden", "Session is archived").into());
    }
    if let Some(env) = row["env"].as_str() {
        return Err(Error::Invalid(format!(
            "Environment {env:?} is not configured; refusing local execution"
        )));
    }
    let result = json!({
        "id": id,
        "cwd": row["cwd"],
        "env": Value::Null,
        "model": row["model"],
        "thinkingLevel": row["thinkingLevel"],
    });
    Ok(result)
}
