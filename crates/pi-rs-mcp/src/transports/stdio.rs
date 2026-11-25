//! The stdio transport (`packages/mcp/src/transports/stdio.ts`): starts a local program that
//! speaks MCP over stdin and stdout, and shuts it down per the spec — close stdin, then SIGTERM,
//! then SIGKILL — applied to the server's whole process group.
//!
//! Upstream also keeps a registry of live process groups and kills them from a process-exit hook;
//! Rust has no portable equivalent of `process.once("exit")`, so a host that abandons a transport
//! without closing it leaves the server running (and `tokio::process::Child` reaps it here).

use std::process::Stdio as Std;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Notify, oneshot};

use crate::protocol::{JsonRpcMessage, McpErrorEvent, McpTransportError};
use crate::transports::lines::{JsonRpcLineReader, JsonRpcLineReaderOptions, OnStderr};
use crate::transports::{McpTransport, McpTransportSendOptions, Subscription, TransportEvents};

/// Time to wait for the server to exit after SIGTERM before sending SIGKILL. Default: 2000 ms.
const DEFAULT_CLOSE_TIMEOUT_MS: u64 = 2_000;
/// How long a server gets to exit on its own after stdin closes, before it is sent SIGTERM.
const STDIN_CLOSE_GRACE_MS: u64 = 500;

/// Where the server's stderr goes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum StdioStderr {
    /// Captured into the bounded tail (default).
    #[default]
    Pipe,
    /// Passed through to the host's stderr.
    Inherit,
}

/// Options of [`StdioTransport`].
#[derive(Clone, Default)]
pub struct StdioTransportOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Added to the environment.
    pub env: Option<indexmap::IndexMap<String, String>>,
    /// Whether the host environment is inherited (default) or replaced by `env`.
    pub inherit_env: Option<bool>,
    pub stderr: Option<StdioStderr>,
    pub on_stderr: Option<OnStderr>,
    pub max_message_bytes: Option<u64>,
    pub max_stderr_bytes: Option<u64>,
    /// Time to wait for the server to exit after SIGTERM before sending SIGKILL. Default: 2000 ms.
    pub close_timeout_ms: Option<u64>,
}

/// The signal of one escalation step of the shutdown.
#[derive(Clone, Copy)]
enum ShutdownSignal {
    Term,
    Kill,
}

/// A server started as a local program that speaks MCP over stdin and stdout.
pub struct StdioTransport {
    events: Arc<TransportEvents>,
    options: StdioTransportOptions,
    lines: Arc<Mutex<JsonRpcLineReader>>,
    watch: Arc<StdioWatch>,
    /// The writer queue: dropping the sender closes the server's stdin.
    stdin: Mutex<Option<tokio::sync::mpsc::UnboundedSender<WriteJob>>>,
    child_pid: Mutex<Option<u32>>,
    started: AtomicBool,
    closed: AtomicBool,
}

/// One queued write and the channel its completion or failure is reported on.
struct WriteJob {
    payload: String,
    reply: oneshot::Sender<Result<(), String>>,
}

/// When the server's stdio has fully closed, like Node's `close` event: the process exited and
/// every pipe reader reached EOF. Children inheriting the pipes keep it pending, exactly like
/// upstream's `close` event waiting for the streams.
struct StdioWatch {
    state: Mutex<StdioWatchState>,
    fired: AtomicBool,
    notify: Notify,
    events: Arc<TransportEvents>,
    lines: Arc<Mutex<JsonRpcLineReader>>,
}

#[derive(Default)]
struct StdioWatchState {
    child_exited: bool,
    stdout_drained: bool,
    stderr_drained: bool,
}

impl StdioWatch {
    fn new(events: Arc<TransportEvents>, lines: Arc<Mutex<JsonRpcLineReader>>) -> Self {
        Self {
            state: Mutex::new(StdioWatchState::default()),
            fired: AtomicBool::new(false),
            notify: Notify::new(),
            events,
            lines,
        }
    }

    /// Completes the watch once `start` knows whether stderr is piped.
    fn arm(&self, stderr_piped: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stdout_drained = false;
        state.stderr_drained = !stderr_piped;
    }

    fn note_child_exit(&self) {
        self.note(StdioPart::ChildExit)
    }

    fn note_stdout_drained(&self) {
        self.note(StdioPart::Stdout)
    }

    fn note_stderr_drained(&self) {
        self.note(StdioPart::Stderr)
    }

    fn note(&self, part: StdioPart) {
        let complete = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match part {
                StdioPart::ChildExit => state.child_exited = true,
                StdioPart::Stdout => state.stdout_drained = true,
                StdioPart::Stderr => state.stderr_drained = true,
            }
            state.child_exited && state.stdout_drained && state.stderr_drained
        };
        if complete {
            self.fire();
        }
    }

    fn fire(&self) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .end();
        self.events.emit_close();
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        if self.fired.load(Ordering::SeqCst) {
            return;
        }
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

enum StdioPart {
    ChildExit,
    Stdout,
    Stderr,
}

impl StdioTransport {
    pub fn new(options: StdioTransportOptions) -> Arc<Self> {
        let events = Arc::new(TransportEvents::default());
        let reader_options = JsonRpcLineReaderOptions {
            max_message_bytes: options.max_message_bytes,
            max_stderr_bytes: options.max_stderr_bytes,
            on_stderr: options.on_stderr.clone(),
            on_message: {
                let events = events.clone();
                Arc::new(move |message: &JsonRpcMessage| events.emit_message(message))
            },
            on_error: {
                let events = events.clone();
                Arc::new(move |error: McpErrorEvent| events.emit_error(error))
            },
        };
        let lines = Arc::new(Mutex::new(JsonRpcLineReader::new(reader_options)));
        let watch = Arc::new(StdioWatch::new(events.clone(), lines.clone()));
        Arc::new(Self {
            events,
            lines: lines.clone(),
            watch,
            options,
            stdin: Mutex::new(None),
            child_pid: Mutex::new(None),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        })
    }

    /// The server's process id, once started.
    pub fn pid(&self) -> Option<u32> {
        *self
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The last `max_stderr_bytes` (default 64 KiB) of the server's stderr.
    pub fn stderr(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stderr()
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Message(
                "MCP stdio transport already started".to_string(),
            ));
        }
        if self.closed() {
            return Err(McpTransportError::connection_closed());
        }

        let mut command = Command::new(&self.options.command);
        command.args(&self.options.args);
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        if self.options.inherit_env == Some(false) {
            command.env_clear();
        }
        if let Some(env) = &self.options.env {
            command.envs(env);
        }
        command.stdin(Std::piped());
        command.stdout(Std::piped());
        let stderr_piped = !matches!(self.options.stderr, Some(StdioStderr::Inherit));
        command.stderr(if stderr_piped {
            Std::piped()
        } else {
            Std::inherit()
        });
        // Own process group, so closing the transport can terminate the server's children too.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child: Child = command
            .spawn()
            .map_err(|error| McpTransportError::Message(error.to_string()))?;
        let pid = child.id();
        let stdin: Option<ChildStdin> = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = pid;
        self.watch.arm(stderr_piped);

        // Writes queue through one task, so closing never waits for a stuck write: dropping the
        // sender ends the server's stdin at once.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteJob>();
        *self
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tx);
        if let Some(mut stdin) = stdin {
            tokio::spawn(async move {
                while let Some(job) = rx.recv().await {
                    if let Err(error) = stdin.write_all(job.payload.as_bytes()).await {
                        let _ = job.reply.send(Err(error.to_string()));
                        break;
                    }
                    let _ = job.reply.send(Ok(()));
                }
                // The sender is gone or a write failed: the stdin handle drops here, closing it.
            });
        }

        // The child is reaped by its own wait, which also completes the close watch.
        let watch = self.watch.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            watch.note_child_exit();
        });

        if let Some(stdout) = stdout {
            let lines = self.lines.clone();
            let events = self.events.clone();
            let watch = self.watch.clone();
            tokio::spawn(async move {
                let mut stdout = tokio::io::BufReader::new(stdout);
                let mut chunk = [0u8; 8192];
                loop {
                    match stdout.read(&mut chunk).await {
                        Ok(0) => break,
                        Err(error) => {
                            events.emit_error(McpErrorEvent::message(error.to_string()));
                            break;
                        }
                        Ok(read) => {
                            lines
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .read_stdout(&chunk[..read]);
                        }
                    }
                }
                watch.note_stdout_drained();
            });
        } else {
            self.watch.note_stdout_drained();
        }

        if let Some(stderr) = stderr {
            let lines = self.lines.clone();
            let events = self.events.clone();
            let watch = self.watch.clone();
            tokio::spawn(async move {
                let mut stderr = tokio::io::BufReader::new(stderr);
                let mut chunk = [0u8; 8192];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) => break,
                        Err(error) => {
                            events.emit_error(McpErrorEvent::message(error.to_string()));
                            break;
                        }
                        Ok(read) => lines
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .read_stderr(&chunk[..read]),
                    }
                }
                watch.note_stderr_drained();
            });
        }
        Ok(())
    }

    async fn send(
        &self,
        message: &JsonRpcMessage,
        _options: &McpTransportSendOptions,
    ) -> Result<(), McpTransportError> {
        if !self.started.load(Ordering::SeqCst) || self.closed() {
            return Err(McpTransportError::connection_closed());
        }
        let (reply, reply_rx) = oneshot::channel();
        let payload = format!(
            "{}
",
            serde_json::to_string(message)
                .map_err(|error| McpTransportError::Message(error.to_string()))?
        );
        let queued = {
            let stdin = self
                .stdin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match stdin.as_ref() {
                Some(sender) => sender.send(WriteJob { payload, reply }).is_ok(),
                None => false,
            }
        };
        if !queued {
            return Err(McpTransportError::connection_closed());
        }
        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                if !self.closed() {
                    self.events
                        .emit_error(McpErrorEvent::message(error.clone()));
                }
                Err(McpTransportError::Message(error))
            }
            // The writer task is gone: the child's stdin was never created or already closed.
            Err(_) => Err(McpTransportError::connection_closed()),
        }
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Closing stdin starts the shutdown; a stuck write never holds it up.
        *self
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let Some(pid) = *self
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        else {
            self.events.emit_close();
            return;
        };
        // The server already closed on its own; nothing is left to shut down (upstream's
        // `this.child` is undefined by then, and `emitClose` is a no-op).
        if self.watch.fired.load(Ordering::SeqCst) {
            return;
        }
        let close_timeout_ms = self
            .options
            .close_timeout_ms
            .unwrap_or(DEFAULT_CLOSE_TIMEOUT_MS);
        let grace = STDIN_CLOSE_GRACE_MS.min(close_timeout_ms);
        tokio::select! {
            biased;
            _ = self.watch.wait() => {}
            _ = tokio::time::sleep(Duration::from_millis(grace)) => {
                kill_process_tree(pid, ShutdownSignal::Term);
            }
        }
        tokio::select! {
            biased;
            _ = self.watch.wait() => {}
            _ = tokio::time::sleep(Duration::from_millis(close_timeout_ms)) => {
                kill_process_tree(pid, ShutdownSignal::Kill);
            }
        }
        // Wait for the pipes to close: SIGKILL guarantees the end, children holding them included.
        self.watch.wait().await;
        // Children of the server that ignored stdin closing would otherwise outlive it.
        kill_process_tree(pid, ShutdownSignal::Term);
    }

    fn on_message(&self, listener: crate::transports::McpTransportMessageListener) -> Subscription {
        self.events.on_message(listener)
    }

    fn on_error(&self, listener: crate::transports::McpTransportErrorListener) -> Subscription {
        self.events.on_error(listener)
    }

    fn on_close(&self, listener: crate::transports::McpTransportCloseListener) -> Subscription {
        self.events.on_close(listener)
    }
}

/// Kills the server's whole process group, falling back to the direct child, like upstream's
/// `killProcessTree`: wrappers such as `npx` or `uvx` must not leave the server behind.
#[cfg(unix)]
fn kill_process_tree(pid: u32, signal: ShutdownSignal) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let signal = match signal {
        ShutdownSignal::Term => Signal::SIGTERM,
        ShutdownSignal::Kill => Signal::SIGKILL,
    };
    // A negative pid addresses the whole group.
    if pid > 0 && kill(Pid::from_raw(-(pid as i32)), signal).is_err() {
        // The group is gone or was never created; try the direct child.
        let _ = kill(Pid::from_raw(pid as i32), signal);
    }
}

/// Windows has no graceful signals, and `.cmd` shims run through `cmd.exe`; killing only that
/// would leave the server running, so the tree is force-killed.
#[cfg(windows)]
fn kill_process_tree(pid: u32, _signal: ShutdownSignal) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Std::null())
        .stdout(Std::null())
        .stderr(Std::null())
        .status();
}
