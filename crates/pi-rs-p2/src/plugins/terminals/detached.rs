//! Detached Unix-socket terminal host (`p2 --terminal-host <data_dir>`) so local PTYs
//! run inside an independent process (via `pi-env` daemon PTY when available, falling back
//! to `LocalTerminal`) and survive `p2` server restarts.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use pi_rs_chord::{Error, Result};
use pi_rs_env::{
    Connection, ConnectionOptions, RemoteTerminal, TerminalForeground, TerminalOptions,
    open_terminal,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex as AsyncMutex, oneshot, watch},
};

const RING: usize = 1024 * 1024;
const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetachedTerminalInfo {
    pub id: String,
    pub session_id: String,
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    pub cwd: String,
    pub title: String,
    pub created_at: u64,
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEntry {
    pub info: DetachedTerminalInfo,
    pub buffered: Vec<u8>,
}

/// Deterministic short Unix socket path in `std::env::temp_dir()` for `data_dir`.
pub fn terminal_host_socket_path(data_dir: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(b":p2-terminals");
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::env::temp_dir().join(format!("p2-{}-t.sock", &hex[..24]))
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::Invalid("terminal frame too large".into()));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(Error::Invalid("invalid terminal frame length".into()));
    }
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body).await?;
    let val: Value = serde_json::from_slice(&body)?;
    Ok(Some(val))
}

enum HostPty {
    Local(pi_rs_env::daemon::LocalTerminal),
    Remote {
        terminal: RemoteTerminal,
        _conn: Arc<Connection>,
    },
}

impl HostPty {
    fn open_with_fallback(
        cwd: String,
        env: Option<Map<String, Value>>,
        cols: u16,
        rows: u16,
        on_data: pi_rs_env::terminal::DataCallback,
    ) -> std::result::Result<Self, String> {
        if let Ok(daemon_path) = pi_rs_env::local_daemon()
            && daemon_path.exists()
        {
            let conn = Arc::new(Connection::new(ConnectionOptions::new(vec![
                daemon_path.to_string_lossy().into_owned(),
            ])));
            let opts = TerminalOptions {
                cwd: cwd.clone(),
                argv: None,
                env: env.clone(),
                inherit_env: None,
                cols,
                rows,
                on_data: on_data.clone(),
            };
            if let Ok(terminal) = open_terminal(conn.clone(), opts) {
                return Ok(Self::Remote {
                    terminal,
                    _conn: conn,
                });
            }
            conn.close();
        }
        let opts = TerminalOptions {
            cwd,
            argv: None,
            env,
            inherit_env: None,
            cols,
            rows,
            on_data,
        };
        pi_rs_env::daemon::LocalTerminal::open(opts)
            .map(Self::Local)
            .map_err(|e| e.to_string())
    }

    fn write(&self, bytes: &[u8]) {
        match self {
            Self::Local(t) => t.write(bytes),
            Self::Remote { terminal, .. } => terminal.write(bytes),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> std::result::Result<(), String> {
        match self {
            Self::Local(t) => t.resize(cols, rows).map_err(|e| e.to_string()),
            Self::Remote { terminal, .. } => {
                terminal.resize(cols, rows);
                Ok(())
            }
        }
    }

    fn foreground(&self) -> Option<TerminalForeground> {
        match self {
            Self::Local(t) => t.foreground().ok().flatten(),
            Self::Remote { terminal, .. } => terminal.foreground().ok().flatten(),
        }
    }

    fn close(&self) {
        match self {
            Self::Local(t) => t.close(),
            Self::Remote { terminal, .. } => terminal.close(),
        }
    }

    fn wait(&self) -> std::result::Result<i32, String> {
        match self {
            Self::Local(t) => t.wait().map_err(|e| e.to_string()),
            Self::Remote { terminal, .. } => terminal.wait().map_err(|e| e.to_string()),
        }
    }
}

struct HostTerminal {
    info: Mutex<DetachedTerminalInfo>,
    ring: Mutex<VecDeque<u8>>,
    pty: Mutex<Option<Arc<HostPty>>>,
}

impl HostTerminal {
    fn append_ring(&self, bytes: &[u8]) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let kept = &bytes[bytes.len().saturating_sub(RING)..];
        let evict = (ring.len() + kept.len()).saturating_sub(RING);
        ring.drain(..evict);
        ring.extend(kept);
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.iter().copied().collect()
    }
}

struct DaemonState {
    terminals: Mutex<HashMap<String, Arc<HostTerminal>>>,
    events: tokio::sync::broadcast::Sender<Value>,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            terminals: Mutex::default(),
            events: tokio::sync::broadcast::channel(512).0,
        }
    }

    fn emit(&self, event: Value) {
        let _ = self.events.send(event);
    }

    fn remove(&self, id: &str) {
        let removed = self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(term) = removed
            && let Some(pty) = term.pty.lock().unwrap_or_else(|e| e.into_inner()).take()
        {
            pty.close();
        }
    }

    fn close_session(&self, session: &str) {
        let ids: Vec<String> = {
            let guard = self.terminals.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .iter()
                .filter(|(_, t)| {
                    t.info.lock().unwrap_or_else(|e| e.into_inner()).session_id == session
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.remove(&id);
        }
    }

    fn close_all(&self) {
        let items: Vec<Arc<HostTerminal>> = self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, t)| t)
            .collect();
        for term in items {
            if let Some(pty) = term.pty.lock().unwrap_or_else(|e| e.into_inner()).take() {
                pty.close();
            }
        }
    }
}

/// Runs the detached terminal host daemon for `data_dir` until shutdown or idle exit.
pub async fn run_terminal_host(data_dir: &Path) -> Result<()> {
    let sock_path = terminal_host_socket_path(data_dir);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if sock_path.exists() {
        if UnixStream::connect(&sock_path).await.is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)?;
    let (stop_tx, stop_rx) = watch::channel(false);
    serve_listener(listener, Some(sock_path), stop_rx, Some(stop_tx)).await
}

/// Serves detached terminal host connections on an already-bound `UnixListener` (useful in tests too).
pub async fn serve_listener(
    listener: UnixListener,
    cleanup_path: Option<PathBuf>,
    mut stop_rx: watch::Receiver<bool>,
    auto_idle_stop: Option<watch::Sender<bool>>,
) -> Result<()> {
    let state = Arc::new(DaemonState::new());
    let active_clients = Arc::new(AtomicU64::new(0));
    let mut idle_tick = tokio::time::interval(Duration::from_secs(2));
    idle_tick.tick().await;
    let mut empty_ticks = 0u32;

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            _ = idle_tick.tick(), if auto_idle_stop.is_some() => {
                let clients = active_clients.load(Ordering::Acquire);
                let terms = state.terminals.lock().unwrap_or_else(|e| e.into_inner()).len();
                if clients == 0 && terms == 0 {
                    empty_ticks += 1;
                    if empty_ticks >= 3 {
                        break;
                    }
                } else {
                    empty_ticks = 0;
                }
            }
            accept = listener.accept() => {
                let Ok((stream, _)) = accept else {
                    break;
                };
                empty_ticks = 0;
                active_clients.fetch_add(1, Ordering::AcqRel);
                let state = state.clone();
                let active_clients = active_clients.clone();
                tokio::spawn(async move {
                    handle_host_client(stream, state).await;
                    active_clients.fetch_sub(1, Ordering::AcqRel);
                });
            }
        }
    }

    state.close_all();
    if let Some(path) = cleanup_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

async fn handle_host_client(stream: UnixStream, state: Arc<DaemonState>) {
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(AsyncMutex::new(writer));
    let mut events = state.events.subscribe();

    let writer_ev = writer.clone();
    let ev_task = tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            let mut guard = writer_ev.lock().await;
            if write_frame(&mut *guard, &ev).await.is_err() {
                break;
            }
        }
    });

    while let Ok(Some(msg)) = read_frame(&mut reader).await {
        let op = msg["op"].as_str().unwrap_or_default().to_owned();
        let req_id = msg.get("id").and_then(Value::as_u64);
        match op.as_str() {
            "list" => {
                let entries: Vec<SnapshotEntry> = {
                    let guard = state.terminals.lock().unwrap_or_else(|e| e.into_inner());
                    guard
                        .values()
                        .map(|t| SnapshotEntry {
                            info: t.info.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                            buffered: t.snapshot_bytes(),
                        })
                        .collect()
                };
                if let Some(id) = req_id {
                    let mut guard = writer.lock().await;
                    let _ = write_frame(
                        &mut *guard,
                        &json!({ "type": "reply", "id": id, "ok": true, "data": entries }),
                    )
                    .await;
                }
            }
            "open" => {
                let reply = handle_host_open(&state, &msg).await;
                if let Some(id) = req_id {
                    let frame = match reply {
                        Ok(info) => json!({ "type": "reply", "id": id, "ok": true, "data": info }),
                        Err(err) => json!({ "type": "reply", "id": id, "ok": false, "error": err }),
                    };
                    let mut guard = writer.lock().await;
                    let _ = write_frame(&mut *guard, &frame).await;
                }
            }
            "write" => {
                let term_id = msg["terminalId"].as_str().unwrap_or_default();
                let bytes: Vec<u8> =
                    serde_json::from_value(msg["bytes"].clone()).unwrap_or_default();
                let pty = state
                    .terminals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(term_id)
                    .and_then(|t| t.pty.lock().unwrap_or_else(|e| e.into_inner()).clone());
                if let Some(pty) = pty {
                    tokio::task::spawn_blocking(move || pty.write(&bytes));
                }
            }
            "resize" => {
                let term_id = msg["terminalId"].as_str().unwrap_or_default();
                let cols = msg["cols"].as_u64().unwrap_or(80) as u16;
                let rows = msg["rows"].as_u64().unwrap_or(24) as u16;
                let term = state
                    .terminals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(term_id)
                    .cloned();
                let res = if let Some(term) = term {
                    let pty = term.pty.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    {
                        let mut info = term.info.lock().unwrap_or_else(|e| e.into_inner());
                        info.cols = cols;
                        info.rows = rows;
                    }
                    if let Some(pty) = pty {
                        tokio::task::spawn_blocking(move || pty.resize(cols, rows))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    } else {
                        Ok(())
                    }
                } else {
                    Err("unknown terminal".into())
                };
                if let Some(id) = req_id {
                    let frame = match res {
                        Ok(()) => json!({ "type": "reply", "id": id, "ok": true, "data": null }),
                        Err(err) => json!({ "type": "reply", "id": id, "ok": false, "error": err }),
                    };
                    let mut guard = writer.lock().await;
                    let _ = write_frame(&mut *guard, &frame).await;
                }
            }
            "foreground" => {
                let term_id = msg["terminalId"].as_str().unwrap_or_default();
                let pty = state
                    .terminals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(term_id)
                    .and_then(|t| t.pty.lock().unwrap_or_else(|e| e.into_inner()).clone());
                let fg = if let Some(pty) = pty {
                    tokio::task::spawn_blocking(move || {
                        pty.foreground()
                            .map(|f| json!({ "pid": f.pid, "name": f.name }))
                    })
                    .await
                    .unwrap_or(None)
                } else {
                    None
                };
                if let Some(id) = req_id {
                    let mut guard = writer.lock().await;
                    let _ = write_frame(
                        &mut *guard,
                        &json!({ "type": "reply", "id": id, "ok": true, "data": fg }),
                    )
                    .await;
                }
            }
            "close" => {
                let term_id = msg["terminalId"].as_str().unwrap_or_default();
                state.remove(term_id);
            }
            "closeSession" => {
                let session = msg["session"].as_str().unwrap_or_default();
                state.close_session(session);
            }
            "closeAll" => {
                state.close_all();
            }
            _ => {}
        }
    }

    ev_task.abort();
}

async fn handle_host_open(
    state: &Arc<DaemonState>,
    msg: &Value,
) -> std::result::Result<DetachedTerminalInfo, String> {
    let info: DetachedTerminalInfo =
        serde_json::from_value(msg["info"].clone()).map_err(|e| e.to_string())?;
    let env: Option<Map<String, Value>> =
        serde_json::from_value(msg["env"].clone()).unwrap_or(None);
    let term_id = info.id.clone();
    let cwd = info.cwd.clone();
    let cols = info.cols;
    let rows = info.rows;

    let term = Arc::new(HostTerminal {
        info: Mutex::new(info.clone()),
        ring: Mutex::default(),
        pty: Mutex::new(None),
    });

    let term_out = Arc::downgrade(&term);
    let state_out = Arc::downgrade(state);
    let tid_out = term_id.clone();
    let on_data: pi_rs_env::terminal::DataCallback = Arc::new(move |bytes| {
        if let Some(term) = term_out.upgrade() {
            term.append_ring(bytes);
        }
        if let Some(st) = state_out.upgrade() {
            st.emit(json!({
                "type": "output",
                "terminalId": tid_out,
                "bytes": bytes,
            }));
        }
    });

    let pty = tokio::task::spawn_blocking(move || {
        HostPty::open_with_fallback(cwd, env, cols, rows, on_data)
    })
    .await
    .map_err(|e| e.to_string())??;
    let pty = Arc::new(pty);
    *term.pty.lock().unwrap_or_else(|e| e.into_inner()) = Some(pty.clone());
    state
        .terminals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(term_id.clone(), term.clone());

    let term_wait = Arc::downgrade(&term);
    let state_wait = Arc::downgrade(state);
    std::thread::spawn(move || {
        let code = pty.wait();
        let exited = match code {
            Ok(c) => json!({ "code": c }),
            Err(err) => json!({ "error": err }),
        };
        if let Some(term) = term_wait.upgrade() {
            term.pty.lock().unwrap_or_else(|e| e.into_inner()).take();
            term.info.lock().unwrap_or_else(|e| e.into_inner()).exited = Some(exited.clone());
        }
        if let Some(st) = state_wait.upgrade() {
            st.emit(json!({
                "type": "exited",
                "terminalId": term_id,
                "exited": exited,
            }));
        }
    });

    Ok(info)
}

pub struct DetachedClient {
    writer: AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    subscribers: Mutex<HashMap<String, Subscriber>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Callback invoked for detached PTY byte chunks.
pub type DetachedDataHandler = Arc<dyn Fn(&[u8]) + Send + Sync>;
/// Callback invoked when a detached PTY process exits.
pub type DetachedExitHandler = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Clone)]
struct Subscriber {
    on_data: DetachedDataHandler,
    on_exit: DetachedExitHandler,
}

impl DetachedClient {
    /// Connects to an already-running detached terminal host without spawning one.
    pub async fn connect_existing(sock_path: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(sock_path).await?;
        Ok(Self::from_stream(stream))
    }

    /// Connects to the detached terminal host for `data_dir`, spawning `p2 --terminal-host <data_dir>` if needed.
    pub async fn connect_or_spawn(data_dir: &Path) -> Result<Arc<Self>> {
        let sock_path = terminal_host_socket_path(data_dir);
        if let Ok(client) = Self::connect_existing(&sock_path).await {
            return Ok(client);
        }
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--terminal-host")
            .arg(data_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.spawn()?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match Self::connect_existing(&sock_path).await {
                Ok(client) => return Ok(client),
                Err(err) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::Invalid(format!(
                            "Detached terminal host did not start: {err}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn from_stream(stream: UnixStream) -> Arc<Self> {
        let (mut reader, writer) = stream.into_split();
        let client = Arc::new(Self {
            writer: AsyncMutex::new(writer),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
            subscribers: Mutex::default(),
            reader_task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&client);
        let task = tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut reader).await {
                let Some(client) = weak.upgrade() else {
                    break;
                };
                match frame["type"].as_str() {
                    Some("reply") => {
                        if let Some(id) = frame["id"].as_u64()
                            && let Some(tx) = client
                                .pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&id)
                        {
                            if frame["ok"] == true {
                                let _ = tx.send(Ok(frame["data"].clone()));
                            } else {
                                let _ = tx.send(Err(frame["error"]
                                    .as_str()
                                    .unwrap_or("terminal host error")
                                    .to_owned()));
                            }
                        }
                    }
                    Some("output") => {
                        if let Some(tid) = frame["terminalId"].as_str()
                            && let Ok(bytes) =
                                serde_json::from_value::<Vec<u8>>(frame["bytes"].clone())
                        {
                            let sub = client
                                .subscribers
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(tid)
                                .cloned();
                            if let Some(sub) = sub {
                                (sub.on_data)(&bytes);
                            }
                        }
                    }
                    Some("exited") => {
                        if let Some(tid) = frame["terminalId"].as_str() {
                            let sub = client
                                .subscribers
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(tid)
                                .cloned();
                            if let Some(sub) = sub {
                                (sub.on_exit)(frame["exited"].clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        *client.reader_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        client
    }

    pub fn register_subscriber(
        &self,
        terminal_id: &str,
        on_data: DetachedDataHandler,
        on_exit: DetachedExitHandler,
    ) {
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(terminal_id.to_owned(), Subscriber { on_data, on_exit });
    }

    async fn request(&self, mut payload: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        payload["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let send_res = {
            let mut writer = self.writer.lock().await;
            write_frame(&mut *writer, &payload).await
        };
        if let Err(err) = send_res {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(err);
        }
        match rx.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(msg)) => Err(Error::Invalid(msg)),
            Err(_) => Err(Error::Closed),
        }
    }

    async fn send_fire_and_forget(&self, payload: Value) -> Result<()> {
        let mut writer = self.writer.lock().await;
        write_frame(&mut *writer, &payload).await
    }

    pub async fn list(&self) -> Result<Vec<SnapshotEntry>> {
        let val = self.request(json!({ "op": "list" })).await?;
        Ok(serde_json::from_value(val)?)
    }

    pub async fn open(
        self: &Arc<Self>,
        info: DetachedTerminalInfo,
        env: Option<Map<String, Value>>,
        on_data: DetachedDataHandler,
        on_exit: DetachedExitHandler,
    ) -> Result<DetachedProcess> {
        self.register_subscriber(&info.id, on_data, on_exit);
        let res = self
            .request(json!({
                "op": "open",
                "info": info,
                "env": env,
            }))
            .await;
        match res {
            Ok(val) => {
                let opened: DetachedTerminalInfo = serde_json::from_value(val)?;
                Ok(DetachedProcess {
                    id: opened.id,
                    client: self.clone(),
                })
            }
            Err(err) => {
                self.subscribers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&info.id);
                Err(err)
            }
        }
    }

    pub async fn close_session(&self, session: &str) -> Result<()> {
        self.send_fire_and_forget(json!({ "op": "closeSession", "session": session }))
            .await
    }

    pub async fn close_all(&self) -> Result<()> {
        self.send_fire_and_forget(json!({ "op": "closeAll" })).await
    }
}

impl Drop for DetachedClient {
    fn drop(&mut self) {
        if let Some(task) = self
            .reader_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct DetachedProcess {
    pub id: String,
    pub client: Arc<DetachedClient>,
}

impl DetachedProcess {
    pub async fn write(&self, bytes: Vec<u8>) {
        let _ = self
            .client
            .send_fire_and_forget(json!({
                "op": "write",
                "terminalId": self.id,
                "bytes": bytes,
            }))
            .await;
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let _ = self
            .client
            .request(json!({
                "op": "resize",
                "terminalId": self.id,
                "cols": cols,
                "rows": rows,
            }))
            .await?;
        Ok(())
    }

    pub async fn foreground(&self) -> Option<TerminalForeground> {
        let val = self
            .client
            .request(json!({
                "op": "foreground",
                "terminalId": self.id,
            }))
            .await
            .ok()?;
        if val.is_null() {
            return None;
        }
        Some(TerminalForeground {
            pid: val["pid"].as_u64().unwrap_or(0) as u32,
            name: val["name"].as_str().map(str::to_owned),
        })
    }

    pub async fn close(&self) {
        self.client
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        let _ = self
            .client
            .send_fire_and_forget(json!({
                "op": "close",
                "terminalId": self.id,
            }))
            .await;
    }
}
