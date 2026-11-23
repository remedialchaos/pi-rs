//! Route a session hold to one native worker, retaining the hold across restarts.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

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

struct Supervisor {
    host: Host,
    processes: Workers,
    start: Box<Start>,
    generation: AtomicU64,
    sessions: Mutex<BTreeMap<String, Arc<Slot>>>,
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
            authorizers: Mutex::default(),
            context,
            cancel,
        })))
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
            let outcome = context
                .await_with(supervisor.start_child(&session, &peer))
                .await;
            drop(supervisor);
            let result = match outcome {
                Ok(Ok(Some(handle))) => handle.wait_exit().await,
                Ok(Ok(None)) | Err(_) => WorkerState::Stopped,
                Ok(Err(_)) => {
                    let mut state = slot.lock().await;
                    let failure = ChordError::new("unavailable", "worker failed to start");
                    state.failure = Some(failure.clone());
                    state.active = false;
                    let _ = peer.fail_child(failure);
                    return;
                }
            };
            let mut state = slot.lock().await;
            let closed = context.is_aborted();
            if matches!(result, WorkerState::Failed(_)) {
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
        peer: &RemotePeer,
    ) -> Result<Option<Arc<WorkerHandle>>> {
        let generation = self.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut init =
            (self.0.start)(session.to_owned(), generation, self.0.context.clone()).await?;
        init.generation = generation;
        if init.session()?.id != session {
            return Err(Error::Invalid(
                "Session parameters do not match worker address".into(),
            ));
        }
        crate::runtime::host_dirs::session_directory(&init.data_dir, session)?;
        if !peer.has_holds() {
            return Ok(None);
        }
        let wire = session_address(session);
        self.0.processes.retry(&wire).await?;
        let handle = self
            .0
            .processes
            .demand(wire, serde_json::to_value(init)?, vec![])
            .await?;
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
        handle.wait_ready().await?;
        if let Err(error) = peer.child_ready() {
            handle.shutdown().await?;
            handle.wait_exit().await;
            return Err(error);
        }
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
