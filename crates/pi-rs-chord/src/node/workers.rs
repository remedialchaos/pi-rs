//! Demand-started native workers. Control JSON and binary Chord frames share bounded stdio IPC.
use crate::{
    Error, Result,
    link::{Link, LinkEnd},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, watch},
};

pub const MAX_FRAME: usize = 17 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeliveredPlugin {
    pub name: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}
impl DeliveredPlugin {
    pub fn validate(&self) -> Result<()> {
        if super::plugins::digest(&self.bytes) != self.digest {
            return Err(Error::Invalid("worker plugin digest mismatch".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    Start {
        address: serde_json::Value,
        start: serde_json::Value,
        scripts: Vec<DeliveredPlugin>,
        lock: PathBuf,
    },
    Shutdown,
    Reload {
        scripts: Vec<DeliveredPlugin>,
    },
    Started,
    Ready {
        idle: bool,
    },
    Idle {
        idle: bool,
    },
    Fatal {
        message: String,
    },
    Reloaded,
    ReloadFailed {
        message: String,
    },
}
#[derive(Clone, Debug)]
pub enum Frame {
    Control(Control),
    Binary(Vec<u8>),
}
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> Result<()> {
    let (tag, body) = match frame {
        Frame::Control(control) => (
            0,
            serde_json::to_vec(&serde_json::json!({"chord.workers":control}))?,
        ),
        Frame::Binary(bytes) => (1, bytes.clone()),
    };
    if body.len() >= MAX_FRAME {
        return Err(Error::Invalid("worker IPC frame too large".into()));
    }
    writer.write_u32((body.len() + 1) as u32).await?;
    writer.write_u8(tag).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let length = reader.read_u32().await? as usize;
    if !(1..=MAX_FRAME).contains(&length) {
        return Err(Error::Invalid("invalid worker IPC frame length".into()));
    }
    let tag = reader.read_u8().await?;
    let mut body = vec![0; length - 1];
    reader.read_exact(&mut body).await?;
    match tag {
        0 => {
            let value: serde_json::Value = serde_json::from_slice(&body)?;
            Ok(Frame::Control(serde_json::from_value(
                value
                    .get("chord.workers")
                    .cloned()
                    .ok_or_else(|| Error::Invalid("missing worker control envelope".into()))?,
            )?))
        }
        1 => Ok(Frame::Binary(body)),
        _ => Err(Error::Invalid("invalid worker IPC tag".into())),
    }
}
/// Kernel-owned advisory file lock. The inode is intentionally retained after unlock.
pub struct ProcessLock {
    file: std::fs::File,
}
impl ProcessLock {
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
    pub async fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(lock) = Self::try_acquire(path)? {
                    return Ok(lock);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| Error::Invalid("worker lock timed out".into()))?
    }
}
impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
struct PipeLink {
    end: LinkEnd,
    outgoing: mpsc::Sender<Frame>,
}
impl Link for PipeLink {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if self.closed() {
            return Err(Error::Closed);
        }
        if bytes.len() >= MAX_FRAME || self.outgoing.try_send(Frame::Binary(bytes)).is_err() {
            self.close();
            return Err(Error::Invalid("worker IPC backpressure".into()));
        }
        Ok(())
    }
    fn close(&self) {
        self.end.ended();
        let _ = self.outgoing.try_send(Frame::Control(Control::Shutdown));
    }
    fn closed(&self) -> bool {
        self.end.closed()
    }
    fn on_message(&self, f: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.end.on_message(f)
    }
    fn on_close(&self, f: Arc<dyn Fn() + Send + Sync>) {
        self.end.on_close(f)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Ready,
    Stopped,
    Failed(String),
}
pub struct WorkerHandle {
    pub link: Arc<dyn Link>,
    pub pid: u32,
    state: watch::Receiver<WorkerState>,
    outgoing: mpsc::Sender<Frame>,
}
impl WorkerHandle {
    pub fn state(&self) -> WorkerState {
        self.state.borrow().clone()
    }
    pub async fn wait_ready(&self) -> Result<()> {
        let mut state = self.state.clone();
        loop {
            let current = state.borrow().clone();
            match current {
                WorkerState::Ready => return Ok(()),
                WorkerState::Stopped => return Err(Error::Closed),
                WorkerState::Failed(error) => return Err(Error::Invalid(error)),
                WorkerState::Starting => {}
            }
            state.changed().await.map_err(|_| Error::Closed)?;
        }
    }
    pub async fn wait_exit(&self) -> WorkerState {
        let mut state = self.state.clone();
        loop {
            let current = state.borrow().clone();
            if matches!(current, WorkerState::Stopped | WorkerState::Failed(_)) {
                return current;
            }
            if state.changed().await.is_err() {
                return state.borrow().clone();
            }
        }
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.link.close();
        Ok(())
    }
    pub async fn reload(&self, scripts: Vec<DeliveredPlugin>) -> Result<()> {
        for script in &scripts {
            script.validate()?;
        }
        self.outgoing
            .send(Frame::Control(Control::Reload { scripts }))
            .await
            .map_err(|_| Error::Closed)
    }
}
#[derive(Clone)]
pub struct WorkerOptions {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub idle_timeout: Duration,
    pub start_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub lock_dir: PathBuf,
}
impl WorkerOptions {
    pub fn new(executable: PathBuf, lock_dir: PathBuf) -> Self {
        Self {
            executable,
            args: vec![],
            env: BTreeMap::new(),
            idle_timeout: Duration::from_secs(300),
            start_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(3),
            lock_dir,
        }
    }
}
pub struct Workers {
    options: WorkerOptions,
    handles: tokio::sync::Mutex<BTreeMap<String, Arc<WorkerHandle>>>,
    closed: Mutex<bool>,
}
impl Workers {
    fn lock_path_for_key(&self, key: &str) -> PathBuf {
        self.options
            .lock_dir
            .join(format!("{}.lock", super::plugins::digest(key.as_bytes())))
    }
    pub(crate) fn lock_path(&self, address: &serde_json::Value) -> Result<PathBuf> {
        Ok(self.lock_path_for_key(&crate::json::JsonValue::from_serde(address.clone()).to_json()?))
    }
    /// Stop and forget only the worker owning this address. AppServer serializes
    /// this with demand and holds the process lock during address mutations.
    pub(crate) async fn retire(&self, address: &serde_json::Value) -> Result<()> {
        let key = crate::json::JsonValue::from_serde(address.clone()).to_json()?;
        let handle = self.handles.lock().await.remove(&key);
        if let Some(handle) = handle {
            handle.shutdown().await?;
            handle.wait_exit().await;
        }
        Ok(())
    }
    pub fn new(options: WorkerOptions) -> Result<Self> {
        super::app::private_directory(&options.lock_dir)?;
        Ok(Self {
            options,
            handles: tokio::sync::Mutex::new(BTreeMap::new()),
            closed: Mutex::new(false),
        })
    }
    pub async fn demand(
        &self,
        address: serde_json::Value,
        start: serde_json::Value,
        scripts: Vec<DeliveredPlugin>,
    ) -> Result<Arc<WorkerHandle>> {
        self.demand_with_idle_guard(address, start, scripts, || true)
            .await
    }

    /// Start a worker whose idle shutdown also depends on demand held by its caller.
    pub async fn demand_with_idle_guard(
        &self,
        address: serde_json::Value,
        start: serde_json::Value,
        scripts: Vec<DeliveredPlugin>,
        can_stop_idle: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<WorkerHandle>> {
        let mut handles = self.handles.lock().await;
        if *self.closed.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(Error::Closed);
        }
        let key = crate::json::JsonValue::from_serde(address.clone()).to_json()?;
        if let Some(handle) = handles.get(&key) {
            // An idle stop is restartable. A crash is sticky until explicit retry.
            if !matches!(handle.state(), WorkerState::Stopped) {
                return Ok(handle.clone());
            }
        }
        for script in &scripts {
            script.validate()?;
        }
        let options = self.options.clone();
        let mut command = tokio::process::Command::new(&options.executable);
        command
            .args(&options.args)
            .envs(&options.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| Error::Invalid("worker has no pid".into()))?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Invalid("worker stdin missing".into()))?;
        let mut output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Invalid("worker stdout missing".into()))?;
        let (sender, mut outgoing) = mpsc::channel(64);
        let (state, receiver) = watch::channel(WorkerState::Starting);
        let link = Arc::new(PipeLink {
            end: LinkEnd::default(),
            outgoing: sender.clone(),
        });
        let lock = self.lock_path_for_key(&key);
        sender
            .send(Frame::Control(Control::Start {
                address,
                start,
                scripts,
                lock,
            }))
            .await
            .map_err(|_| Error::Closed)?;
        let handle = Arc::new(WorkerHandle {
            link: link.clone(),
            pid,
            state: receiver,
            outgoing: sender,
        });
        handles.insert(key, handle.clone());
        tokio::spawn(async move {
            let (events_tx, mut events) = mpsc::channel(64);
            let reader = tokio::spawn(async move {
                loop {
                    match read_frame(&mut output).await {
                        Ok(frame) => {
                            if events_tx.send(Ok(frame)).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = events_tx.send(Err(error)).await;
                            break;
                        }
                    }
                }
            });
            let writer = tokio::spawn(async move {
                while let Some(frame) = outgoing.recv().await {
                    write_frame(&mut input, &frame).await?;
                }
                Ok::<_, Error>(())
            });
            let mut deadline = tokio::time::Instant::now() + options.start_timeout;
            let mut timed = true;
            let mut stopping = false;
            let mut failure = None;
            loop {
                tokio::select! {
                    exit=child.wait()=>{if !stopping && !link.closed() {failure=Some(format!("worker exited: {exit:?}"));}break;},
                    event=events.recv()=>{match event {
                        Some(Ok(Frame::Binary(bytes)))=>link.end.deliver(bytes),
                        Some(Ok(Frame::Control(Control::Ready {idle}))) if !stopping=>{state.send_replace(WorkerState::Ready);timed=idle;deadline=tokio::time::Instant::now()+options.idle_timeout;},
                        Some(Ok(Frame::Control(Control::Idle {idle}))) if !stopping && matches!(*state.borrow(),WorkerState::Ready)=>{timed=idle;deadline=tokio::time::Instant::now()+options.idle_timeout;},
                        Some(Ok(Frame::Control(Control::Fatal {message})))=>{failure=Some(message);let _=child.kill().await;break;},
                        Some(Ok(_))=>{},
                        _=>{if !stopping && !link.closed() {failure=Some("worker IPC disconnected".into());}let _=child.kill().await;break;}
                    }},
                    ()=tokio::time::sleep_until(deadline),if timed=>{
                        if stopping {let _=child.kill().await;break;}
                        let starting = matches!(*state.borrow(), WorkerState::Starting);
                        if !starting && !can_stop_idle() {
                            // Ready and Idle describe the child's view, which can lag pending holds.
                            deadline = tokio::time::Instant::now() + Duration::from_millis(20);
                            continue;
                        }
                        stopping = true;
                        deadline = tokio::time::Instant::now() + options.shutdown_timeout;
                        if starting {
                            failure = Some("worker startup timed out".into());
                            let _ = link.outgoing.try_send(Frame::Control(Control::Shutdown));
                        } else {
                            // Demand arriving during teardown belongs to the next worker.
                            link.close();
                        }
                    },
                    ()=async {while !link.closed(){tokio::time::sleep(Duration::from_millis(20)).await;}},if !stopping=>{stopping=true;timed=true;deadline=tokio::time::Instant::now()+options.shutdown_timeout;}
                }
            }
            link.end.ended();
            reader.abort();
            writer.abort();
            let _ = child.wait().await;
            state.send_replace(failure.map_or(WorkerState::Stopped, WorkerState::Failed));
        });
        Ok(handle)
    }
    pub async fn retry(&self, address: &serde_json::Value) -> Result<()> {
        let key = crate::json::JsonValue::from_serde(address.clone()).to_json()?;
        let mut handles = self.handles.lock().await;
        if let Some(handle) = handles.get(&key)
            && !matches!(
                handle.state(),
                WorkerState::Stopped | WorkerState::Failed(_)
            )
        {
            return Err(Error::Invalid("cannot retry a running worker".into()));
        }
        handles.remove(&key);
        Ok(())
    }
    pub async fn close(&self) {
        let handles = {
            let handles = self.handles.lock().await;
            *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
            handles.values().cloned().collect::<Vec<_>>()
        };
        for handle in &handles {
            handle.link.close();
        }
        for handle in handles {
            handle.wait_exit().await;
        }
    }
}
/// Child-side transport. Native applications supply their async stdin/stdout, with no Node runtime.
impl Drop for Workers {
    fn drop(&mut self) {
        for handle in self.handles.get_mut().values() {
            handle.link.close();
        }
    }
}
pub struct WorkerSession {
    pub address: serde_json::Value,
    pub start: serde_json::Value,
    pub scripts: Vec<DeliveredPlugin>,
    pub parent: Arc<dyn Link>,
    pub controls: mpsc::Receiver<Control>,
    sender: mpsc::Sender<Frame>,
    receive_task: tokio::task::JoinHandle<()>,
    send_task: tokio::task::JoinHandle<Result<()>>,
    _lock: ProcessLock,
}
impl WorkerSession {
    pub async fn start<R, W>(mut reader: R, mut writer: W) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let Frame::Control(Control::Start {
            address,
            start,
            scripts,
            lock,
        }) = read_frame(&mut reader).await?
        else {
            return Err(Error::Invalid("worker expected start".into()));
        };
        for script in &scripts {
            script.validate()?;
        }
        let lock = ProcessLock::acquire(&lock, Duration::from_secs(60)).await?;
        let (sender, mut outgoing) = mpsc::channel(64);
        let (control_tx, controls) = mpsc::channel(16);
        let parent = Arc::new(PipeLink {
            end: LinkEnd::default(),
            outgoing: sender.clone(),
        });
        let link = parent.clone();
        let receive_task = tokio::spawn(async move {
            while let Ok(frame) = read_frame(&mut reader).await {
                match frame {
                    Frame::Binary(bytes) => link.end.deliver(bytes),
                    Frame::Control(control) => {
                        if control_tx.send(control).await.is_err() {
                            break;
                        }
                    }
                }
            }
            link.end.ended();
        });
        let link = parent.clone();
        let send_task = tokio::spawn(async move {
            let result = async {
                while let Some(frame) = outgoing.recv().await {
                    write_frame(&mut writer, &frame).await?;
                    if matches!(frame, Frame::Control(Control::Fatal { .. })) {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            link.end.ended();
            result
        });
        sender
            .send(Frame::Control(Control::Started))
            .await
            .map_err(|_| Error::Closed)?;
        Ok(Self {
            address,
            start,
            scripts,
            parent,
            controls,
            sender,
            receive_task,
            send_task,
            _lock: lock,
        })
    }
    pub async fn report(&self, control: Control) -> Result<()> {
        self.sender
            .send(Frame::Control(control))
            .await
            .map_err(|_| Error::Closed)
    }

    pub(crate) async fn report_start_failure(&mut self, message: String) -> Result<()> {
        self.report(Control::Fatal { message }).await?;
        // Dropping the session cancels the writer. Let it flush the terminal frame first.
        (&mut self.send_task).await.map_err(|_| Error::Closed)?
    }
}
impl Drop for WorkerSession {
    fn drop(&mut self) {
        self.parent.close();
        self.receive_task.abort();
        self.send_task.abort();
    }
}
