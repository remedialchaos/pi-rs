//! Public host and wire contracts. `Option<Value>` distinguishes undefined from JSON null.
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);
impl From<String> for HostError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    reason: Mutex<Option<String>>,
    wakers: Mutex<Vec<std::task::Waker>>,
}
impl CancellationToken {
    pub fn cancel(&self, reason: impl Into<String>) {
        if let Ok(mut state) = self.0.reason.lock()
            && state.is_none()
        {
            *state = Some(reason.into());
        }
        if let Ok(mut wakers) = self.0.wakers.lock() {
            for waker in wakers.drain(..) {
                waker.wake();
            }
        }
    }
    pub fn reason(&self) -> Option<String> {
        self.0.reason.lock().ok().and_then(|r| r.clone())
    }
    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }
    pub async fn cancelled(&self) -> String {
        futures::future::poll_fn(|cx| {
            if let Ok(mut wakers) = self.0.wakers.lock()
                && !wakers.iter().any(|w| w.will_wake(cx.waker()))
            {
                wakers.push(cx.waker().clone());
            }
            self.reason()
                .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
        })
        .await
    }
}
#[derive(Clone)]
pub struct ToolContext {
    pub signal: CancellationToken,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}
pub type ToolFuture = BoxFuture<'static, Result<Option<Value>, HostError>>;
pub type ToolExecutor = Arc<dyn Fn(Option<Value>, ToolContext) -> ToolFuture + Send + Sync>;
/// Resolves only after the QuickJS guest and its worker resources have been dropped.
#[derive(Clone, Default)]
pub struct WorkerExit(pub(crate) CancellationToken);
impl WorkerExit {
    pub async fn wait(&self) {
        self.0.cancelled().await;
    }
}
pub type WorkerRelease = Box<dyn FnOnce() + Send>;
pub type AcquireWorker = Arc<dyn Fn() -> Result<WorkerRelease, HostError> + Send + Sync>;
pub type OnWorkerStarted = Arc<dyn Fn(WorkerExit) -> Result<(), HostError> + Send + Sync>;
#[derive(Clone)]
pub struct CodemodeTool {
    pub declaration: ToolDeclaration,
    pub spread: bool,
    pub execute: ToolExecutor,
}
impl CodemodeTool {
    pub fn new<F, Fut>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<Value>, HostError>> + Send + 'static,
    {
        Self {
            declaration: ToolDeclaration {
                name: name.into(),
                ..Default::default()
            },
            spread: false,
            execute: Arc::new(move |value, context| Box::pin(execute(value, context))),
        }
    }
    pub fn name(&self) -> &str {
        &self.declaration.name
    }
}
#[derive(Clone)]
pub struct SandboxOptions {
    pub tools: Vec<CodemodeTool>,
    pub globals: Vec<CodemodeTool>,
    /// None disables the wall-clock deadline, like JavaScript Infinity.
    pub timeout: Option<Duration>,
    /// QuickJS guest heap byte cap, independent of the Wasmtime linear memory.
    pub memory_limit_bytes: Option<usize>,
    /// Explicit QuickJS-WASI guest file, otherwise the embedded pinned asset.
    pub wasm_path: Option<std::path::PathBuf>,
    pub acquire_worker: Option<AcquireWorker>,
    pub on_worker_started: Option<OnWorkerStarted>,
    pub loop_iteration_limit: u64,
    pub recursion_limit: usize,
}
impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: Some(Duration::from_secs(300)),
            memory_limit_bytes: None,
            wasm_path: None,
            acquire_worker: None,
            on_worker_started: None,
            loop_iteration_limit: 64 * 1024 * 1024,
            recursion_limit: 1500,
        }
    }
}
#[derive(Clone, Default)]
pub struct ExecuteOptions {
    /// Outer None inherits, Some(None) disables, Some(Some(d)) overrides.
    pub timeout: Option<Option<Duration>>,
    pub signal: CancellationToken,
    pub store: indexmap::IndexMap<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OutputItem {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        console: bool,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}
fn is_false(value: &bool) -> bool {
    !*value
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallStatus {
    Ok,
    Error,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeCall {
    pub name: String,
    pub status: CallStatus,
    pub duration_ms: f64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    Script,
    Timeout,
    Aborted,
    Sandbox,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodeError {
    pub kind: ErrorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}
impl CodemodeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            name: None,
            stack: None,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoreWrites {
    pub set: indexmap::IndexMap<String, Value>,
    pub delete: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct During {
    pub seq: u64,
    pub issued: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodemodeResult {
    Success {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        output: Vec<OutputItem>,
        calls: Vec<CodemodeCall>,
        #[serde(rename = "storeWrites")]
        store_writes: StoreWrites,
        #[serde(skip_serializing_if = "Option::is_none")]
        during: Option<During>,
    },
    Failure {
        ok: bool,
        error: CodemodeError,
        output: Vec<OutputItem>,
        calls: Vec<CodemodeCall>,
        #[serde(skip_serializing_if = "Option::is_none")]
        during: Option<During>,
    },
}
impl CodemodeResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Success { value, .. } => value.as_ref(),
            _ => None,
        }
    }
    pub fn error(&self) -> Option<&CodemodeError> {
        match self {
            Self::Failure { error, .. } => Some(error),
            _ => None,
        }
    }
    pub fn output(&self) -> &[OutputItem] {
        match self {
            Self::Success { output, .. } | Self::Failure { output, .. } => output,
        }
    }
    pub fn calls(&self) -> &[CodemodeCall] {
        match self {
            Self::Success { calls, .. } | Self::Failure { calls, .. } => calls,
        }
    }
}
