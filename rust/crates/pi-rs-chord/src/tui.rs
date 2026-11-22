//! Sandbox bridge protocol and a pi-rs-tui terminal adapter. This module performs no process or OS I/O.
use crate::{
    Error, Result,
    context::{Context, lock},
    link::{Link, LinkEnd},
    surfaces::{ColorDepth, ThemeMode},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::oneshot;
mod scope;
pub use scope::{Optimistic, TuiScope};
#[derive(Clone)]
pub struct TerminalTheme {
    theme: crate::state::NativeSource<crate::surfaces::Theme>,
    depth: ColorDepth,
}
impl TerminalTheme {
    pub fn new(theme: crate::surfaces::Theme, depth: ColorDepth) -> Self {
        Self {
            theme: crate::state::NativeSource::new(
                crate::state::Batch::new(|_| {}),
                theme,
                |_, _| false,
            ),
            depth,
        }
    }
    pub fn theme(&self) -> crate::surfaces::Theme {
        self.theme.value()
    }
    pub fn source(&self) -> crate::state::NativeSource<crate::surfaces::Theme> {
        self.theme.clone()
    }
    pub fn set(&self, theme: crate::surfaces::Theme) -> Result<()> {
        self.theme.set(theme, &Context::background())
    }
    pub fn fg(&self, role: &str, text: &str) -> String {
        format!(
            "\x1b[{}m{text}\x1b[39m",
            crate::surfaces::terminal_color(&self.theme(), role, false, self.depth)
        )
    }
    pub fn bg(&self, role: &str, text: &str) -> String {
        format!(
            "\x1b[{}m{text}\x1b[49m",
            crate::surfaces::terminal_color(&self.theme(), role, true, self.depth)
        )
    }
}
pub const BRIDGE: &str = "chordBridge";
pub const FRAME_UNITS: usize = 16 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeStart {
    pub base: String,
    pub combined: String,
    pub columns: usize,
    pub rows: usize,
    pub colors: ColorDepth,
    pub mode: ThemeMode,
    pub locales: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeRequest {
    Fetch { path: String },
    Base,
    Copy { text: String },
    Open { url: String },
    Attach { path: String },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxMessage {
    Write { data: String },
    SocketOpen { socket: u64 },
    SocketSend { socket: u64, bytes: Vec<u8> },
    SocketClose { socket: u64 },
    Request { id: u64, request: BridgeRequest },
    Outdated,
    Failed { message: String },
    Exit { code: i32 },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LauncherMessage {
    Input {
        data: String,
    },
    Resize {
        columns: usize,
        rows: usize,
    },
    SocketOpened {
        socket: u64,
    },
    SocketMessage {
        socket: u64,
        bytes: Vec<u8>,
    },
    SocketClosed {
        socket: u64,
    },
    Reply {
        id: u64,
        value: serde_json::Value,
    },
    Attached {
        id: u64,
        name: String,
        bytes: Vec<u8>,
    },
    Refused {
        id: u64,
        message: String,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeReply {
    Value(serde_json::Value),
    Attached { name: String, bytes: Vec<u8> },
}
/// Counts UTF-16 units, never splits a Unicode scalar or escape sequence, including unterminated OSC strings.
pub fn split_frames(data: &str) -> Vec<String> {
    if data.encode_utf16().count() <= FRAME_UNITS {
        return vec![data.into()];
    }
    let bytes = data.as_bytes();
    let mut result = Vec::new();
    let (mut start, mut at, mut units) = (0, 0, 0);
    while at < data.len() {
        let end = if bytes[at] == 27 {
            sequence_end(data, at)
        } else {
            at + data[at..].chars().next().map(char::len_utf8).unwrap_or(0)
        };
        let size = data[at..end].encode_utf16().count();
        if units + size > FRAME_UNITS && at > start {
            result.push(data[start..at].into());
            start = at;
            units = 0;
        }
        units += size;
        at = end;
    }
    result.push(data[start..].into());
    result
}
fn sequence_end(data: &str, start: usize) -> usize {
    let bytes = data.as_bytes();
    match bytes.get(start + 1) {
        Some(b'[') => {
            let mut at = start + 2;
            while at < bytes.len() && (0x20..=0x3f).contains(&bytes[at]) {
                at += 1;
            }
            if at < bytes.len() {
                at += data[at..].chars().next().map(char::len_utf8).unwrap_or(0);
            }
            at
        }
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            let mut at = start + 2;
            while at < bytes.len() {
                if bytes[at] == 7 {
                    return at + 1;
                }
                if bytes[at] == 27 && bytes.get(at + 1) == Some(&b'\\') {
                    return at + 2;
                }
                at += 1;
            }
            bytes.len()
        }
        Some(_) => {
            start
                + 1
                + data[start + 1..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0)
        }
        None => bytes.len(),
    }
}
type Sender = Arc<dyn Fn(SandboxMessage) -> Result<()> + Send + Sync>;
struct State {
    columns: usize,
    rows: usize,
    input: Vec<u8>,
    pending: BTreeMap<u64, oneshot::Sender<Result<BridgeReply>>>,
    sockets: BTreeMap<u64, Weak<BridgeSocket>>,
    closed: bool,
}
struct Inner {
    start: BridgeStart,
    send: Sender,
    state: Mutex<State>,
    ids: AtomicU64,
}
#[derive(Clone)]
pub struct SandboxChannel(Arc<Inner>);
impl SandboxChannel {
    pub fn new(
        start: BridgeStart,
        send: impl Fn(SandboxMessage) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                columns: start.columns,
                rows: start.rows,
                input: Vec::new(),
                pending: BTreeMap::new(),
                sockets: BTreeMap::new(),
                closed: false,
            }),
            start,
            send: Arc::new(send),
            ids: AtomicU64::new(1),
        }))
    }
    pub fn start(&self) -> &BridgeStart {
        &self.0.start
    }
    pub fn write(&self, data: &str) -> Result<()> {
        if lock(&self.0.state).closed {
            return Err(Error::Closed);
        }
        for data in split_frames(data) {
            (self.0.send)(SandboxMessage::Write { data })?;
        }
        Ok(())
    }
    pub async fn request(&self, request: BridgeRequest, context: &Context) -> Result<BridgeReply> {
        let id = self.0.ids.fetch_add(1, Ordering::AcqRel);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            state.pending.insert(id, tx);
        }
        struct Remove(SandboxChannel, u64);
        impl Drop for Remove {
            fn drop(&mut self) {
                lock(&self.0.0.state).pending.remove(&self.1);
            }
        }
        let _remove = Remove(self.clone(), id);
        (self.0.send)(SandboxMessage::Request { id, request })?;
        context
            .await_with(rx)
            .await
            .map_err(|e| crate::data::ChordError::new("aborted", e.to_string()))?
            .map_err(|_| Error::Closed)?
    }
    pub fn receive(&self, message: LauncherMessage) -> Result<()> {
        match message {
            LauncherMessage::Input { data } => lock(&self.0.state).input.extend(data.into_bytes()),
            LauncherMessage::Resize { columns, rows } => {
                let mut state = lock(&self.0.state);
                state.columns = columns;
                state.rows = rows;
            }
            LauncherMessage::Reply { id, value } => {
                if let Some(reply) = lock(&self.0.state).pending.remove(&id) {
                    let _ = reply.send(Ok(BridgeReply::Value(value)));
                }
            }
            LauncherMessage::Attached { id, name, bytes } => {
                if let Some(reply) = lock(&self.0.state).pending.remove(&id) {
                    let _ = reply.send(Ok(BridgeReply::Attached { name, bytes }));
                }
            }
            LauncherMessage::Refused { id, message } => {
                if let Some(reply) = lock(&self.0.state).pending.remove(&id) {
                    let _ = reply.send(Err(Error::Invalid(message)));
                }
            }
            LauncherMessage::SocketOpened { socket } => {
                let socket = lock(&self.0.state)
                    .sockets
                    .get(&socket)
                    .and_then(Weak::upgrade);
                if let Some(socket) = socket {
                    let queued = lock(&socket.queued).take().unwrap_or_default();
                    for bytes in queued {
                        socket.send(bytes)?;
                    }
                }
            }
            LauncherMessage::SocketMessage { socket, bytes } => {
                let socket = lock(&self.0.state)
                    .sockets
                    .get(&socket)
                    .and_then(Weak::upgrade);
                if let Some(socket) = socket {
                    socket.end.deliver(bytes);
                }
            }
            LauncherMessage::SocketClosed { socket } => {
                let socket = lock(&self.0.state)
                    .sockets
                    .remove(&socket)
                    .and_then(|s| s.upgrade());
                if let Some(socket) = socket {
                    socket.end.ended();
                }
            }
        }
        Ok(())
    }
    pub fn socket(&self) -> Result<Arc<dyn Link>> {
        let id = self.0.ids.fetch_add(1, Ordering::AcqRel);
        let socket = Arc::new(BridgeSocket {
            id,
            channel: Arc::downgrade(&self.0),
            queued: Mutex::new(Some(Vec::new())),
            end: LinkEnd::default(),
        });
        {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            state.sockets.insert(id, Arc::downgrade(&socket));
        }
        (self.0.send)(SandboxMessage::SocketOpen { socket: id })?;
        Ok(socket)
    }
    pub fn close(&self) {
        let (pending, sockets) = {
            let mut state = lock(&self.0.state);
            state.closed = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.sockets),
            )
        };
        for (_, reply) in pending {
            let _ = reply.send(Err(Error::Closed));
        }
        for (_, socket) in sockets {
            if let Some(socket) = socket.upgrade() {
                socket.end.ended();
            }
        }
    }
    pub fn terminal(&self) -> BridgeTerminal {
        BridgeTerminal {
            channel: self.clone(),
            error: None,
        }
    }
}
struct BridgeSocket {
    id: u64,
    channel: Weak<Inner>,
    queued: Mutex<Option<Vec<Vec<u8>>>>,
    end: LinkEnd,
}
impl Link for BridgeSocket {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if self.end.closed() {
            return Ok(());
        }
        let mut queued = lock(&self.queued);
        if let Some(queued) = queued.as_mut() {
            queued.push(bytes);
            return Ok(());
        }
        drop(queued);
        let channel = self.channel.upgrade().ok_or(Error::Closed)?;
        (channel.send)(SandboxMessage::SocketSend {
            socket: self.id,
            bytes,
        })
    }
    fn close(&self) {
        if self.end.closed() {
            return;
        }
        self.end.ended();
        if let Some(channel) = self.channel.upgrade() {
            lock(&channel.state).sockets.remove(&self.id);
            let _ = (channel.send)(SandboxMessage::SocketClose { socket: self.id });
        }
    }
    fn on_message(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.end.on_message(listener)
    }
    fn on_close(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.end.on_close(listener)
    }
    fn closed(&self) -> bool {
        self.end.closed()
    }
}
impl Drop for BridgeSocket {
    fn drop(&mut self) {
        self.close();
    }
}
pub struct BridgeTerminal {
    channel: SandboxChannel,
    error: Option<String>,
}
impl BridgeTerminal {
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}
impl pi_rs_tui::Terminal for BridgeTerminal {
    fn width(&self) -> usize {
        lock(&self.channel.0.state).columns
    }
    fn height(&self) -> usize {
        lock(&self.channel.0.state).rows
    }
    fn write(&mut self, data: &str) {
        if let Err(error) = self.channel.write(data) {
            self.error = Some(error.to_string());
        }
    }
    fn read_available(&mut self) -> Vec<u8> {
        std::mem::take(&mut lock(&self.channel.0.state).input)
    }
    fn is_tty(&self) -> bool {
        true
    }
}
