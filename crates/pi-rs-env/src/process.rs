//! A long-lived program with plain pipes, for programs that speak a protocol on stdin and stdout (port of
//! `packages/env/src/process.ts`).

use crate::connection::{
    AbortController, Connection, EventCallback, RemoteError, Reply, RequestOptions, request_async,
};
use serde_json::{Map, Value, json};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Input larger than this goes to the daemon in several writes; frames are at most 16 MiB.
const WRITE_CHUNK: usize = 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub type OutputCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct ProcessOptions {
    /// Working directory of the program; it must exist.
    pub cwd: String,
    /// Program and arguments, run directly without a shell.
    pub argv: Vec<String>,
    pub env: Option<Map<String, Value>>,
    pub inherit_env: Option<bool>,
    pub on_stdout: OutputCallback,
    pub on_stderr: Option<OutputCallback>,
}

/// A program running with plain pipes in a pi-env daemon. It ends when the program exits, `close()` kills it, or the
/// connection is lost (the daemon then kills it).
pub struct RemoteProcess {
    pub pid: u32,
    connection: Arc<Connection>,
    id: u32,
    session: u64,
    controller: AbortController,
    result: Mutex<Option<Receiver<Result<Reply, RemoteError>>>>,
}

impl RemoteProcess {
    /// Write to the program's stdin, in order; once stdin is closed or the program has ended, input is dropped.
    pub fn write(&self, data: &[u8]) {
        for chunk in data.chunks(WRITE_CHUNK) {
            self.request("processWrite", chunk);
        }
    }

    /// Close the program's stdin after the input written before it.
    pub fn close_stdin(&self) {
        self.request("processCloseStdin", &[]);
    }

    /// Kill the program and its process group.
    pub fn close(&self) {
        self.controller.abort();
    }

    fn request(&self, op: &str, payload: &[u8]) {
        let _ = self.connection.submit(
            self.session,
            op,
            json!({"target": self.id}),
            payload.to_vec(),
        );
    }

    /// Wait for the program's exit code. Rejects with the daemon's error, with code `aborted` after `close()`.
    pub fn wait(&self) -> Result<i32, RemoteError> {
        let receiver = lock(&self.result).take();
        let Some(receiver) = receiver else {
            return Err(RemoteError::new(&serde_json::json!({
                "code": "unknown",
                "message": "the process result was already awaited",
            })));
        };
        let reply = receiver
            .recv()
            .map_err(|_| RemoteError::lost("pi-env connection lost"))??;
        reply
            .json
            .get("exitCode")
            .and_then(Value::as_i64)
            .map(|code| code as i32)
            .ok_or_else(|| {
                RemoteError::new(
                    &serde_json::json!({ "code": "unknown", "message": "missing exitCode" }),
                )
            })
    }
}

/// Start a program with plain pipes in the daemon behind `connection`. Fails with `not_supported` when the daemon was
/// released before the `process` operation.
pub fn open_process(
    connection: Arc<Connection>,
    options: ProcessOptions,
) -> Result<RemoteProcess, RemoteError> {
    if !connection.info()?.has_feature("process") {
        return Err(RemoteError::new(&serde_json::json!({
            "code": "not_supported",
            "message": "The pi-env daemon cannot run processes; update it",
        })));
    }
    let controller = AbortController::new();
    let request_info: Arc<Mutex<Option<(u32, u64)>>> = Arc::new(Mutex::new(None));
    let (started_sender, started_receiver) = mpsc::channel::<(u32, u64, u32)>();
    let started_sender = Arc::new(Mutex::new(started_sender));
    let on_start_info = Arc::clone(&request_info);
    let on_start: Arc<dyn Fn(u32, u64) + Send + Sync> = Arc::new(move |id, session| {
        *lock(&on_start_info) = Some((id, session));
    });
    let on_start_event = Arc::clone(&started_sender);
    let on_start_info = Arc::clone(&request_info);
    let on_stdout = options.on_stdout;
    let on_stderr = options.on_stderr;
    let on_event: EventCallback =
        Arc::new(
            move |event, payload| match event.get("kind").and_then(Value::as_str) {
                Some("output") => {
                    if event.get("stream").and_then(Value::as_str) == Some("stderr") {
                        if let Some(callback) = &on_stderr {
                            callback(payload);
                        }
                    } else {
                        on_stdout(payload);
                    }
                }
                Some("started") => {
                    if let Some((id, session)) = *lock(&on_start_info) {
                        let pid = event.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
                        let _ = lock(&on_start_event).send((id, session, pid));
                    }
                }
                _ => {}
            },
        );
    let mut request = json!({ "cwd": options.cwd, "argv": options.argv });
    if let Some(env) = options.env {
        request["env"] = Value::Object(env);
    }
    if let Some(inherit_env) = options.inherit_env {
        request["inheritEnv"] = json!(inherit_env);
    }
    let result = request_async(
        Arc::clone(&connection),
        "process",
        request,
        RequestOptions {
            signal: Some(controller.signal()),
            on_start: Some(on_start),
            on_event: Some(on_event),
            ..RequestOptions::default()
        },
    );
    let deadline = Instant::now() + START_TIMEOUT;
    let (id, session, pid) = loop {
        if let Ok(started) = started_receiver.try_recv() {
            break started;
        }
        // Failing before it started rejects the open; afterwards, `wait` carries it.
        if let Ok(early) = result.try_recv() {
            return Err(early.err().unwrap_or_else(|| {
                RemoteError::new(&serde_json::json!({ "code": "spawn_error", "message": "process ended before it started" }))
            }));
        }
        if Instant::now() >= deadline {
            controller.abort();
            return Err(RemoteError::lost("pi-env did not start the process"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(RemoteProcess {
        pid,
        connection,
        id,
        session,
        controller,
        result: Mutex::new(Some(result)),
    })
}
