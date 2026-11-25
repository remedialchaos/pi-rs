//! The local runner: each session's worker as a process on this machine. Per session one operation at
//! a time: a start waits for the previous worker's exit. A worker that crashes fails its session until
//! `retry` or a rollout (no restart loop); one that stopped itself idle, or was stopped, starts again
//! on the next start. The rollout step reloads every running worker that runs another set of scripts
//! and clears the failures.
//!
//! Upstream forks `worker.ts` and keeps a Chord IPC link beside the control channel. This port starts
//! the same binary in its worker mode and speaks the control messages of [`crate::api::v1::control`]
//! over its stdio; the launcher is a trait so tests can run a worker in this process and so the
//! `2.1b-3` routing seam has one place to grow into.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_chord::{context::Context, link::Link};
use tokio::{
    process::Command,
    sync::{Mutex as AsyncMutex, mpsc, oneshot, watch},
};

use crate::api::v1::control::{
    FromWorker, IpcFrame, IpcPipeLink, ToWorker, read_ipc_frame, worker_control, worker_message,
    write_ipc_frame,
};
use crate::install::WorkerSpec;

/// One plugin's identity for a worker: what it runs, as a digest names it.
pub type PluginScript = String;

/// A worker or runner operation's future.
pub type RunnerFuture<T> = Pin<Box<dyn Future<Output = Result<T, RunnerError>> + Send + 'static>>;

/// How a worker process starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerLaunch {
    pub session: String,
    pub spec: WorkerSpec,
    /// Added to the server's environment (the compile cache's directory).
    pub env: Vec<(String, String)>,
}

/// How a worker's exit ended: asked (a stop, or the worker leaving idle), or a crash with its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Clean,
    Crash { reason: String },
}

/// What a worker is doing, as its runner sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Starting { pid: u32 },
    Running { pid: u32 },
    Stopping { pid: u32 },
    Failed { reason: String },
}

/// A reload's outcome on one host, as a rollout step lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RolloutHost {
    Reloaded { host: String, failed: Vec<String> },
    Stopped { host: String },
    Failed { host: String, error: String },
}

/// One host kind's part of a rollout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolloutStep {
    pub name: String,
    pub hosts: Vec<RolloutHost>,
}

/// A rollout's result: what each step did, and the step that stopped it, if one did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolloutResult {
    pub steps: Vec<RolloutStep>,
    pub stopped: Option<String>,
}

/// What the runner reads from the server: the Pi's places, and how its workers run.
pub trait WorkerEnvironment: Send + Sync {
    fn data_dir(&self) -> &Path;
    fn worker_spec(&self) -> &WorkerSpec;
    fn worker_idle_ms(&self) -> u64;
    fn worker_scripts(&self, context: &Context) -> RunnerFuture<Vec<PluginScript>>;
}

/// How a session's worker runs: started for the route, and the rollout step that ships plugin reloads
/// to it.
pub trait WorkerRunner: Send + Sync {
    /// Starts the worker of session `id` (or returns the running one) and gives the link to its host.
    fn start(&self, session: &str, context: &Context) -> RunnerFuture<WorkerLink>;
    /// Stops the worker of session `id`, if one runs; resolves once it exited.
    fn stop(&self, session: &str) -> RunnerFuture<()>;
    /// Clears session `id`'s failure: it starts again on its next start.
    fn retry(&self, session: &str);
    /// What runs: each session's worker by id.
    fn state(&self) -> IndexMap<String, WorkerState>;
    /// Ships `scripts` to every running worker that runs another set, and clears the failures.
    fn rollout(&self, scripts: &[PluginScript], context: &Context) -> RunnerFuture<RolloutStep>;
    /// Stops every worker: the runner's object ends.
    fn close(&self) -> RunnerFuture<()>;
}

/// A session's worker as the route reached it.
#[derive(Clone)]
pub struct WorkerLink {
    pub session: String,
    pub pid: u32,
    pub link: Option<Arc<dyn Link>>,
}

impl std::fmt::Debug for WorkerLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerLink")
            .field("session", &self.session)
            .field("pid", &self.pid)
            .field("has_link", &self.link.is_some())
            .finish()
    }
}

/// What the runner refused.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("p3: session {session}: worker {reason}")]
    Failed { session: String, reason: String },
    #[error("p3: session {session}: worker exited before it was ready")]
    NotReady { session: String },
    #[error("p3: session {session}: worker did not start: {source}")]
    Launch {
        session: String,
        #[source]
        source: std::io::Error,
    },
    #[error("p3: session {session}: worker channel failed: {source}")]
    Channel {
        session: String,
        #[source]
        source: std::io::Error,
    },
    #[error("p3: session {session}: no runner environment is set")]
    NoEnvironment { session: String },
}

/// A worker's process, as the runner reaches it: its pid, its control channel, and its exit.
pub trait WorkerTransport: Send + Sync {
    fn pid(&self) -> Option<u32>;
    fn link(&self) -> Option<Arc<dyn Link>> {
        None
    }
    fn send(&self, message: ToWorker) -> RunnerFuture<()>;
    fn recv(&self) -> RunnerFuture<Option<FromWorker>>;
    fn stop(&self) -> RunnerFuture<()>;
    fn kill(&self) -> RunnerFuture<()>;
    fn wait(&self) -> RunnerFuture<u32>;
}

/// Starts a worker's process.
pub trait WorkerLauncher: Send + Sync {
    fn launch(&self, launch: WorkerLaunch) -> Result<Arc<dyn WorkerTransport>, RunnerError>;
}

/// The launcher of this install: the worker as a child process with its stdio for control messages.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessLauncher;

impl WorkerLauncher for ProcessLauncher {
    fn launch(&self, launch: WorkerLaunch) -> Result<Arc<dyn WorkerTransport>, RunnerError> {
        let mut command = Command::new(&launch.spec.executable);
        command
            .args(&launch.spec.args)
            .envs(launch.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| RunnerError::Launch {
            session: launch.session.clone(),
            source,
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let pid = child.id();
        match (stdin, stdout) {
            (Some(mut stdin), Some(mut stdout)) => {
                let (exit_code, exit) = watch::channel(None);
                tokio::spawn(async move {
                    let code = child
                        .wait()
                        .await
                        .ok()
                        .and_then(|status| status.code())
                        .map(|code| code.max(0) as u32)
                        .unwrap_or(1);
                    exit_code.send_replace(Some(code));
                });
                let (out_tx, mut out_rx) = mpsc::channel::<IpcFrame>(64);
                let (ctrl_tx, ctrl_rx) = mpsc::channel::<FromWorker>(32);
                let link = IpcPipeLink::new(out_tx.clone());
                let reader_link = link.clone();
                tokio::spawn(async move {
                    while let Some(frame) = out_rx.recv().await {
                        if write_ipc_frame(&mut stdin, &frame).await.is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    while let Ok(frame) = read_ipc_frame(&mut stdout).await {
                        match frame {
                            IpcFrame::Binary(bytes) => reader_link.deliver(bytes),
                            IpcFrame::Control(bytes) => {
                                if let Ok(value) =
                                    serde_json::from_slice::<serde_json::Value>(&bytes)
                                    && let Some(message) = worker_control::<FromWorker>(&value)
                                    && ctrl_tx.send(message).await.is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    reader_link.ended();
                });
                Ok(Arc::new(ProcessTransport {
                    pid,
                    outgoing: out_tx,
                    controls: Arc::new(AsyncMutex::new(ctrl_rx)),
                    link,
                    exit,
                }))
            }
            _ => {
                let _ = child.start_kill();
                Err(RunnerError::Launch {
                    session: launch.session,
                    source: std::io::Error::other("worker has no stdio"),
                })
            }
        }
    }
}

struct ProcessTransport {
    pid: Option<u32>,
    outgoing: mpsc::Sender<IpcFrame>,
    controls: Arc<AsyncMutex<mpsc::Receiver<FromWorker>>>,
    link: Arc<IpcPipeLink>,
    exit: watch::Receiver<Option<u32>>,
}

impl ProcessTransport {
    fn channel_error(session: String, source: std::io::Error) -> RunnerError {
        RunnerError::Channel { session, source }
    }
}

impl WorkerTransport for ProcessTransport {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn link(&self) -> Option<Arc<dyn Link>> {
        Some(self.link.clone())
    }

    fn send(&self, message: ToWorker) -> RunnerFuture<()> {
        let outgoing = self.outgoing.clone();
        let session = self.pid.map(|pid| pid.to_string()).unwrap_or_default();
        Box::pin(async move {
            let bytes = serde_json::to_vec(&worker_message(&message)).map_err(|err| {
                ProcessTransport::channel_error(session.clone(), std::io::Error::other(err))
            })?;
            outgoing.send(IpcFrame::Control(bytes)).await.map_err(|_| {
                ProcessTransport::channel_error(
                    session,
                    std::io::Error::other("worker input is closed"),
                )
            })
        })
    }

    fn recv(&self) -> RunnerFuture<Option<FromWorker>> {
        let controls = self.controls.clone();
        Box::pin(async move { Ok(controls.lock().await.recv().await) })
    }

    fn stop(&self) -> RunnerFuture<()> {
        self.send(ToWorker::Shutdown)
    }

    fn kill(&self) -> RunnerFuture<()> {
        let pid = self.pid;
        Box::pin(async move {
            // The child waits in a task of its own (a `wait` under a shared lock would deadlock a
            // kill). Without `libc::kill` (unsafe, forbidden here) the system utility signals it.
            if let Some(pid) = pid {
                let _ = std::process::Command::new("kill")
                    .arg("-KILL")
                    .arg(pid.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            Ok(())
        })
    }

    fn wait(&self) -> RunnerFuture<u32> {
        let mut exit = self.exit.clone();
        Box::pin(async move {
            loop {
                if let Some(code) = *exit.borrow_and_update() {
                    return Ok(code);
                }
                if exit.changed().await.is_err() {
                    return Ok(1);
                }
            }
        })
    }
}

/// How long a worker may take to be ready before it is killed, and how long a stopping worker gets
/// before it is killed.
#[derive(Clone, Copy, Debug)]
pub struct RunnerOptions {
    pub start_timeout: Duration,
    pub stop_grace: Duration,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(60),
            stop_grace: Duration::from_secs(5),
        }
    }
}

/// What a worker is doing to its runner.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Running,
    Stopping,
    Exited(Exit),
}

struct Worker {
    session: String,
    pid: u32,
    phase: watch::Sender<Phase>,
    shipped: Mutex<String>,
    transport: Arc<dyn WorkerTransport>,
    pending_reload: Mutex<Option<oneshot::Sender<FromWorker>>>,
}

impl Worker {
    fn phase(&self) -> Phase {
        self.phase.borrow().clone()
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    fn state(&self) -> Option<WorkerState> {
        match self.phase() {
            Phase::Starting => Some(WorkerState::Starting { pid: self.pid }),
            Phase::Running => Some(WorkerState::Running { pid: self.pid }),
            Phase::Stopping => Some(WorkerState::Stopping { pid: self.pid }),
            Phase::Exited(_) => None,
        }
    }

    fn handle(&self, message: FromWorker) {
        match message {
            FromWorker::Ready { failed } => {
                if failed.is_empty() {
                    self.set_phase(Phase::Running);
                } else {
                    eprintln!("p3 runner: {}: plugins failed: {failed:?}", self.session);
                    self.set_phase(Phase::Running);
                }
            }
            FromWorker::Closed { .. } => self.set_phase(Phase::Stopping),
            FromWorker::Reloaded { failed } => {
                if let Some(sender) = self
                    .pending_reload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = sender.send(FromWorker::Reloaded { failed });
                }
            }
            FromWorker::ReloadFailed { error } => {
                if let Some(sender) = self
                    .pending_reload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = sender.send(FromWorker::ReloadFailed { error });
                }
            }
        }
    }

    async fn wait_ready(&self) -> Result<(), Exit> {
        let mut phase = self.phase.subscribe();
        loop {
            match phase.borrow_and_update().clone() {
                Phase::Running => return Ok(()),
                Phase::Exited(exit) => return Err(exit),
                Phase::Starting | Phase::Stopping => {}
            }
            if phase.changed().await.is_err() {
                return Err(Exit::Crash {
                    reason: "worker channel closed".into(),
                });
            }
        }
    }

    async fn wait_exit(&self) -> Exit {
        let mut phase = self.phase.subscribe();
        loop {
            if let Phase::Exited(exit) = phase.borrow_and_update().clone() {
                return exit;
            }
            if phase.changed().await.is_err() {
                return Exit::Crash {
                    reason: "worker channel closed".into(),
                };
            }
        }
    }

    async fn reload(&self, scripts: Vec<PluginScript>) -> FromWorker {
        let (sender, receiver) = oneshot::channel();
        *self
            .pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
        if let Err(error) = self.transport.send(ToWorker::Reload { scripts }).await {
            return FromWorker::ReloadFailed {
                error: error.to_string(),
            };
        }
        match receiver.await {
            Ok(message) => message,
            Err(_) => FromWorker::ReloadFailed {
                error: "worker: exited".into(),
            },
        }
    }
}

struct Queue {
    operation: AsyncMutex<()>,
    worker: Mutex<Option<Arc<Worker>>>,
    failed: Mutex<Option<String>>,
}

impl Queue {
    fn new() -> Self {
        Self {
            operation: AsyncMutex::new(()),
            worker: Mutex::new(None),
            failed: Mutex::new(None),
        }
    }

    fn worker(&self) -> Option<Arc<Worker>> {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_worker(&self, worker: Option<Arc<Worker>>) {
        *self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = worker;
    }

    fn failed(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_failed(&self, reason: Option<String>) {
        *self
            .failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
    }
}

/// The local runner: workers as processes of this machine, one per session.
#[derive(Clone)]
pub struct LocalRunner {
    environment: Arc<OnceLock<Arc<dyn WorkerEnvironment>>>,
    launcher: Arc<dyn WorkerLauncher>,
    options: RunnerOptions,
    queues: Arc<Mutex<BTreeMap<String, Arc<Queue>>>>,
}

impl LocalRunner {
    pub fn new(launcher: Arc<dyn WorkerLauncher>, options: RunnerOptions) -> Self {
        Self {
            environment: Arc::new(OnceLock::new()),
            launcher,
            options,
            queues: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Sets the server values the runner reads; called once, when the server is built.
    pub fn set_environment(&self, environment: Arc<dyn WorkerEnvironment>) {
        let _ = self.environment.set(environment);
    }

    fn environment(&self, session: &str) -> Result<Arc<dyn WorkerEnvironment>, RunnerError> {
        self.environment
            .get()
            .cloned()
            .ok_or_else(|| RunnerError::NoEnvironment {
                session: session.to_string(),
            })
    }

    fn queue(&self, session: &str) -> Arc<Queue> {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues
            .entry(session.to_string())
            .or_insert_with(|| Arc::new(Queue::new()))
            .clone()
    }

    fn spawn_monitor(transport: Arc<dyn WorkerTransport>, worker: Arc<Worker>, queue: Arc<Queue>) {
        tokio::spawn(async move {
            let mut code = None;
            loop {
                tokio::select! {
                    message = transport.recv() => {
                        match message {
                            Ok(Some(message)) => worker.handle(message),
                            _ => break,
                        }
                    }
                    exited = transport.wait() => {
                        code = exited.ok();
                        break;
                    }
                }
            }
            if code.is_none() {
                code = transport.wait().await.ok();
            }
            let code = code.unwrap_or(1);
            let exit = match worker.phase() {
                Phase::Stopping | Phase::Exited(Exit::Clean) => Exit::Clean,
                _ => Exit::Crash {
                    reason: format!("exited (code {code})"),
                },
            };
            worker.set_phase(Phase::Exited(exit.clone()));
            if let Exit::Crash { reason } = &exit {
                queue.set_failed(Some(reason.clone()));
                eprintln!("p3 runner: {}: worker {reason}", worker.session);
            }
            let current = queue.worker();
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &worker))
            {
                queue.set_worker(None);
            }
            if let Some(link) = transport.link() {
                link.close();
            }
        });
    }

    async fn stop_worker(&self, worker: &Worker) {
        worker.set_phase(Phase::Stopping);
        let _ = worker.transport.stop().await;
        let transport = worker.transport.clone();
        let grace = self.options.stop_grace;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let _ = transport.kill().await;
        });
        let _ = worker.wait_exit().await;
    }
}

impl WorkerRunner for LocalRunner {
    fn start(&self, session: &str, context: &Context) -> RunnerFuture<WorkerLink> {
        let this = self.clone();
        let session = session.to_string();
        let context = context.clone();
        Box::pin(async move {
            let queue = this.queue(&session);
            let _operation = queue.operation.lock().await;
            if let Some(worker) = queue.worker() {
                return Ok(WorkerLink {
                    session,
                    pid: worker.pid,
                    link: worker.transport.link(),
                });
            }
            if let Some(reason) = queue.failed() {
                return Err(RunnerError::Failed { session, reason });
            }
            let environment = this.environment(&session)?;
            let dir = environment.data_dir().join("workers").join(&session);
            let scripts = environment.worker_scripts(&context).await?;
            let spec = environment.worker_spec().clone();
            let idle_ms = environment.worker_idle_ms();
            let cache = environment.data_dir().join("cache");
            let transport = this.launcher.launch(WorkerLaunch {
                session: session.clone(),
                spec,
                env: vec![(
                    "P3_COMPILE_CACHE".into(),
                    cache.to_string_lossy().into_owned(),
                )],
            })?;
            let pid = transport.pid().unwrap_or(0);
            let (phase, _receiver) = watch::channel(Phase::Starting);
            let worker = Arc::new(Worker {
                session: session.clone(),
                pid,
                phase,
                shipped: Mutex::new(scripts_key(&scripts)),
                transport: transport.clone(),
                pending_reload: Mutex::new(None),
            });
            transport
                .send(ToWorker::Start {
                    session: session.clone(),
                    dir: dir.to_string_lossy().into_owned(),
                    idle_ms,
                    scripts,
                })
                .await?;
            queue.set_worker(Some(worker.clone()));
            LocalRunner::spawn_monitor(transport.clone(), worker.clone(), queue.clone());
            match tokio::time::timeout(this.options.start_timeout, worker.wait_ready()).await {
                Ok(Ok(())) => Ok(WorkerLink {
                    session,
                    pid,
                    link: transport.link(),
                }),
                Ok(Err(_exit)) => {
                    let reason = queue
                        .failed()
                        .unwrap_or_else(|| "exited before it was ready".into());
                    Err(RunnerError::Failed {
                        session: session.clone(),
                        reason,
                    })
                }
                Err(_) => {
                    let _ = transport.kill().await;
                    let _ = worker.wait_exit().await;
                    queue.set_failed(Some("did not become ready".into()));
                    Err(RunnerError::NotReady { session })
                }
            }
        })
    }

    fn stop(&self, session: &str) -> RunnerFuture<()> {
        let this = self.clone();
        let session = session.to_string();
        Box::pin(async move {
            let queue = this.queue(&session);
            let _operation = queue.operation.lock().await;
            if let Some(worker) = queue.worker() {
                this.stop_worker(&worker).await;
            }
            Ok(())
        })
    }

    fn retry(&self, session: &str) {
        self.queue(session).set_failed(None);
    }

    fn state(&self) -> IndexMap<String, WorkerState> {
        let queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut state = IndexMap::new();
        for (session, queue) in queues {
            if let Some(reason) = queue.failed() {
                state.insert(session, WorkerState::Failed { reason });
            } else if let Some(worker) = queue.worker()
                && let Some(shown) = worker.state()
            {
                state.insert(session, shown);
            }
        }
        state
    }

    fn rollout(&self, scripts: &[PluginScript], _context: &Context) -> RunnerFuture<RolloutStep> {
        let this = self.clone();
        let scripts = scripts.to_vec();
        Box::pin(async move {
            let queues = this
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let key = scripts_key(&scripts);
            let mut hosts = Vec::new();
            for (session, queue) in queues {
                if queue.failed().is_some() {
                    queue.set_failed(None);
                    continue;
                }
                let Some(worker) = queue.worker() else {
                    continue;
                };
                if worker.phase() != Phase::Running {
                    continue;
                }
                let shipped = worker
                    .shipped
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if shipped == key {
                    continue;
                }
                match worker.reload(scripts.clone()).await {
                    FromWorker::Reloaded { failed } => {
                        if failed.is_empty() {
                            *worker
                                .shipped
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = key.clone();
                        }
                        hosts.push(RolloutHost::Reloaded {
                            host: session,
                            failed,
                        });
                    }
                    FromWorker::ReloadFailed { error } => hosts.push(RolloutHost::Failed {
                        host: session,
                        error,
                    }),
                    _ => hosts.push(RolloutHost::Failed {
                        host: session,
                        error: "worker: answered a reload with no report".into(),
                    }),
                }
            }
            Ok(RolloutStep {
                name: "workers".into(),
                hosts,
            })
        })
    }

    fn close(&self) -> RunnerFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            let workers = this
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter_map(|queue| queue.worker())
                .collect::<Vec<_>>();
            for worker in workers {
                this.stop_worker(&worker).await;
            }
            Ok(())
        })
    }
}

/// A set of scripts' identity: its plugins and digests.
pub fn scripts_key(scripts: &[PluginScript]) -> String {
    let mut sorted = scripts.to_vec();
    sorted.sort();
    sorted.join("\n")
}

/// The worker's private directory inside a Pi.
pub fn worker_dir(data_dir: &Path, session: &str) -> PathBuf {
    data_dir.join("workers").join(session)
}
