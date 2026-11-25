//! MCP transports: framing and I/O. A transport delivers individual JSON-RPC messages to the
//! client; the client owns request correlation, initialization, timeouts, cancellation, server
//! requests, and protocol-level helpers.

mod in_memory;
mod lines;
mod pipe;
mod stdio;
mod streamable_http;

pub use in_memory::{InMemoryTransport, InMemoryTransportPair, create_in_memory_transport_pair};
pub use lines::{JsonRpcLineReader, JsonRpcLineReaderOptions, OnError, OnMessage, OnStderr};
pub use pipe::{
    McpPipe, McpPipeHandlers, McpPipeOpener, PipeData, PipeExited, PipeTransport,
    PipeTransportOptions,
};
pub use stdio::{StdioStderr, StdioTransport, StdioTransportOptions};
pub use streamable_http::{
    ConsumeSseOptions, SseEvent, StreamableHttpReconnectOptions, StreamableHttpTransport,
    StreamableHttpTransportOptions, consume_sse_stream,
};

use std::sync::{Mutex, atomic::AtomicBool};

use crate::abort::AbortSignal;
use crate::protocol::{JsonRpcMessage, McpErrorEvent};
use indexmap::IndexMap;

/// The largest message the transports accept by default.
pub const DEFAULT_MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Receives each message a transport delivers.
pub type McpTransportMessageListener = std::sync::Arc<dyn Fn(&JsonRpcMessage) + Send + Sync>;
/// Receives transport errors; they are reported, and only close fails pending requests.
pub type McpTransportErrorListener = std::sync::Arc<dyn Fn(&McpErrorEvent) + Send + Sync>;
/// Called once when the transport closes, however it closed.
pub type McpTransportCloseListener = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Options for sending: `signal` is aborted once the sender no longer waits for a reply (the
/// request was cancelled, timed out, answered, or the client closed). A transport must not
/// dispatch the message after that, for example when retrying it after an authorization refresh.
/// It does not undo a message already dispatched.
#[derive(Clone, Default)]
pub struct McpTransportSendOptions {
    pub signal: Option<AbortSignal>,
}

/// A transport: framing and I/O for one MCP connection.
#[async_trait::async_trait]
pub trait McpTransport: Send + Sync {
    async fn start(&self) -> Result<(), crate::protocol::McpTransportError>;
    async fn send(
        &self,
        message: &JsonRpcMessage,
        options: &McpTransportSendOptions,
    ) -> Result<(), crate::protocol::McpTransportError>;
    async fn close(&self);
    fn on_message(&self, listener: McpTransportMessageListener) -> Subscription;
    fn on_error(&self, listener: McpTransportErrorListener) -> Subscription;
    fn on_close(&self, listener: McpTransportCloseListener) -> Subscription;
    fn set_protocol_version(&self, _version: &str) {}
}

/// Removes a listener, like the disposer functions upstream's `on*` methods return. The listener
/// stays registered until [`Subscription::unsubscribe`] is called; dropping the handle keeps it.
pub struct Subscription {
    remove: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    pub fn new(remove: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            remove: Some(Box::new(remove)),
        }
    }

    /// Removes the listener.
    pub fn unsubscribe(mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

/// Listener bookkeeping shared by transports. `emit_close` fires at most once per transport.
#[derive(Default)]
pub struct TransportEvents {
    inner: std::sync::Arc<Mutex<TransportEventsInner>>,
    close_emitted: AtomicBool,
}

#[derive(Default)]
struct TransportEventsInner {
    message_listeners: IndexMap<u64, McpTransportMessageListener>,
    error_listeners: IndexMap<u64, McpTransportErrorListener>,
    close_listeners: IndexMap<u64, McpTransportCloseListener>,
    next_id: u64,
}

impl TransportEvents {
    pub fn on_message(&self, listener: McpTransportMessageListener) -> Subscription {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.message_listeners.insert(id, listener);
        let inner = self.inner.clone();
        Subscription::new(move || {
            inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .message_listeners
                .shift_remove(&id);
        })
    }

    pub fn on_error(&self, listener: McpTransportErrorListener) -> Subscription {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.error_listeners.insert(id, listener);
        let inner = self.inner.clone();
        Subscription::new(move || {
            inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .error_listeners
                .shift_remove(&id);
        })
    }

    pub fn on_close(&self, listener: McpTransportCloseListener) -> Subscription {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.close_listeners.insert(id, listener);
        let inner = self.inner.clone();
        Subscription::new(move || {
            inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .close_listeners
                .shift_remove(&id);
        })
    }

    pub fn emit_message(&self, message: &JsonRpcMessage) {
        let listeners = self
            .lock()
            .message_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(message);
        }
    }

    pub fn emit_error(&self, error: McpErrorEvent) {
        let listeners = self
            .lock()
            .error_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(&error);
        }
    }

    pub fn emit_close(&self) {
        if self
            .close_emitted
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let listeners = self
            .lock()
            .close_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TransportEventsInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
