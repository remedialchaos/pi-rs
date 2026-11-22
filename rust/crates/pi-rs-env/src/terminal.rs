//! A program in a pseudo-terminal (port of `packages/env/src/terminal.ts`).

use crate::connection::{
    AbortController, Connection, EventCallback, RemoteError, Reply, RequestOptions, request_async,
};
use serde_json::{Map, Value, json};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const WRITE_CHUNK: usize = 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub type DataCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct TerminalOptions {
    pub cwd: String,
    /// Program and arguments. Default: the user's shell as a login shell.
    pub argv: Option<Vec<String>>,
    pub env: Option<Map<String, Value>>,
    pub inherit_env: Option<bool>,
    pub cols: u16,
    pub rows: u16,
    pub on_data: DataCallback,
}

/// The foreground job of a terminal: its process group id, and its leader's name if the system tells.
#[derive(Debug, Clone)]
pub struct TerminalForeground {
    pub pid: u32,
    pub name: Option<String>,
}

/// A program running in a pseudo-terminal of a pi-env daemon.
pub struct RemoteTerminal {
    pub pid: u32,
    connection: Arc<Connection>,
    id: u32,
    session: u64,
    controller: AbortController,
    result: Mutex<Option<Receiver<Result<Reply, RemoteError>>>>,
}

impl RemoteTerminal {
    /// Send input; once the terminal has ended, input is dropped.
    pub fn write(&self, data: &[u8]) {
        for chunk in data.chunks(WRITE_CHUNK) {
            self.request("ptyWrite", json!({}), chunk);
        }
    }

    /// Change the size; the program receives `SIGWINCH`.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.request("ptyResize", json!({ "cols": cols, "rows": rows }), &[]);
    }

    /// Kill the program and everything in its session.
    pub fn close(&self) {
        self.controller.abort();
    }

    /// The job in the terminal's foreground when it is not the program itself.
    pub fn foreground(&self) -> Result<Option<TerminalForeground>, RemoteError> {
        let reply = self.connection.request(
            "ptyForeground",
            json!({ "target": self.id }),
            RequestOptions {
                session: Some(self.session),
                ..RequestOptions::default()
            },
        )?;
        let Some(foreground) = reply.json.get("foreground") else {
            return Ok(None);
        };
        if foreground.is_null() {
            return Ok(None);
        }
        Ok(Some(TerminalForeground {
            pid: foreground.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            name: foreground
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        }))
    }

    fn request(&self, op: &str, extra: Value, payload: &[u8]) {
        let mut json_value = extra.as_object().cloned().unwrap_or_default();
        json_value.insert("target".to_string(), json!(self.id));
        let mut options = RequestOptions {
            session: Some(self.session),
            ..RequestOptions::default()
        };
        if !payload.is_empty() {
            options.payload = payload.to_vec();
        }
        let _ = self
            .connection
            .request(op, Value::Object(json_value), options);
    }

    /// Wait for the program's exit code.
    pub fn wait(&self) -> Result<i32, RemoteError> {
        let receiver = lock(&self.result).take();
        let Some(receiver) = receiver else {
            return Err(RemoteError::new(&serde_json::json!({
                "code": "unknown",
                "message": "the terminal result was already awaited",
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

/// Start a program in a new pseudo-terminal of the daemon behind `connection`.
pub fn open_terminal(
    connection: Arc<Connection>,
    options: TerminalOptions,
) -> Result<RemoteTerminal, RemoteError> {
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
    let on_data = options.on_data;
    let on_event: EventCallback =
        Arc::new(
            move |event, payload| match event.get("kind").and_then(Value::as_str) {
                Some("output") => on_data(payload),
                Some("started") => {
                    if let Some((id, session)) = *lock(&on_start_info) {
                        let pid = event.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
                        let _ = lock(&on_start_event).send((id, session, pid));
                    }
                }
                _ => {}
            },
        );
    let mut request = json!({
        "cwd": options.cwd,
        "cols": options.cols,
        "rows": options.rows,
    });
    if let Some(argv) = options.argv {
        request["argv"] = json!(argv);
    }
    if let Some(env) = options.env {
        request["env"] = Value::Object(env);
    }
    if let Some(inherit_env) = options.inherit_env {
        request["inheritEnv"] = json!(inherit_env);
    }
    let result = request_async(
        Arc::clone(&connection),
        "pty",
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
        if let Ok(early) = result.try_recv() {
            return Err(early.err().unwrap_or_else(|| {
                RemoteError::new(&serde_json::json!({ "code": "spawn_error", "message": "terminal ended before it started" }))
            }));
        }
        if Instant::now() >= deadline {
            controller.abort();
            return Err(RemoteError::lost("pi-env did not start the terminal"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(RemoteTerminal {
        pid,
        connection,
        id,
        session,
        controller,
        result: Mutex::new(Some(result)),
    })
}
