use super::{
    location::{Location, LocationInput, is_rooted},
    session_worker::SessionInit,
};
use pi_rs_chord::context::Context;
use pi_rs_durable::env::{ExecutionEnv, FileKind, LocalEnvOptions};
use pi_rs_durable::{
    ConversationId, DocToken, DocumentAddress, JsonObject, ROOT_CONVERSATION_ID, Session,
};
use pi_rs_env::{Connection, ConnectionOptions, RemoteExecutionEnvOptions};
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum WorkerEnvironmentError {
    #[error("The session's Harness is not open")]
    HarnessClosed,
    #[error("The session's environments are closed")]
    EnvironmentsClosed,
    #[error("Session location closed")]
    LocationClosed,
    #[error(transparent)]
    Cancelled(#[from] pi_rs_chord::context::AbortError),
    #[error("document read failed: {0}")]
    Document(#[from] pi_rs_durable::session::Error),
    #[error("{0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("{0}")]
    Placement(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("not an existing directory")]
    Filesystem(#[from] pi_rs_durable::env::FileError),
    #[error("not an existing directory")]
    NotDirectory,
}
pub fn spawn_location(session: &SessionInit) -> Location {
    session.location()
}

pub struct SessionLocation(Mutex<LocationState>);
struct LocationState {
    value: Location,
    version: u64,
    subscribers: Vec<mpsc::UnboundedSender<Location>>,
}
impl SessionLocation {
    pub fn new(value: Location) -> Self {
        Self(Mutex::new(LocationState {
            value,
            version: 0,
            subscribers: Vec::new(),
        }))
    }
    fn state(&self) -> MutexGuard<'_, LocationState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn value(&self) -> Location {
        self.state().value.clone()
    }
    pub fn version(&self) -> u64 {
        self.state().version
    }
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Location> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = self.state();
        state.subscribers.retain(|s| !s.is_closed());
        state.subscribers.push(tx);
        rx
    }
    pub fn next(
        &self,
        ctx: &Context,
    ) -> impl Future<Output = Result<Location, WorkerEnvironmentError>> + use<> {
        let mut rx = self.subscribe();
        let ctx = ctx.clone();
        async move {
            ctx.await_with(rx.recv())
                .await?
                .ok_or(WorkerEnvironmentError::LocationClosed)
        }
    }
    pub fn set(&self, next: Location) {
        let mut state = self.state();
        state.version += 1;
        state.subscribers.retain(|s| s.send(next.clone()).is_ok());
        state.value = next;
    }
}

pub enum Machine {
    Local(Option<Arc<Connection>>),
    Remote {
        id: String,
        connection: Arc<Connection>,
        shell_path: Option<String>,
    },
}
pub fn local_machine() -> Machine {
    Machine::Local(
        pi_rs_env::local_daemon()
            .ok()
            .filter(|p| p.exists())
            .map(|p| {
                Arc::new(Connection::new(ConnectionOptions::new(vec![
                    p.to_string_lossy().into_owned(),
                ])))
            }),
    )
}
impl Machine {
    fn at(&self, cwd: &str) -> Arc<dyn ExecutionEnv> {
        match self {
            Self::Local(connection) => {
                let options = LocalEnvOptions {
                    cwd: cwd.into(),
                    shell_path: std::env::var("SHELL").ok(),
                    ..Default::default()
                };
                match connection {
                    Some(c) => Arc::new(pi_rs_env::LocalExecutionEnv::new(options, c.clone())),
                    None => Arc::new(pi_rs_durable::env::LocalExecutionEnv::with_options(options)),
                }
            }
            Self::Remote {
                id,
                connection,
                shell_path,
            } => Arc::new(pi_rs_env::RemoteExecutionEnv::new(
                RemoteExecutionEnvOptions {
                    id: id.clone(),
                    cwd: cwd.into(),
                    connection: connection.clone(),
                    shell_path: shell_path.clone(),
                    shell_env: Default::default(),
                    watch: Default::default(),
                },
            )),
        }
    }
    fn close(self) {
        match self {
            Self::Remote { connection, .. } | Self::Local(Some(connection)) => connection.close(),
            Self::Local(None) => {}
        }
    }
}

pub trait EnvironmentProvider: Send + Sync {
    fn open(&self, name: &str) -> Result<Machine, WorkerEnvironmentError>;
    fn sandbox(&self, name: &str) -> Result<bool, WorkerEnvironmentError>;
}
pub type PlacementFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, WorkerEnvironmentError>> + Send + 'a>>;
pub trait PlacementReader: Send + Sync {
    fn snapshot<'a>(
        &'a self,
        definition: &'a DocToken,
        address: &'a DocumentAddress,
        ctx: &'a Context,
    ) -> PlacementFuture<'a, Option<Arc<JsonObject>>>;
}
impl PlacementReader for Session {
    fn snapshot<'a>(
        &'a self,
        definition: &'a DocToken,
        address: &'a DocumentAddress,
        ctx: &'a Context,
    ) -> PlacementFuture<'a, Option<Arc<JsonObject>>> {
        Box::pin(async move {
            Session::snapshot(self, definition, address, ctx)
                .await
                .map_err(WorkerEnvironmentError::from)
        })
    }
}
pub trait ConversationPlacement: Send + Sync {
    fn own<'a>(
        &'a self,
        read: &'a dyn PlacementReader,
        id: ConversationId,
        ctx: &'a Context,
    ) -> PlacementFuture<'a, Option<Location>>;
}
type PlacementLookup = dyn Fn() -> Option<Arc<dyn ConversationPlacement>> + Send + Sync;
pub struct Execution {
    session: Location,
    provider: Arc<dyn EnvironmentProvider>,
    placement: Box<PlacementLookup>,
    reader: Option<Arc<dyn PlacementReader>>,
    closed: bool,
    machines: HashMap<Option<String>, Machine>,
    environments: HashMap<(Option<String>, String), Arc<dyn ExecutionEnv>>,
}
impl Execution {
    pub fn new(session: Location, provider: Arc<dyn EnvironmentProvider>) -> Self {
        Self {
            session,
            provider,
            placement: Box::new(|| None),
            reader: None,
            closed: false,
            machines: Default::default(),
            environments: Default::default(),
        }
    }
    pub fn set_placement_lookup(
        &mut self,
        lookup: impl Fn() -> Option<Arc<dyn ConversationPlacement>> + Send + Sync + 'static,
    ) {
        self.placement = Box::new(lookup);
    }
    pub fn set_reader(&mut self, reader: Option<Arc<dyn PlacementReader>>) {
        self.reader = reader;
    }
    pub fn session(&self) -> &Location {
        &self.session
    }
    pub fn switch(&mut self, next: Location) {
        self.session = next
    }
    pub fn sandbox(&self, env: Option<&str>) -> Result<Option<String>, WorkerEnvironmentError> {
        match env {
            Some(n) if self.provider.sandbox(n)? => Ok(Some(n.into())),
            _ => Ok(None),
        }
    }
    /// Probes whether `location`'s machine runs Windows (`None` if remote machine is unreachable).
    pub fn platform_is_windows(&mut self, location: &Location) -> Option<bool> {
        let Some(name) = location.env.as_deref() else {
            return Some(cfg!(windows));
        };
        let machine = match self.machines.entry(Some(name.to_owned())) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let opened = self.provider.open(name).ok()?;
                entry.insert(opened)
            }
        };
        match machine {
            Machine::Local(_) => Some(cfg!(windows)),
            Machine::Remote { connection, .. } => {
                connection.info().ok().map(|info| info.os == "windows")
            }
        }
    }
    pub fn at(
        &mut self,
        location: &Location,
    ) -> Result<Arc<dyn ExecutionEnv>, WorkerEnvironmentError> {
        if self.closed {
            return Err(WorkerEnvironmentError::EnvironmentsClosed);
        }
        let key = (location.env.clone(), location.cwd.clone());
        if let Some(env) = self.environments.get(&key) {
            return Ok(env.clone());
        }
        let machine = match self.machines.entry(location.env.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(match location.env.as_deref() {
                    Some(n) => self.provider.open(n)?,
                    None => local_machine(),
                })
            }
        };
        let env = machine.at(&location.cwd);
        self.environments.insert(key, env.clone());
        Ok(env)
    }
    pub async fn retire(&mut self, used: &[Location]) {
        let keys: Vec<_> = self
            .environments
            .keys()
            .filter(|(env, cwd)| !used.iter().any(|l| &l.env == env && &l.cwd == cwd))
            .cloned()
            .collect();
        for k in keys {
            if let Some(env) = self.environments.remove(&k) {
                env.cleanup(&Context::background()).await
            }
        }
        let keys: Vec<_> = self
            .machines
            .keys()
            .filter(|env| !used.iter().any(|l| &l.env == *env))
            .cloned()
            .collect();
        for k in keys {
            if let Some(m) = self.machines.remove(&k) {
                m.close()
            }
        }
    }
    pub async fn close(&mut self) {
        self.closed = true;
        self.retire(&[]).await
    }
}
pub async fn locate_conversation(
    execution: Option<&Execution>,
    id: ConversationId,
    ctx: &Context,
    read: Option<&dyn PlacementReader>,
) -> Result<Location, WorkerEnvironmentError> {
    let execution = execution.ok_or(WorkerEnvironmentError::HarnessClosed)?;
    if id != ROOT_CONVERSATION_ID {
        let documents = read
            .or(execution.reader.as_deref())
            .ok_or(WorkerEnvironmentError::HarnessClosed)?;
        if let Some(plugin) = (execution.placement)()
            && let Some(location) = ctx.await_with(plugin.own(documents, id, ctx)).await??
        {
            return Ok(location);
        }
    }
    Ok(execution.session.clone())
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Confinement {
    pub sandbox: String,
}
pub async fn conversation_confinement(
    execution: &Execution,
    id: ConversationId,
    ctx: &Context,
    read: Option<&dyn PlacementReader>,
) -> Result<Option<Confinement>, WorkerEnvironmentError> {
    let location = locate_conversation(Some(execution), id, ctx, read).await?;
    Ok(execution
        .sandbox(location.env.as_deref())?
        .map(|sandbox| Confinement { sandbox }))
}
pub async fn conversation_environment(
    execution: &mut Execution,
    id: ConversationId,
    ctx: &Context,
    read: Option<&dyn PlacementReader>,
) -> Result<(Location, Arc<dyn ExecutionEnv>), WorkerEnvironmentError> {
    let location = locate_conversation(Some(execution), id, ctx, read).await?;
    let env = execution.at(&location)?;
    Ok((location, env))
}
pub async fn resolve_directory(
    execution: &mut Execution,
    target: &LocationInput,
    from: &Location,
    ctx: &Context,
) -> Result<Location, WorkerEnvironmentError> {
    let same = target.env == from.env;
    let typed = target.path.trim();
    let path = if typed.is_empty() {
        if same { from.cwd.clone() } else { "~".into() }
    } else if same || is_rooted(typed) {
        typed.into()
    } else {
        format!("~/{typed}")
    };
    let env = execution.at(&Location {
        env: target.env.clone(),
        cwd: if same { from.cwd.clone() } else { "/".into() },
    })?;
    let cwd = env.absolute_path(&path, ctx).await?;
    let canonical = env.canonical_path(&cwd, ctx).await?;
    if env.file_info(&canonical, ctx).await?.kind != FileKind::Directory {
        return Err(WorkerEnvironmentError::NotDirectory);
    }
    Ok(Location {
        env: target.env.clone(),
        cwd,
    })
}
