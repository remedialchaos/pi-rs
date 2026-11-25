//! MCP's stdio transport over a program that something else runs, for example on another machine
//! through pi-env (`packages/mcp/src/transports/pipe.ts`). The transport closes when the program
//! ends; a failed exit that `close()` did not cause is reported as an error first.

use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::protocol::{JsonRpcMessage, McpErrorEvent, McpTransportError};
use crate::transports::lines::{JsonRpcLineReader, JsonRpcLineReaderOptions, OnStderr};
use crate::transports::{McpTransport, McpTransportSendOptions, Subscription, TransportEvents};

/// After closing stdin, how long the program may take to exit before `close()` kills it.
const DEFAULT_CLOSE_TIMEOUT_MS: u64 = 2_000;

/// The data a pipe takes: bytes, or text as the transports write it.
#[derive(Clone, Debug)]
pub enum PipeData {
    Bytes(Vec<u8>),
    Text(String),
}

impl PipeData {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Text(text) => text.as_bytes(),
        }
    }
}

impl From<&str> for PipeData {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

/// The promise of a running program's end: `Ok` when it ended however it ended, `Err` with its
/// message when it failed. Awaitable repeatedly.
pub type PipeExited = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// A running program's stdio, as `@earendil-works/pi-env`'s `RemoteProcess` provides it.
pub trait McpPipe: Send + Sync {
    fn write(&self, data: PipeData);
    fn close_stdin(&self);
    /// Kill the program (and what it started). The pipe owns cleaning up what the program started.
    fn close(&self);
    /// Settles when the program has ended, whatever the reason; it must also settle after
    /// [`McpPipe::close`].
    fn exited(&self) -> PipeExited;
}

/// Receives one chunk of a program's output.
type OnBytes = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Handlers the opener connects to the program's output.
#[derive(Clone)]
pub struct McpPipeHandlers {
    pub on_stdout: OnBytes,
    pub on_stderr: OnBytes,
}

/// Starts the program; the handlers receive its output from then on. Fails with the program's
/// startup error.
pub type McpPipeOpener = Arc<
    dyn Fn(
            McpPipeHandlers,
        ) -> Pin<Box<dyn Future<Output = Result<Arc<dyn McpPipe>, String>> + Send>>
        + Send
        + Sync,
>;

/// Options of [`PipeTransport`].
#[derive(Clone)]
pub struct PipeTransportOptions {
    pub open: McpPipeOpener,
    pub max_message_bytes: Option<u64>,
    pub max_stderr_bytes: Option<u64>,
    pub on_stderr: Option<OnStderr>,
    /// After closing stdin, how long the program may take to exit before `close()` kills it.
    /// Default 2000.
    pub close_timeout_ms: Option<u64>,
}

/// MCP's stdio transport over a program that something else runs.
pub struct PipeTransport {
    events: Arc<TransportEvents>,
    options: PipeTransportOptions,
    lines: Arc<Mutex<JsonRpcLineReader>>,
    pipe: Mutex<Option<Arc<dyn McpPipe>>>,
    /// The transport's own handle, for tasks it spawns.
    weak: Mutex<Option<std::sync::Weak<PipeTransport>>>,
    started: AtomicBool,
    closed: AtomicBool,
    ended: AtomicBool,
}

impl PipeTransport {
    pub fn new(options: PipeTransportOptions) -> Arc<Self> {
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
        let this = Arc::new(Self {
            events,
            options,
            lines,
            pipe: Mutex::new(None),
            weak: Mutex::new(None),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        *this
            .weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(&this));
        this
    }

    /// The last `max_stderr_bytes` (default 64 KiB) of the program's stderr.
    pub fn stderr(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stderr()
    }

    /// The program ended: report an unfinished message, then close.
    fn exit(&self) {
        self.ended.store(true, Ordering::SeqCst);
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .end();
        self.events.emit_close();
    }

    /// `exited` failed: an error unless `close()` caused it, then the same as an exit.
    fn fail(&self, message: String) {
        if !self.closed.load(Ordering::SeqCst) {
            self.events.emit_error(McpErrorEvent::message(message));
        }
        self.exit();
    }
}

#[async_trait::async_trait]
impl McpTransport for PipeTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Message(
                "MCP pipe transport already started".to_string(),
            ));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::connection_closed());
        }
        let lines = self.lines.clone();
        let handlers = McpPipeHandlers {
            on_stdout: Arc::new(move |bytes: &[u8]| {
                lines
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .read_stdout(bytes)
            }),
            on_stderr: Arc::new({
                let lines = self.lines.clone();
                move |bytes: &[u8]| {
                    lines
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .read_stderr(bytes)
                }
            }),
        };
        let pipe = (self.options.open)(handlers)
            .await
            .map_err(McpTransportError::Message)?;
        if self.closed.load(Ordering::SeqCst) {
            // `close()` ran while the program was starting and found nothing to shut down. Nobody
            // else awaits `exited`, and a pipe may fail it after a kill.
            tokio::spawn({
                let pipe = pipe.clone();
                async move {
                    let _ = pipe.exited().await;
                }
            });
            pipe.close();
            return Err(McpTransportError::connection_closed());
        }
        *self
            .pipe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pipe.clone());
        let this = self
            .weak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        tokio::spawn(async move {
            match pipe.exited().await {
                Ok(()) => {
                    if let Some(this) = this {
                        this.exit();
                    }
                }
                Err(message) => {
                    if let Some(this) = this {
                        this.fail(message);
                    }
                }
            }
        });
        Ok(())
    }

    async fn send(
        &self,
        message: &JsonRpcMessage,
        _options: &McpTransportSendOptions,
    ) -> Result<(), McpTransportError> {
        let pipe = self
            .pipe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(pipe) = pipe else {
            return Err(McpTransportError::connection_closed());
        };
        if self.closed.load(Ordering::SeqCst) || self.ended.load(Ordering::SeqCst) {
            return Err(McpTransportError::connection_closed());
        }
        let payload = serde_json::to_string(message)
            .map_err(|error| McpTransportError::Message(error.to_string()))?;
        pipe.write(PipeData::Text(format!("{payload}\n")));
        Ok(())
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let pipe = self
            .pipe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(pipe) = pipe else {
            self.events.emit_close();
            return;
        };
        if self.ended.load(Ordering::SeqCst) {
            return;
        }
        // Shutdown per the spec: close stdin and let the program exit, then kill it.
        pipe.close_stdin();
        let close_timeout_ms = self
            .options
            .close_timeout_ms
            .unwrap_or(DEFAULT_CLOSE_TIMEOUT_MS);
        let exited_in_time = tokio::select! {
            biased;
            exited = pipe.exited() => {
                let _ = exited;
                true
            }
            _ = tokio::time::sleep(Duration::from_millis(close_timeout_ms)) => false,
        };
        if exited_in_time {
            return;
        }
        pipe.close();
        let _ = pipe.exited().await;
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
