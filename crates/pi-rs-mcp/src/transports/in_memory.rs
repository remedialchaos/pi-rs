//! The in-memory transport of `packages/mcp/src/transports/in-memory.ts`, for client and adapter
//! tests: two peers wired directly together.

use std::sync::{Arc, Mutex};

use crate::protocol::{JsonRpcMessage, McpTransportError};
use crate::transports::{McpTransport, McpTransportSendOptions, Subscription, TransportEvents};

/// An in-memory MCP transport. Build a connected pair with
/// [`create_in_memory_transport_pair`].
#[derive(Default)]
pub struct InMemoryTransport {
    events: TransportEvents,
    peer: Mutex<Option<Arc<InMemoryTransport>>>,
    started: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
}

impl InMemoryTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Wires this transport to its `peer`. Fails when it already has one.
    pub fn connect_peer(&self, peer: Arc<InMemoryTransport>) -> Result<(), McpTransportError> {
        let mut current = self
            .peer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_some() {
            return Err(McpTransportError::Message(
                "In-memory MCP transport already has a peer".to_string(),
            ));
        }
        *current = Some(peer);
        Ok(())
    }

    /// Exposed so tests can simulate transport-level failures.
    pub fn emit_error(&self, error: crate::protocol::McpErrorEvent) {
        self.events.emit_error(error);
    }

    fn started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn deliver(&self, message: &JsonRpcMessage) {
        if self.closed() {
            return;
        }
        self.events.emit_message(message);
    }
}

#[async_trait::async_trait]
impl McpTransport for InMemoryTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.closed() {
            return Err(McpTransportError::connection_closed());
        }
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn send(
        &self,
        message: &JsonRpcMessage,
        _options: &McpTransportSendOptions,
    ) -> Result<(), McpTransportError> {
        if !self.started() || self.closed() {
            return Err(McpTransportError::connection_closed());
        }
        let peer = self
            .peer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(peer) = peer.filter(|peer| peer.started() && !peer.closed()) else {
            return Err(McpTransportError::ConnectionClosed(
                "In-memory MCP peer is not connected".to_string(),
            ));
        };
        let copy = message.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            peer.deliver(&copy);
        });
        Ok(())
    }

    async fn close(&self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.events.emit_close();
        let peer = self
            .peer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(peer) = peer {
            peer.close().await;
        }
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

/// A client transport and a server transport wired together.
#[derive(Clone)]
pub struct InMemoryTransportPair {
    pub client: Arc<InMemoryTransport>,
    pub server: Arc<InMemoryTransport>,
}

/// Creates a connected pair of in-memory transports, for client and adapter tests.
pub fn create_in_memory_transport_pair() -> InMemoryTransportPair {
    let client = InMemoryTransport::new();
    let server = InMemoryTransport::new();
    let _ = client.connect_peer(server.clone());
    let _ = server.connect_peer(client.clone());
    InMemoryTransportPair { client, server }
}
