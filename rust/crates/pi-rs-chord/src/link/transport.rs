//! Structurally typed transport adapters. Ports own OS/WebSocket I/O, these ends enforce whole-frame contracts.
use crate::{Error, Result};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
pub type MessageListener = Arc<dyn Fn(Vec<u8>) + Send + Sync>;
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;
pub trait Link: Send + Sync {
    fn send(&self, bytes: Vec<u8>) -> Result<()>;
    fn close(&self);
    fn on_message(&self, listener: MessageListener);
    fn on_close(&self, listener: CloseListener);
    fn closed(&self) -> bool;
}
#[derive(Default)]
pub struct LinkEnd(Mutex<End>);
#[derive(Default)]
struct End {
    closed: bool,
    listener: Option<MessageListener>,
    close: Option<CloseListener>,
    queued: VecDeque<Vec<u8>>,
    delivering: bool,
}
impl LinkEnd {
    pub fn closed(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
    pub fn on_message(&self, listener: MessageListener) {
        let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if end.closed {
            return;
        }
        end.listener = Some(listener);
        drop(end);
        self.drain();
    }
    pub fn on_close(&self, listener: CloseListener) {
        let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if end.closed {
            drop(end);
            listener();
        } else {
            end.close = Some(listener);
        }
    }
    pub fn deliver(&self, bytes: Vec<u8>) {
        let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if end.closed {
            return;
        }
        end.queued.push_back(bytes);
        drop(end);
        self.drain();
    }
    fn drain(&self) {
        {
            let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if end.delivering {
                return;
            }
            end.delivering = true;
        }
        loop {
            let next = {
                let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
                if end.closed || end.listener.is_none() || end.queued.is_empty() {
                    end.delivering = false;
                    return;
                }
                end.queued.pop_front().zip(end.listener.clone())
            };
            if let Some((bytes, listener)) = next {
                listener(bytes);
            }
        }
    }
    pub fn ended(&self) {
        let listener = {
            let mut end = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if end.closed {
                return;
            }
            end.closed = true;
            end.queued.clear();
            end.listener = None;
            end.close.take()
        };
        if let Some(listener) = listener {
            listener();
        }
    }
}
struct MemoryLink {
    end: Arc<LinkEnd>,
    outgoing: tokio::sync::mpsc::UnboundedSender<Option<Vec<u8>>>,
}
impl Link for MemoryLink {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if !self.end.closed() {
            let _ = self.outgoing.send(Some(bytes));
        }
        Ok(())
    }
    fn close(&self) {
        if self.end.closed() {
            return;
        }
        self.end.ended();
        let _ = self.outgoing.send(None);
    }
    fn on_message(&self, f: MessageListener) {
        self.end.on_message(f)
    }
    fn on_close(&self, f: CloseListener) {
        self.end.on_close(f)
    }
    fn closed(&self) -> bool {
        self.end.closed()
    }
}
impl Drop for MemoryLink {
    fn drop(&mut self) {
        self.end.ended();
        let _ = self.outgoing.send(None);
    }
}
pub fn memory_link_pair() -> (Arc<dyn Link>, Arc<dyn Link>) {
    let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
    let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel();
    let a = Arc::new(MemoryLink {
        end: Arc::new(LinkEnd::default()),
        outgoing: a_tx,
    });
    let b = Arc::new(MemoryLink {
        end: Arc::new(LinkEnd::default()),
        outgoing: b_tx,
    });
    let b_end = Arc::downgrade(&b.end);
    tokio::spawn(async move {
        while let Some(message) = a_rx.recv().await {
            tokio::task::yield_now().await;
            let Some(end) = b_end.upgrade() else {
                return;
            };
            match message {
                Some(bytes) => end.deliver(bytes),
                None => {
                    end.ended();
                    return;
                }
            }
        }
    });
    let a_end = Arc::downgrade(&a.end);
    tokio::spawn(async move {
        while let Some(message) = b_rx.recv().await {
            tokio::task::yield_now().await;
            let Some(end) = a_end.upgrade() else {
                return;
            };
            match message {
                Some(bytes) => end.deliver(bytes),
                None => {
                    end.ended();
                    return;
                }
            }
        }
    });
    (a, b)
}
/// A Node-style IPC port preserves one byte array per message. False is backpressure, not rejection.
pub trait IpcPort: Send + Sync {
    fn connected(&self) -> bool;
    fn send(&self, bytes: Vec<u8>, done: Arc<dyn Fn(Result<()>) + Send + Sync>) -> Result<bool>;
    fn disconnect(&self);
    fn detach(&self);
}
pub struct IpcLink {
    end: Arc<LinkEnd>,
    port: Arc<dyn IpcPort>,
}
impl IpcLink {
    pub fn new(port: Arc<dyn IpcPort>) -> Arc<Self> {
        let end = Arc::new(LinkEnd::default());
        if !port.connected() {
            end.ended();
        }
        Arc::new(Self { end, port })
    }
    /// Application start/control messages are not link frames and are ignored.
    pub fn receive(&self, bytes: Option<Vec<u8>>) {
        if let Some(bytes) = bytes {
            self.end.deliver(bytes);
        }
    }
    pub fn disconnected(&self) {
        self.port.detach();
        self.end.ended();
    }
}
impl Link for IpcLink {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if self.end.closed() {
            return Ok(());
        }
        let end = self.end.clone();
        let port = self.port.clone();
        let done = Arc::new(move |result: Result<()>| {
            if result.is_err() {
                port.detach();
                end.ended();
            }
        });
        if let Err(error) = self.port.send(bytes, done) {
            self.disconnected();
            return Err(error);
        }
        Ok(())
    }
    fn close(&self) {
        if self.end.closed() {
            return;
        }
        self.disconnected();
        if self.port.connected() {
            self.port.disconnect();
        }
    }
    fn on_message(&self, f: MessageListener) {
        self.end.on_message(f)
    }
    fn on_close(&self, f: CloseListener) {
        self.end.on_close(f)
    }
    fn closed(&self) -> bool {
        self.end.closed()
    }
}
pub trait WebSocketPort: Send + Sync {
    fn ready_state(&self) -> u8;
    fn buffered_amount(&self) -> usize;
    fn send_binary(&self, bytes: Vec<u8>) -> Result<()>;
    fn close(&self, code: u16);
    fn detach_messages(&self);
}
pub struct WebSocketLink {
    end: LinkEnd,
    port: Arc<dyn WebSocketPort>,
    connecting: Mutex<Option<VecDeque<Vec<u8>>>>,
    max_payload: usize,
    max_buffered: usize,
}
impl WebSocketLink {
    pub fn new(port: Arc<dyn WebSocketPort>, max_payload: usize, max_buffered: usize) -> Arc<Self> {
        let state = port.ready_state();
        let end = LinkEnd::default();
        if state > 1 {
            end.ended();
        }
        Arc::new(Self {
            end,
            port,
            connecting: Mutex::new((state == 0).then(VecDeque::new)),
            max_payload,
            max_buffered,
        })
    }
    pub fn opened(&self) -> Result<()> {
        let queued = self
            .connecting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default();
        for bytes in queued {
            self.send(bytes)?;
        }
        Ok(())
    }
    /// Text/Blob frames are not protocol messages. Oversize frames use WebSocket close code 1009.
    pub fn receive(&self, binary: Option<Vec<u8>>) {
        if self.end.closed() {
            return;
        }
        match binary {
            Some(bytes) if bytes.len() <= self.max_payload => self.end.deliver(bytes),
            Some(_) => self.end_with(1009),
            None => self.end_with(1002),
        }
    }
    pub fn socket_closed(&self) {
        self.connecting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.port.detach_messages();
        self.end.ended();
    }
    fn end_with(&self, code: u16) {
        if !self.end.closed() {
            self.socket_closed();
            self.port.close(code);
        }
    }
}
impl Link for WebSocketLink {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if self.end.closed() {
            return Ok(());
        }
        if bytes.len() > self.max_payload {
            self.end_with(1009);
            return Err(Error::Invalid("websocket: frame exceeds maxPayload".into()));
        }
        let mut connecting = self.connecting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = connecting.as_mut() {
            let buffered = queue.iter().map(Vec::len).sum::<usize>();
            if buffered.saturating_add(bytes.len()) > self.max_buffered {
                drop(connecting);
                self.end_with(1009);
                return Err(Error::Invalid(
                    "websocket: connecting queue exceeds bound".into(),
                ));
            }
            queue.push_back(bytes);
            return Ok(());
        }
        drop(connecting);
        if self.port.buffered_amount().saturating_add(bytes.len()) > self.max_buffered {
            self.end_with(1009);
            return Err(Error::Invalid(
                "websocket: backpressure bound exceeded".into(),
            ));
        }
        if let Err(e) = self.port.send_binary(bytes) {
            self.end_with(1011);
            return Err(e);
        }
        Ok(())
    }
    fn close(&self) {
        self.end_with(1000);
    }
    fn on_message(&self, f: MessageListener) {
        self.end.on_message(f)
    }
    fn on_close(&self, f: CloseListener) {
        self.end.on_close(f)
    }
    fn closed(&self) -> bool {
        self.end.closed()
    }
}
