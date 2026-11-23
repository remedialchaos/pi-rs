//! A watcher running inside the daemon, next to the files (port of `packages/env/src/watch.ts`).
//!
//! `pi-rs-durable` is ported in 2.3b; this module defines the Rust equivalents of its `FileChange`, `WatchTarget` and
//! watch-mode types so the bridge can wrap them without changing this transport.

use crate::connection::{
    AbortController, AbortSignal, Connection, EventCallback, RemoteError, RequestOptions,
    is_connection_lost,
};
use crate::errors::{FileErrorCode, to_file_error};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// How the daemon watches, like durable's `NodeWatchOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Native,
    Polling,
}

impl WatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchMode::Native => "native",
            WatchMode::Polling => "polling",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteWatchOptions {
    pub mode: Option<WatchMode>,
    pub poll_interval_ms: Option<u64>,
    pub max_directories: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct WatchExclude {
    pub hidden: bool,
    pub names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub path: String,
    pub recursive: bool,
    pub exclude: Option<WatchExclude>,
}

/// The changes a watcher reports.
#[derive(Debug, Clone)]
pub enum FileChange {
    Paths(Vec<String>),
    /// Changes while disconnected were not seen.
    Overflow,
    Failed {
        code: FileErrorCode,
        message: String,
    },
}

const RECONNECT_FIRST_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30_000;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct WatcherState {
    stopped: bool,
    mode: WatchMode,
    ready: AtomicBool,
}

/// Shared wake-up and state for the watcher's supervisor thread and its owner.
struct Shared {
    state: Mutex<WatcherState>,
    wake: Condvar,
}

impl Shared {
    fn stop(&self) {
        let mut state = lock(&self.state);
        state.stopped = true;
        self.wake.notify_all();
    }

    /// Wait for the backoff, returning false if the watcher was stopped.
    fn backoff(&self, delay: Duration) -> bool {
        let (state, _) = self
            .wake
            .wait_timeout(lock(&self.state), delay)
            .unwrap_or_else(PoisonError::into_inner);
        !state.stopped
    }

    fn ready(&self) -> bool {
        lock(&self.state).ready.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> bool {
        lock(&self.state).stopped
    }
}

pub type ChangeCallback = Arc<dyn Fn(FileChange) + Send + Sync>;

enum Status {
    Ready,
    Failed(RemoteError),
}

struct Supervisor {
    connection: Arc<Connection>,
    shared: Arc<Shared>,
    targets: Vec<WatchTarget>,
    options: RemoteWatchOptions,
    signal: AbortSignal,
    on_change: ChangeCallback,
    ready_status: Sender<Status>,
}

impl Supervisor {
    fn deliver(&self, change: FileChange) {
        if self.shared.stopped() {
            return;
        }
        (self.on_change)(change);
    }

    /// One watch request, ending when it is cancelled or the connection is lost. Signals `ready` to the opener.
    fn watch_request(&self) -> Result<(), RemoteError> {
        let mut request = json!({
            "targets": self.targets.iter().map(|target| {
                let exclude = target.exclude.clone().unwrap_or_default();
                json!({
                    "path": target.path,
                    "recursive": target.recursive,
                    "exclude": { "hidden": exclude.hidden, "names": exclude.names },
                })
            }).collect::<Vec<_>>(),
        });
        if let Some(mode) = self.options.mode {
            request["mode"] = json!(mode.as_str());
        }
        if let Some(poll) = self.options.poll_interval_ms {
            request["pollIntervalMs"] = json!(poll);
        }
        if let Some(max) = self.options.max_directories {
            request["maxDirectories"] = json!(max);
        }
        let deliver = Arc::clone(&self.on_change);
        let shared = Arc::clone(&self.shared);
        let status = self.ready_status.clone();
        let on_event: EventCallback =
            Arc::new(
                move |event, _payload| match event.get("kind").and_then(Value::as_str) {
                    Some("ready") => {
                        let mut state = lock(&shared.state);
                        state.mode = if event.get("mode").and_then(Value::as_str) == Some("polling")
                        {
                            WatchMode::Polling
                        } else {
                            WatchMode::Native
                        };
                        state.ready.store(true, Ordering::SeqCst);
                        drop(state);
                        let _ = status.send(Status::Ready);
                    }
                    Some("change") => {
                        if event.get("mode").and_then(Value::as_str) == Some("polling") {
                            lock(&shared.state).mode = WatchMode::Polling;
                        }
                        if event.get("overflow").and_then(Value::as_bool) == Some(true) {
                            deliver(FileChange::Overflow);
                        } else if let Some(paths) = event.get("paths").and_then(Value::as_array) {
                            deliver(FileChange::Paths(
                                paths
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect(),
                            ));
                        }
                    }
                    Some("error") => {
                        deliver(FileChange::Failed {
                            code: if event.get("code").and_then(Value::as_str)
                                == Some("permission_denied")
                            {
                                FileErrorCode::PermissionDenied
                            } else {
                                FileErrorCode::Invalid
                            },
                            message: event
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        });
                        shared.stop();
                    }
                    _ => {}
                },
            );
        self.connection
            .request(
                "watch",
                request,
                RequestOptions {
                    signal: Some(self.signal.clone()),
                    on_event: Some(on_event),
                    ..RequestOptions::default()
                },
            )
            .map(|_| ())
    }

    fn run(self) {
        let mut delay = Duration::from_millis(RECONNECT_FIRST_MS);
        loop {
            if self.shared.stopped() {
                return;
            }
            lock(&self.shared.state)
                .ready
                .store(false, Ordering::SeqCst);
            match self.watch_request() {
                Ok(()) => return,
                Err(error) => {
                    if !self.shared.ready() {
                        // The opener is still waiting for readiness; it fails with this error.
                        let _ = self.ready_status.send(Status::Failed(error));
                        return;
                    }
                    if self.shared.stopped() {
                        return;
                    }
                    if is_connection_lost(&error) {
                        if !self.shared.backoff(delay) {
                            return;
                        }
                        // Changes while disconnected were not seen.
                        self.deliver(FileChange::Overflow);
                        delay = delay
                            .saturating_mul(2)
                            .min(Duration::from_millis(RECONNECT_MAX_MS));
                        continue;
                    }
                    let file_error = to_file_error(&error, None);
                    self.deliver(FileChange::Failed {
                        code: file_error.code,
                        message: file_error.message,
                    });
                    self.shared.stop();
                    return;
                }
            }
        }
    }
}

/// A watcher running in the daemon. When the connection is lost it is opened again and reports `overflow`.
pub struct RemoteWatcher {
    shared: Arc<Shared>,
    controller: AbortController,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteWatcher {
    /// Watch `targets` (resolved paths); fails like `NodeFileWatcher.open` if coverage cannot be established.
    pub fn open(
        connection: Arc<Connection>,
        targets: Vec<WatchTarget>,
        on_change: ChangeCallback,
        options: RemoteWatchOptions,
    ) -> Result<RemoteWatcher, RemoteError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(WatcherState {
                stopped: false,
                mode: WatchMode::Native,
                ready: AtomicBool::new(false),
            }),
            wake: Condvar::new(),
        });
        let controller = AbortController::new();
        let (status, receiver) = mpsc::channel::<Status>();
        let supervisor = Supervisor {
            connection,
            shared: Arc::clone(&shared),
            targets,
            options,
            signal: controller.signal(),
            on_change,
            ready_status: status,
        };
        let thread = std::thread::spawn(move || supervisor.run());
        // Wait for coverage, or for the first attempt to fail before it was established.
        let opened = loop {
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(Status::Ready) => break Ok(()),
                Ok(Status::Failed(error)) => break Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if thread.is_finished() {
                        break Err(RemoteError::lost(
                            "pi-env watcher stopped before it was ready",
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(RemoteError::lost(
                        "pi-env watcher stopped before it was ready",
                    ));
                }
            }
        };
        match opened {
            Ok(()) => Ok(RemoteWatcher {
                shared,
                controller,
                thread: Mutex::new(Some(thread)),
            }),
            Err(error) => {
                shared.stop();
                controller.abort();
                let _ = thread.join();
                Err(error)
            }
        }
    }

    pub fn mode(&self) -> WatchMode {
        lock(&self.shared.state).mode
    }

    /// Stop watching. The daemon's request is cancelled and the background thread is joined.
    pub fn stop(&self) {
        if let Some(thread) = self.begin_stop() {
            let _ = thread.join();
        }
    }

    pub(crate) fn begin_stop(&self) -> Option<JoinHandle<()>> {
        self.shared.stop();
        self.controller.abort();
        lock(&self.thread).take()
    }
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}
