//! Server-side demand and caller propagation for native session processes.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{Contract, check_params},
    data::ChordError,
    host::{Caller, Host},
    json::JsonValue,
    node::workers::{WorkerHandle, WorkerOptions, Workers},
    remote::{LinkRole, RemoteOptions, RemotePeer, RemoteReference},
    sync::SyncFuture,
};

use super::worker::WorkerInit;
use crate::api::v1::{Principal, session_address};

type Start = dyn Fn(String, u64, Context) -> SyncFuture<WorkerInit> + Send + Sync;
type Slot = tokio::sync::Mutex<Option<Running>>;

struct Running {
    handle: Arc<WorkerHandle>,
    peer: RemotePeer,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.peer.close();
    }
}

pub struct SessionWorkers {
    host: Host,
    processes: Workers,
    start: Box<Start>,
    generation: AtomicU64,
    sessions: Mutex<BTreeMap<String, Arc<Slot>>>,
}

impl SessionWorkers {
    /// Supply the session catalog and other plugin start values through `start`.
    /// Configure `process` to invoke the application's worker entry point.
    pub fn new(
        host: &Host,
        process: WorkerOptions,
        start: impl Fn(String, u64, Context) -> SyncFuture<WorkerInit> + Send + Sync + 'static,
    ) -> Result<Self> {
        Ok(Self {
            host: host.clone(),
            processes: Workers::new(process)?,
            start: Box::new(start),
            generation: AtomicU64::new(0),
            sessions: Mutex::default(),
        })
    }

    /// Hold a session contract as a caller already identified by the server host.
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
            .cloned()
            .and_then(|value| serde_json::from_value::<Principal>(value).ok());
        if !principal.is_some_and(|principal| {
            principal == Principal::Server
                || (contract.name() != "p2.worker-boundary"
                    && contract.name() != "chord.app.data"
                    && principal.can_view(session))
        }) {
            return Err(ChordError::new("forbidden", "worker address denied").into());
        }
        let slot = self.slot(session);
        let mut running = slot.lock().await;
        if let Some(previous) = running.as_ref()
            && previous.handle.link.closed()
        {
            previous.handle.wait_exit().await;
            if matches!(
                previous.handle.state(),
                pi_rs_chord::node::workers::WorkerState::Failed(_)
            ) {
                previous.handle.wait_ready().await?;
            }
            *running = None;
        }
        if running.is_none() {
            let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
            let mut init = (self.start)(session.to_owned(), generation, context.clone()).await?;
            init.generation = generation;
            if init.session()?.id != session {
                return Err(Error::Invalid(
                    "Session parameters do not match worker address".into(),
                ));
            }
            crate::runtime::host_dirs::session_directory(&init.data_dir, session)?;
            let handle = self
                .processes
                .demand(wire, serde_json::to_value(init)?, vec![])
                .await?;
            let mut options = RemoteOptions::new(serde_json::to_value(Principal::Worker {
                session: session.to_owned(),
                generation,
            })?);
            options.role = LinkRole::Child;
            options.trusted = true;
            let peer = RemotePeer::serve(self.host.clone(), handle.link.clone(), options)?;
            *running = Some(Running { handle, peer });
        }
        let running = running.as_ref().ok_or(Error::Closed)?;
        running.handle.wait_ready().await?;
        running.peer.hold_as(contract, params, caller, context)
    }

    pub async fn retry(&self, session: &str) -> Result<()> {
        let slot = self.slot(session);
        let mut running = slot.lock().await;
        self.processes.retry(&session_address(session)).await?;
        *running = None;
        Ok(())
    }

    pub async fn close(&self) {
        self.processes.close().await;
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
    }

    fn slot(&self, session: &str) -> Arc<Slot> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .entry(session.to_owned())
            .or_default()
            .clone()
    }
}
