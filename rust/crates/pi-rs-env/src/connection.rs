//! Client transport for the `pi-env` wire protocol (port of `packages/env/src/connection.ts`).
//!
//! A `Connection` starts a daemon lazily on the first request, speaks the length-prefixed frames over its stdin and
//! stdout, and starts a fresh daemon after a lost connection. Requests in flight when a connection dies fail with a
//! connection-lost error; handles belong to the daemon that opened them, so callers can pin a request to a session.

use crate::framing::{self, CANCEL, ERROR, EVENT, PING, REQUEST, RESULT};
use rand::RngCore;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PING_INTERVAL: Duration = Duration::from_secs(5);
const SILENCE_LIMIT_MS: u64 = 30_000;
const START_TIMEOUT: Duration = Duration::from_secs(60);
const ABORT_GRACE: Duration = Duration::from_millis(1000);
const STDERR_TAIL_CHARS: usize = 2000;
const STDERR_TAIL_LINES: usize = 20;
const WAIT_SLICE: Duration = Duration::from_millis(20);

type RequestResult = Result<Reply, RemoteError>;

/// Lock a mutex, recovering the guarded value after a panic in a previous holder. The connection's locks hold only
/// plain data and channels, so a poisoned guard is safe to reuse and one failed callback cannot strand every request.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A failure reported by the daemon, carrying its Node-style code and any extra fields of the error frame.
#[derive(Debug, Clone)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    pub fields: Map<String, Value>,
}

impl RemoteError {
    pub fn new(fields: &Value) -> RemoteError {
        let fields = fields.as_object().cloned().unwrap_or_default();
        let message = fields
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Remote operation failed")
            .to_string();
        let code = fields
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let path = fields
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        RemoteError {
            code,
            message,
            path,
            fields,
        }
    }

    /// A connection failure: a request may succeed once the daemon is started again.
    pub fn lost(message: impl Into<String>) -> RemoteError {
        RemoteError::new(&json!({ "code": "unknown", "message": message.into(), "lost": true }))
    }

    pub fn spawn_error(message: impl Into<String>) -> RemoteError {
        RemoteError::new(&json!({ "code": "spawn_error", "message": message.into(), "lost": true }))
    }

    pub fn aborted() -> RemoteError {
        RemoteError::new(&json!({ "code": "aborted", "message": "aborted" }))
    }

    fn closed() -> RemoteError {
        RemoteError::new(&json!({ "code": "unknown", "message": "Connection closed" }))
    }

    /// Attach the captured stderr of a failed start to its message.
    fn with_stderr(mut self, stderr: &str) -> RemoteError {
        let lines: Vec<String> = sanitize_stderr(stderr)
            .split('\n')
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        if !lines.is_empty() {
            let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
            self.message = format!(
                "{}\nremote stderr:\n{}",
                self.message,
                lines[start..].join("\n")
            );
        }
        self
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Whether an error means the connection was lost, not that the daemon refused the request.
pub fn is_connection_lost(error: &RemoteError) -> bool {
    error.fields.get("lost").and_then(Value::as_bool) == Some(true)
}

/// Remove terminal escapes and control characters from remote stderr, keeping printable text, tabs and newlines.
fn sanitize_stderr(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let characters: Vec<char> = normalized.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        index += 1;
        if character == '\u{1b}' {
            if index < characters.len() && characters[index] == '[' {
                index += 1;
                while index < characters.len() {
                    let final_byte = characters[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&(final_byte as u32)) {
                        break;
                    }
                }
            } else if index < characters.len() {
                index += 1;
            }
            continue;
        }
        let code = character as u32;
        if character == '\n'
            || character == '\t'
            || (code >= 0x20 && !(0x7f..=0x9f).contains(&code))
        {
            out.push(character);
        }
    }
    out
}

fn append_stderr(tail: &mut String, text: &str) {
    tail.push_str(text);
    if tail.chars().count() > STDERR_TAIL_CHARS {
        let kept: String = tail.chars().rev().take(STDERR_TAIL_CHARS).collect();
        *tail = kept.chars().rev().collect();
    }
}

/// What `hello` reports about the remote machine.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub protocol: u64,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub home: String,
    pub tmpdir: String,
    pub separator: String,
    pub cwd: String,
    pub drive_cwds: Map<String, Value>,
    pub pid: u64,
    pub features: Vec<String>,
}

impl RemoteInfo {
    pub fn from_json(value: &Value) -> Result<RemoteInfo, RemoteError> {
        let field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| RemoteError::new(&json!({ "code": "unknown", "message": format!("invalid hello field {key}") })))
        };
        Ok(RemoteInfo {
            protocol: value.get("protocol").and_then(Value::as_u64).unwrap_or(0),
            version: field("version")?,
            os: field("os")?,
            arch: field("arch")?,
            home: field("home")?,
            tmpdir: field("tmpdir")?,
            separator: field("separator")?,
            cwd: field("cwd")?,
            drive_cwds: value
                .get("driveCwds")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            pid: value.get("pid").and_then(Value::as_u64).unwrap_or(0),
            features: value
                .get("features")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    pub fn has_feature(&self, name: &str) -> bool {
        self.features.iter().any(|feature| feature == name)
    }
}

/// Node's names for the daemon's system and architecture, which `hello` reports in Rust's spelling.
pub fn node_platform(info: &RemoteInfo) -> (String, String) {
    let platform = match info.os.as_str() {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match info.arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform.to_string(), arch.to_string())
}

pub type LogCallback = Arc<dyn Fn(&str) + Send + Sync>;
pub type EventCallback = Arc<dyn Fn(&Value, &[u8]) + Send + Sync>;
pub type StartCallback = Arc<dyn Fn(u32, u64) + Send + Sync>;

/// Command that starts the daemon, before its `serve --token <hex>` arguments. A function is evaluated at each start.
pub enum CommandSource {
    Static(Arc<Vec<String>>),
    Dynamic(Arc<dyn Fn() -> Result<Vec<String>, RemoteError> + Send + Sync>),
}

impl CommandSource {
    fn resolve(&self) -> Result<Vec<String>, RemoteError> {
        match self {
            CommandSource::Static(command) => Ok((**command).clone()),
            CommandSource::Dynamic(function) => function(),
        }
    }
}

impl From<Vec<String>> for CommandSource {
    fn from(command: Vec<String>) -> CommandSource {
        CommandSource::Static(Arc::new(command))
    }
}

pub struct ConnectionOptions {
    pub command: CommandSource,
    pub on_log: Option<LogCallback>,
}

impl ConnectionOptions {
    pub fn new(command: impl Into<CommandSource>) -> ConnectionOptions {
        ConnectionOptions {
            command: command.into(),
            on_log: None,
        }
    }
}

/// One successful reply; `session` identifies the daemon that answered, for handle-affine follow-up requests.
#[derive(Debug, Clone)]
pub struct Reply {
    pub json: Value,
    pub payload: Vec<u8>,
    pub session: u64,
}

/// A minimal abort signal: checking a flag from the requesting thread is enough to send a cancel and start the grace.
#[derive(Clone)]
pub struct AbortSignal {
    aborted: Arc<AtomicBool>,
}

impl AbortSignal {
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> AbortController {
        AbortController {
            signal: AbortSignal {
                aborted: Arc::new(AtomicBool::new(false)),
            },
        }
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    pub fn abort(&self) {
        self.signal.aborted.store(true, Ordering::SeqCst);
    }
}

impl Default for AbortController {
    fn default() -> AbortController {
        AbortController::new()
    }
}

#[derive(Default)]
pub struct RequestOptions {
    pub payload: Vec<u8>,
    pub signal: Option<AbortSignal>,
    pub on_event: Option<EventCallback>,
    pub on_start: Option<StartCallback>,
    /// Only run in this daemon session, for requests on handles it opened.
    pub session: Option<u64>,
}

struct Pending {
    result: Sender<RequestResult>,
    on_event: Option<EventCallback>,
}

struct SessionShared {
    id: u64,
    token: String,
    child: Mutex<Option<Child>>,
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u32, Pending>>,
    live: AtomicBool,
    last_seen: AtomicU64,
    synced: AtomicBool,
    hello_done: AtomicBool,
    stderr_tail: Mutex<String>,
    next_id: AtomicU32,
}

/// Mark a session dead, stop its daemon and fail everything waiting on it.
fn teardown_session(session: &SessionShared, error: &RemoteError) {
    if !session.live.swap(false, Ordering::SeqCst) {
        return;
    }
    if let Some(mut child) = lock(&session.child).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let waiting: Vec<Pending> = lock(&session.pending)
        .drain()
        .map(|(_, pending)| pending)
        .collect();
    for pending in waiting {
        let _ = pending.result.send(Err(error.clone()));
    }
}

fn write_frame(
    session: &SessionShared,
    kind: u8,
    id: u32,
    json: &Value,
    payload: &[u8],
) -> Result<(), RemoteError> {
    if !session.live.load(Ordering::SeqCst) {
        return Err(RemoteError::lost("pi-env connection lost"));
    }
    let bytes = framing::encode(kind, id, json, payload)
        .map_err(|error| RemoteError::lost(error.to_string()))?;
    let written = lock(&session.stdin).write_all(&bytes);
    if let Err(error) = written {
        let lost = RemoteError::lost("pi-env connection lost");
        teardown_session(session, &lost);
        return Err(RemoteError::new(&json!({
            "code": "unknown",
            "message": format!("pi-env connection lost: {error}"),
            "lost": true,
        })));
    }
    Ok(())
}

fn dispatch_frame(session: &SessionShared, frame: framing::Frame) {
    match frame.kind {
        EVENT => {
            // Copy the callback out before invoking it: an event handler may issue another request, and the mutex is
            // not reentrant.
            let callback = lock(&session.pending)
                .get(&frame.id)
                .and_then(|pending| pending.on_event.clone());
            if let Some(callback) = callback {
                callback(&frame.json, &frame.payload);
            }
        }
        RESULT | ERROR => {
            let pending = lock(&session.pending).remove(&frame.id);
            if let Some(pending) = pending {
                let reply = if frame.kind == RESULT {
                    Ok(Reply {
                        json: frame.json,
                        payload: frame.payload,
                        session: session.id,
                    })
                } else {
                    Err(RemoteError::new(&frame.json))
                };
                let _ = pending.result.send(reply);
            }
        }
        _ => {}
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one daemon's stdout: skip its sync line, then parse and dispatch frames.
fn reader_loop(session: Arc<SessionShared>, mut stdout: ChildStdout, on_log: Option<LogCallback>) {
    let marker = format!("PI-ENV {}\n", session.token).into_bytes();
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if !session.live.load(Ordering::SeqCst) {
            return;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                session.last_seen.store(now_ms(), Ordering::Relaxed);
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(_) => break,
        }
        if !session.synced.load(Ordering::SeqCst) {
            match find_bytes(&buffer, &marker) {
                Some(index) => {
                    let noise = String::from_utf8_lossy(&buffer[..index]);
                    if !noise.trim().is_empty()
                        && let Some(callback) = &on_log
                    {
                        callback(&noise);
                    }
                    buffer.drain(..index + marker.len());
                    session.synced.store(true, Ordering::SeqCst);
                }
                None => {
                    if buffer.len() > 1024 * 1024 {
                        let keep = buffer.len() - marker.len();
                        buffer.drain(..keep);
                    }
                    continue;
                }
            }
        }
        loop {
            match framing::decode(&buffer) {
                Ok(Some((frame, used))) => {
                    buffer.drain(..used);
                    dispatch_frame(&session, frame);
                }
                Ok(None) => break,
                Err(error) => {
                    let lost = RemoteError::lost(format!("Corrupt frame from pi-env: {error}"));
                    teardown_session(&session, &lost);
                    return;
                }
            }
        }
    }
    teardown_session(&session, &RemoteError::lost("pi-env connection lost"));
}

/// Capture stderr for start diagnostics and forward it to the log unchanged.
fn stderr_loop(session: Arc<SessionShared>, mut stderr: ChildStderr, on_log: Option<LogCallback>) {
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                let text = String::from_utf8_lossy(&chunk[..read]);
                if let Some(callback) = &on_log {
                    callback(&text);
                }
                if !session.hello_done.load(Ordering::SeqCst) {
                    append_stderr(&mut lock(&session.stderr_tail), &text);
                }
            }
            Err(_) => return,
        }
    }
}

/// Send pings, notice a daemon that exited, and drop a connection whose peer went silent.
fn supervisor_loop(session: Arc<SessionShared>) {
    let mut last_ping = Instant::now();
    loop {
        if !session.live.load(Ordering::SeqCst) {
            return;
        }
        let exited = lock(&session.child)
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .map(|status| status.is_some())
                    .unwrap_or(true)
            })
            .unwrap_or(true);
        if exited {
            teardown_session(&session, &RemoteError::lost("pi-env connection lost"));
            return;
        }
        if last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            let _ = write_frame(&session, PING, 0, &json!({}), &[]);
            if now_ms().saturating_sub(session.last_seen.load(Ordering::Relaxed)) > SILENCE_LIMIT_MS
            {
                teardown_session(&session, &RemoteError::lost("pi-env connection timed out"));
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send one request and wait for its result, handling cancellation and the abort grace period.
fn send_request_inner(
    session: &Arc<SessionShared>,
    op: &str,
    json_value: Value,
    options: RequestOptions,
    deadline: Option<Instant>,
) -> Result<Reply, RemoteError> {
    if !session.live.load(Ordering::SeqCst) {
        return Err(RemoteError::lost("pi-env connection lost"));
    }
    let id = session.next_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver): (Sender<RequestResult>, Receiver<RequestResult>) = mpsc::channel();
    lock(&session.pending).insert(
        id,
        Pending {
            result: sender,
            on_event: options.on_event,
        },
    );
    if let Some(callback) = &options.on_start {
        callback(id, session.id);
    }
    let mut request = json_value.as_object().cloned().unwrap_or_default();
    request.insert("op".to_string(), json!(op));
    if let Err(error) = write_frame(
        session,
        REQUEST,
        id,
        &Value::Object(request),
        &options.payload,
    ) {
        lock(&session.pending).remove(&id);
        return Err(error);
    }

    let mut cancel_sent = false;
    let mut grace: Option<Instant> = None;
    loop {
        if let Some(signal) = &options.signal
            && signal.is_aborted()
            && !cancel_sent
        {
            let _ = write_frame(session, CANCEL, id, &json!({}), &[]);
            cancel_sent = true;
            grace = Some(Instant::now() + ABORT_GRACE);
        }
        let wait = match grace {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .min(WAIT_SLICE),
            None => WAIT_SLICE,
        };
        match receiver.recv_timeout(wait) {
            Ok(reply) => return reply,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RemoteError::lost("pi-env connection lost"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if grace.is_some_and(|grace| now >= grace) {
            lock(&session.pending).remove(&id);
            return Err(RemoteError::aborted());
        }
        if deadline.is_some_and(|deadline| now >= deadline) {
            lock(&session.pending).remove(&id);
            return Err(RemoteError::lost(format!(
                "pi-env did not answer within {} s",
                START_TIMEOUT.as_secs()
            )));
        }
    }
}

fn send_request(
    session: &Arc<SessionShared>,
    op: &str,
    json_value: Value,
    options: RequestOptions,
) -> Result<Reply, RemoteError> {
    send_request_inner(session, op, json_value, options, None)
}

/// Run a request on its own thread, returning a channel for its result. Used by the long-lived process, terminal and
/// watcher handles, whose result arrives much later than `open` itself.
pub(crate) fn request_async(
    connection: Arc<Connection>,
    op: &'static str,
    json_value: Value,
    options: RequestOptions,
) -> Receiver<RequestResult> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = connection.request(op, json_value, options);
        let _ = sender.send(result);
    });
    receiver
}

fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Start one daemon process and complete its `hello` handshake.
fn start_session(
    command: Vec<String>,
    on_log: &Option<LogCallback>,
    id: u64,
) -> Result<(Arc<SessionShared>, RemoteInfo), RemoteError> {
    let program = command
        .first()
        .cloned()
        .ok_or_else(|| RemoteError::spawn_error("No daemon command"))?;
    let token = random_hex();
    let mut spawn = Command::new(program);
    spawn
        .args(&command[1..])
        .arg("serve")
        .arg("--token")
        .arg(&token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn
        .spawn()
        .map_err(|error| RemoteError::spawn_error(error.to_string()))?;
    // INFALLIBLE: all three streams were configured as pipes above.
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let session = Arc::new(SessionShared {
        id,
        token,
        child: Mutex::new(Some(child)),
        stdin: Mutex::new(stdin),
        pending: Mutex::new(HashMap::new()),
        live: AtomicBool::new(true),
        last_seen: AtomicU64::new(now_ms()),
        synced: AtomicBool::new(false),
        hello_done: AtomicBool::new(false),
        stderr_tail: Mutex::new(String::new()),
        next_id: AtomicU32::new(1),
    });
    let reader = Arc::clone(&session);
    let reader_log = on_log.clone();
    std::thread::spawn(move || reader_loop(reader, stdout, reader_log));
    let capture = Arc::clone(&session);
    let capture_log = on_log.clone();
    std::thread::spawn(move || stderr_loop(capture, stderr, capture_log));
    let supervisor = Arc::clone(&session);
    std::thread::spawn(move || supervisor_loop(supervisor));

    let result = send_request_inner(
        &session,
        "hello",
        json!({ "protocol": 1 }),
        RequestOptions::default(),
        Some(Instant::now() + START_TIMEOUT),
    );
    match result {
        Ok(reply) => {
            let protocol = reply
                .json
                .get("protocol")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if protocol != 1 {
                let error = RemoteError::lost(format!("Unsupported protocol {protocol}"));
                teardown_session(&session, &error);
                return Err(error);
            }
            let info = RemoteInfo::from_json(&reply.json)?;
            session.hello_done.store(true, Ordering::SeqCst);
            lock(&session.stderr_tail).clear();
            Ok((session, info))
        }
        Err(error) => {
            let stderr = lock(&session.stderr_tail).clone();
            let error = error.with_stderr(&stderr);
            teardown_session(&session, &error);
            Err(error)
        }
    }
}

struct ConnectionInner {
    command: CommandSource,
    on_log: Option<LogCallback>,
    session: Option<Arc<SessionShared>>,
    info: Option<RemoteInfo>,
    closed: bool,
    next_session: u64,
}

/// One connection to a `pi-env` daemon, started lazily and restarted after a lost connection.
pub struct Connection {
    inner: Mutex<ConnectionInner>,
}

impl Connection {
    pub fn new(options: ConnectionOptions) -> Connection {
        Connection {
            inner: Mutex::new(ConnectionInner {
                command: options.command,
                on_log: options.on_log,
                session: None,
                info: None,
                closed: false,
                next_session: 0,
            }),
        }
    }

    /// Connect if needed and return what the daemon reported about its machine.
    pub fn info(&self) -> Result<RemoteInfo, RemoteError> {
        self.connect().map(|(_, info)| info)
    }

    /// Connect if needed and return the live session's id.
    pub fn session(&self) -> Result<u64, RemoteError> {
        self.connect().map(|(session, _)| session.id)
    }

    fn connect(&self) -> Result<(Arc<SessionShared>, RemoteInfo), RemoteError> {
        let mut inner = lock(&self.inner);
        if inner.closed {
            return Err(RemoteError::closed());
        }
        if let Some(session) = &inner.session
            && session.live.load(Ordering::SeqCst)
            && let Some(info) = &inner.info
        {
            return Ok((Arc::clone(session), info.clone()));
        }
        let command = inner.command.resolve()?;
        let id = inner.next_session + 1;
        let (session, info) = start_session(command, &inner.on_log, id)?;
        inner.next_session = id;
        inner.session = Some(Arc::clone(&session));
        inner.info = Some(info.clone());
        Ok((session, info))
    }

    /// Send a request. Aborting while the connection starts rejects at once; after it was sent, the daemon answers the
    /// cancel, or the request settles as `aborted` after a one-second grace period.
    pub fn request(
        &self,
        op: &str,
        json_value: Value,
        options: RequestOptions,
    ) -> Result<Reply, RemoteError> {
        if let Some(signal) = &options.signal
            && signal.is_aborted()
        {
            return Err(RemoteError::aborted());
        }
        if let Some(session_id) = options.session {
            let inner = lock(&self.inner);
            let session = inner.session.as_ref().filter(|session| {
                session.id == session_id && session.live.load(Ordering::SeqCst) && !inner.closed
            });
            let Some(session) = session else {
                return Err(RemoteError::lost("pi-env connection lost"));
            };
            return send_request(session, op, json_value, options);
        }
        let (session, _) = self.connect()?;
        send_request(&session, op, json_value, options)
    }

    /// Stop the daemon; it kills everything it started.
    pub fn close(&self) {
        let mut inner = lock(&self.inner);
        inner.closed = true;
        if let Some(session) = inner.session.take() {
            teardown_session(&session, &RemoteError::closed());
        }
        inner.info = None;
    }

    /// Kill one `exec` without aborting it, so it settles with the killed process's status.
    pub fn kill(&self, id: u32, session_id: u64) {
        let inner = lock(&self.inner);
        if let Some(session) = &inner.session
            && session.id == session_id
            && session.live.load(Ordering::SeqCst)
        {
            let _ = write_frame(session, CANCEL, id, &json!({ "mode": "kill" }), &[]);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_stderr_keeps_the_last_twenty_lines_and_the_last_two_thousand_chars() {
        let text = format!(
            "\u{1b}[31mred\u{1b}[0m\r\nbell\u{7} del\u{7f} tab\tc1:\u{85}\u{9d} end\n{}",
            (0..30)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            sanitize_stderr(&text)
                .split('\n')
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>(),
            ["red", "bell del tab\tc1: end"]
                .into_iter()
                .map(str::to_string)
                .chain((0..30).map(|index| format!("line {index}")))
                .collect::<Vec<_>>()
        );
        let mut tail = String::new();
        append_stderr(&mut tail, &format!("first\n{}\n", "x".repeat(5000)));
        assert_eq!(tail.chars().count(), STDERR_TAIL_CHARS);
        assert_eq!(tail, format!("{}\n", "x".repeat(STDERR_TAIL_CHARS - 1)));
    }

    #[test]
    fn remote_errors_distinguish_lost_from_refused() {
        let lost = RemoteError::lost("gone");
        assert_eq!(lost.code, "unknown");
        assert!(is_connection_lost(&lost));
        let refused = RemoteError::new(
            &json!({ "code": "EINVAL", "message": "bad", "path": "/x", "extra": 1 }),
        );
        assert_eq!(refused.path.as_deref(), Some("/x"));
        assert_eq!(refused.fields["extra"], 1);
        assert!(!is_connection_lost(&refused));
        assert_eq!(RemoteError::aborted().code, "aborted");
    }

    #[test]
    fn maps_daemon_platform_names_to_node_names() {
        let mut info = RemoteInfo::from_json(&json!({
            "protocol": 1, "version": "1.6.0", "os": "windows", "arch": "aarch64", "home": "C:\\h",
            "tmpdir": "C:\\t", "separator": "\\", "cwd": "C:\\", "driveCwds": {}, "pid": 1,
        }))
        .unwrap();
        assert_eq!(
            node_platform(&info),
            ("win32".to_string(), "arm64".to_string())
        );
        info.os = "macos".into();
        info.arch = "x86_64".into();
        assert_eq!(
            node_platform(&info),
            ("darwin".to_string(), "x64".to_string())
        );
        info.os = "linux".into();
        info.arch = "x86".into();
        assert_eq!(
            node_platform(&info),
            ("linux".to_string(), "ia32".to_string())
        );
    }
}
