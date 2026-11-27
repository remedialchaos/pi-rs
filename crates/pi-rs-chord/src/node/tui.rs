//! Native terminal launcher, bounded pipe codec, credential storage and output filtering.
use crate::{
    Error, Result,
    link::Link,
    tui::{BridgeRequest, BridgeStart, LauncherMessage, SandboxChannel, SandboxMessage},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
mod actions;
pub use actions::TerminalActions;
mod process;
#[cfg(windows)]
mod windows;
pub use process::LocalActions;
pub const MAX_PIPE_FRAME: usize = 17 * 1024 * 1024;
pub const MAX_HEADER: usize = 1024 * 1024;
pub fn frame(header: &Value, body: &[u8]) -> Result<Vec<u8>> {
    let header = serde_json::to_vec(header)?;
    let length = 4 + header.len() + body.len();
    if header.len() > MAX_HEADER || length > MAX_PIPE_FRAME {
        return Err(Error::Invalid("terminal frame too large".into()));
    }
    let mut bytes = Vec::with_capacity(length + 4);
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(body);
    Ok(bytes)
}
pub fn launcher_frame(message: &LauncherMessage) -> Result<Vec<u8>> {
    let mut header = serde_json::to_value(message)?;
    let body = match message {
        LauncherMessage::Input { data } => {
            header.as_object_mut().map(|v| v.remove("data"));
            data.as_bytes()
        }
        LauncherMessage::SocketMessage { bytes, .. } | LauncherMessage::Attached { bytes, .. } => {
            header.as_object_mut().map(|v| v.remove("bytes"));
            bytes.as_slice()
        }
        _ => &[],
    };
    frame(&header, body)
}
#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}
impl FrameReader {
    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<SandboxMessage>> {
        let mut messages = Vec::new();
        while !chunk.is_empty() {
            let need = if self.buffer.len() < 4 {
                4 - self.buffer.len()
            } else {
                let length = u32::from_be_bytes(
                    self.buffer[..4]
                        .try_into()
                        .map_err(|_| Error::Invalid("bad pipe prefix".into()))?,
                ) as usize;
                if !(4..=MAX_PIPE_FRAME).contains(&length) {
                    self.buffer.clear();
                    return Err(Error::Invalid("malformed terminal frame length".into()));
                }
                length + 4 - self.buffer.len()
            };
            let take = need.min(chunk.len());
            self.buffer.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.buffer.len() >= 4 {
                let length = u32::from_be_bytes(
                    self.buffer[..4]
                        .try_into()
                        .map_err(|_| Error::Invalid("bad pipe prefix".into()))?,
                ) as usize;
                if !(4..=MAX_PIPE_FRAME).contains(&length) {
                    self.buffer.clear();
                    return Err(Error::Invalid("malformed terminal frame length".into()));
                }
            }
            if self.buffer.len() >= 8 {
                let length = u32::from_be_bytes(
                    self.buffer[..4]
                        .try_into()
                        .map_err(|_| Error::Invalid("bad pipe prefix".into()))?,
                ) as usize;
                let header = u32::from_be_bytes(
                    self.buffer[4..8]
                        .try_into()
                        .map_err(|_| Error::Invalid("bad header prefix".into()))?,
                ) as usize;
                if header > MAX_HEADER || header + 4 > length {
                    self.buffer.clear();
                    return Err(Error::Invalid("malformed terminal header length".into()));
                }
                if self.buffer.len() == length + 4 {
                    messages.push(parse_message(
                        &self.buffer[8..8 + header],
                        &self.buffer[8 + header..],
                    )?);
                    self.buffer.clear();
                }
            }
        }
        Ok(messages)
    }
    pub fn finish(&self) -> Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid("truncated terminal frame".into()))
        }
    }
}
fn parse_message(header: &[u8], body: &[u8]) -> Result<SandboxMessage> {
    let mut header: Value = serde_json::from_slice(header)?;
    let kind = header
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let object = header
        .as_object_mut()
        .ok_or_else(|| Error::Invalid("malformed terminal message".into()))?;
    match kind.as_str() {
        "write" => {
            object.insert(
                "data".into(),
                Value::String(String::from_utf8_lossy(body).into_owned()),
            );
        }
        "socket_send" => {
            object.insert("bytes".into(), serde_json::to_value(body)?);
        }
        "exit"
            if !object
                .get("code")
                .is_some_and(|v| v.as_i64().is_some_and(|n| i32::try_from(n).is_ok())) =>
        {
            object.insert("code".into(), serde_json::json!(1));
        }
        _ => {}
    }
    for key in ["socket", "id"] {
        if let Some(value) = object.get(key)
            && !value
                .as_u64()
                .is_some_and(|n| n <= crate::link::MAX_SAFE_INTEGER)
        {
            return Err(Error::Invalid("invalid terminal message id".into()));
        }
    }
    Ok(serde_json::from_value(header)?)
}
pub fn filter_output(output: &str) -> Result<String> {
    if output.len() > 64 * 1024 {
        return Err(Error::Invalid("terminal frame exceeds 64 KiB".into()));
    }
    let chars = output.chars().collect::<Vec<_>>();
    let mut kept = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c) {
            let escaped = c == '\u{1b}';
            let code = if escaped {
                i += 1;
                chars.get(i).copied().unwrap_or('\0')
            } else {
                c
            };
            if code == '[' || c == '\u{9b}' {
                i += 1;
                let start = i;
                while chars.get(i).is_some_and(|c| (' '..='?').contains(c)) {
                    i += 1;
                }
                if let Some(end) = chars.get(i) {
                    let body = chars[start..i].iter().collect::<String>();
                    if escaped && allowed_csi(&body, *end) {
                        kept.push_str("\x1b[");
                        kept.push_str(&body);
                        kept.push(*end);
                    }
                }
            } else if "]PX^_".contains(code)
                || ['\u{90}', '\u{98}', '\u{9d}', '\u{9e}', '\u{9f}'].contains(&c)
            {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\u{7}' || chars[i] == '\u{9c}' {
                        break;
                    }
                    if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'\\') {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            } else {
                while chars.get(i).is_some_and(|c| (' '..='/').contains(c)) {
                    i += 1;
                }
            }
        } else if c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\u{7f}') {
            kept.push(c);
        }
        i += 1;
    }
    Ok(kept)
}
fn allowed_csi(body: &str, end: char) -> bool {
    if body.len() > 128 {
        return false;
    }
    if matches!(end, 'h' | 'l') {
        return body.strip_prefix('?').is_some_and(|mode| {
            [
                "25", "2004", "2026", "1049", "7", "1000", "1002", "1006", "1004",
            ]
            .contains(&mode)
        });
    }
    let mut digits = 0;
    for b in body.bytes() {
        if b.is_ascii_digit() {
            digits += 1;
            if digits >= 5 {
                return false;
            }
        } else {
            digits = 0;
            if b != b';' && !(end == 'm' && b == b':') {
                return false;
            }
        }
    }
    match end {
        'm' => true,
        'A'..='H' | 'f' | 'd' => {
            body.split(';').count() <= if matches!(end, 'H' | 'f') { 2 } else { 1 }
        }
        'J' | 'K' => ["", "0", "1", "2"].contains(&body),
        'r' => {
            body.is_empty()
                || body
                    .split_once(';')
                    .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains(';'))
        }
        's' | 'u' => body.is_empty(),
        _ => false,
    }
}
pub fn save_credentials(path: &Path, token: &str) -> Result<bool> {
    super::app::write_secret(path, token, false)
}
pub fn read_credentials(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        return Err(Error::Invalid("empty terminal token".into()));
    }
    Ok(token)
}
pub struct TerminalLauncher {
    links: BTreeMap<u64, Arc<dyn Link>>,
    token: String,
}
impl TerminalLauncher {
    pub fn new(token: String) -> Self {
        Self {
            links: BTreeMap::new(),
            token,
        }
    }
    pub async fn connect(&mut self, id: u64, url: &str) -> Result<Arc<dyn Link>> {
        if self.links.contains_key(&id) {
            return Err(Error::Invalid("duplicate terminal socket".into()));
        }
        let mut headers = ::http::HeaderMap::new();
        headers.insert(
            "Authorization",
            ::http::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|_| Error::Invalid("invalid terminal token".into()))?,
        );
        let link = super::http::connect_websocket(url, headers, 1024 * 1024, 8 * 1024 * 1024)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.links.insert(id, link.clone());
        Ok(link)
    }
    pub fn render(
        &self,
        component: &dyn pi_rs_tui::Component,
        width: usize,
        output: &mut impl std::io::Write,
    ) -> Result<()> {
        output.write_all(filter_output(&component.render(width).join("\r\n"))?.as_bytes())?;
        Ok(())
    }
    pub fn apply(
        &mut self,
        message: SandboxMessage,
        output: &mut impl std::io::Write,
    ) -> Result<Option<crate::tui::BridgeRequest>> {
        match message {
            SandboxMessage::Write { data } => output.write_all(filter_output(&data)?.as_bytes())?,
            SandboxMessage::SocketSend { socket, bytes } => self
                .links
                .get(&socket)
                .ok_or_else(|| Error::Invalid("unknown terminal socket".into()))?
                .send(bytes)?,
            SandboxMessage::SocketClose { socket } => {
                if let Some(link) = self.links.remove(&socket) {
                    link.close();
                }
            }
            SandboxMessage::Request { request, .. } => {
                if let crate::tui::BridgeRequest::Open { url } = &request
                    && !url.starts_with("https://")
                    && !url.starts_with("http://")
                {
                    return Err(Error::Invalid("unsafe terminal URL".into()));
                }
                return Ok(Some(request));
            }
            SandboxMessage::Failed { message } => return Err(Error::Invalid(message)),
            SandboxMessage::Exit { .. } => self.close(),
            _ => {}
        }
        Ok(None)
    }
    pub fn close(&mut self) {
        for (_, link) in std::mem::take(&mut self.links) {
            link.close();
        }
    }
}
impl Drop for TerminalLauncher {
    fn drop(&mut self) {
        self.close();
    }
}

const PREFIX: &str = "/_chord/tui/";
const MAX_BUNDLE: usize = 8 * 1024 * 1024;
const MAX_SOCKETS: usize = 4;
const QUEUE_LENGTH: usize = 16;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

/// Trusted host-side GET adapter. It must use the exact URI, disable redirects, and stop
/// reading after the supplied byte limit. The sandbox never receives this capability or token.
pub trait SurfaceGet {
    fn get(
        &self,
        uri: http::Uri,
        authorization: http::HeaderValue,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

fn surface_path(path: &str) -> Result<&str> {
    if path.len() > 4096 {
        return Err(Error::Invalid("terminal surface path too long".into()));
    }
    let name = path
        .strip_prefix(PREFIX)
        .map(|name| name.strip_prefix("plugin/").unwrap_or(name))
        .filter(|name| !matches!(*name, "" | "." | ".."))
        .filter(|name| {
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.~@-".contains(&b))
        });
    name.map(|_| path)
        .ok_or_else(|| Error::Invalid("not a terminal surface file".into()))
}

/// Terminal settings supplied by the native host, independent of the server manifest.
pub struct TerminalSettings {
    pub columns: usize,
    pub rows: usize,
    pub colors: crate::surfaces::ColorDepth,
    pub mode: crate::surfaces::ThemeMode,
    pub locales: Vec<String>,
}

/// One authenticated sandbox session. Drive it with `step` until exit or host shutdown.
/// Local machine actions are refused here; they require a separate trusted UI.
pub struct TerminalSession<G> {
    launcher: TerminalLauncher,
    get: G,
    origin: http::Uri,
    authorization: http::HeaderValue,
    websocket: String,
    channel: SandboxChannel,
    replies: Option<mpsc::Sender<LauncherMessage>>,
    requests: mpsc::Receiver<SandboxMessage>,
    overloaded: Arc<AtomicBool>,
}

impl<G: SurfaceGet> TerminalSession<G> {
    /// Fetch the live manifest and its artifacts, returning the session and native base bytes.
    pub async fn start(
        origin: http::Uri,
        token: String,
        settings: TerminalSettings,
        get: G,
    ) -> Result<(Self, Vec<u8>)> {
        #[derive(serde::Deserialize)]
        struct Manifest {
            base: String,
            script: String,
            combined: String,
        }
        let ws = match origin.scheme_str() {
            Some("http") => "ws",
            Some("https") => "wss",
            _ => return Err(Error::Invalid("invalid terminal origin".into())),
        };
        let authority = origin
            .authority()
            .filter(|authority| !authority.as_str().contains('@'))
            .filter(|_| origin.path() == "/" && origin.query().is_none())
            .ok_or_else(|| Error::Invalid("invalid terminal origin".into()))?;
        let websocket = format!("{ws}://{authority}/_chord/link");
        let mut authorization = http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Invalid("invalid terminal token".into()))?;
        authorization.set_sensitive(true);
        let manifest: Manifest = serde_json::from_slice(
            &Self::fetch(&get, &origin, &authorization, "/_chord/tui/start").await?,
        )?;
        let (script, combined) = tokio::try_join!(
            Self::fetch(&get, &origin, &authorization, &manifest.script),
            Self::fetch(&get, &origin, &authorization, &manifest.combined),
        )?;
        let combined = String::from_utf8(combined)
            .map_err(|_| Error::Invalid("invalid terminal plugin manifest".into()))?;
        let (sender, requests) = mpsc::channel(QUEUE_LENGTH);
        let overloaded = Arc::new(AtomicBool::new(false));
        let sender_overloaded = overloaded.clone();
        let channel = SandboxChannel::new(
            BridgeStart {
                base: manifest.base,
                combined,
                columns: settings.columns,
                rows: settings.rows,
                colors: settings.colors,
                mode: settings.mode,
                locales: settings.locales,
            },
            move |message| {
                if matches!(&message, SandboxMessage::SocketSend { bytes, .. } if bytes.len() > 1024 * 1024)
                {
                    return Err(Error::Invalid("terminal socket frame too large".into()));
                }
                if let SandboxMessage::Request {
                    request: BridgeRequest::Fetch { path },
                    ..
                } = &message
                {
                    surface_path(path)?;
                }
                sender.try_send(message).map_err(|err| {
                    if matches!(err, mpsc::error::TrySendError::Full(_)) {
                        sender_overloaded.store(true, Ordering::Release);
                    }
                    Error::Closed
                })
            },
        );
        Ok((
            Self {
                launcher: TerminalLauncher::new(token),
                get,
                origin,
                authorization,
                websocket,
                channel,
                replies: None,
                requests,
                overloaded,
            },
            script,
        ))
    }

    async fn fetch(
        get: &G,
        origin: &http::Uri,
        auth: &http::HeaderValue,
        path: &str,
    ) -> Result<Vec<u8>> {
        let mut parts = origin.clone().into_parts();
        parts.path_and_query = Some(
            surface_path(path)?
                .parse()
                .map_err(|_| Error::Invalid("invalid terminal path".into()))?,
        );
        let uri = http::Uri::from_parts(parts)
            .map_err(|_| Error::Invalid("invalid terminal URI".into()))?;
        let bytes = tokio::time::timeout(NETWORK_TIMEOUT, get.get(uri, auth.clone(), MAX_BUNDLE))
            .await
            .map_err(|_| Error::Invalid("terminal fetch timed out".into()))??;
        if bytes.len() > MAX_BUNDLE {
            return Err(Error::Invalid("terminal artifact exceeds 8 MiB".into()));
        }
        Ok(bytes)
    }

    pub fn channel(&self) -> &SandboxChannel {
        &self.channel
    }

    async fn open(&mut self, socket: u64) -> Result<()> {
        self.launcher.links.retain(|_, link| !link.closed());
        if self.launcher.links.contains_key(&socket) {
            return Ok(());
        }
        let channel = self.channel.clone();
        let replies = self.replies.clone();
        let send = move |message| match &replies {
            Some(tx) => tx.try_send(message).map_err(|_| Error::Closed),
            None => channel.receive(message),
        };
        let link = if self.launcher.links.len() < MAX_SOCKETS {
            tokio::time::timeout(
                NETWORK_TIMEOUT,
                self.launcher.connect(socket, &self.websocket),
            )
            .await
            .ok()
            .and_then(Result::ok)
        } else {
            None
        };
        let Some(link) = link else {
            return send(LauncherMessage::SocketClosed { socket });
        };
        let send_close = send.clone();
        link.on_close(Arc::new(move || {
            let _ = send_close(LauncherMessage::SocketClosed { socket });
        }));
        send(LauncherMessage::SocketOpened { socket })?;
        let weak = Arc::downgrade(&link);
        link.on_message(Arc::new(move |bytes| {
            if send(LauncherMessage::SocketMessage { socket, bytes }).is_err()
                && let Some(link) = weak.upgrade()
            {
                link.close();
            }
        }));
        Ok(())
    }

    /// Process one message. Returns the sandbox exit code when the session ends.
    /// Cancelling an in-progress step may discard its message; cancel only when closing.
    pub async fn step(&mut self, output: &mut impl std::io::Write) -> Result<Option<i32>> {
        let result = self.step_inner(output).await;
        if !matches!(result, Ok(None)) {
            self.close();
        }
        result
    }

    async fn step_inner(&mut self, output: &mut impl std::io::Write) -> Result<Option<i32>> {
        let message = self.requests.recv().await;
        if self.overloaded.load(Ordering::Acquire) {
            return Err(Error::Invalid("terminal bridge queue full".into()));
        }
        self.handle(message, output).await
    }

    async fn handle(
        &mut self,
        message: Option<SandboxMessage>,
        output: &mut impl std::io::Write,
    ) -> Result<Option<i32>> {
        match message {
            None => return Ok(Some(0)),
            Some(SandboxMessage::Exit { code }) => return Ok(Some(code)),
            Some(SandboxMessage::SocketOpen { socket }) => self.open(socket).await?,
            Some(SandboxMessage::SocketSend { socket, bytes }) => {
                if bytes.len() > 1024 * 1024 {
                    return Err(Error::Invalid("terminal socket frame too large".into()));
                }
                if let Some(link) = self.launcher.links.get(&socket)
                    && link.send(bytes).is_err()
                {
                    link.close();
                }
            }
            Some(SandboxMessage::Request { id, request }) => {
                let (path, json) = match &request {
                    BridgeRequest::Fetch { path } => (Some(path.as_str()), false),
                    BridgeRequest::Base => (Some("/_chord/tui/base"), true),
                    _ => (None, false),
                };
                let result = if let Some(path) = path {
                    Self::fetch(&self.get, &self.origin, &self.authorization, path)
                        .await
                        .and_then(|bytes| {
                            if json {
                                Ok(serde_json::from_slice(&bytes)?)
                            } else {
                                Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                            }
                        })
                } else {
                    Err(Error::Invalid("terminal action requires trusted UI".into()))
                };
                let reply = match result {
                    Ok(value) => LauncherMessage::Reply { id, value },
                    Err(_) => LauncherMessage::Refused {
                        id,
                        message: "terminal request refused".into(),
                    },
                };
                if let Some(replies) = &self.replies {
                    replies.try_send(reply).map_err(|_| Error::Closed)?;
                } else {
                    self.channel.receive(reply)?;
                }
            }
            Some(message) => {
                self.launcher.apply(message, output)?;
            }
        }
        Ok(None)
    }
}

impl<G> TerminalSession<G> {
    pub fn close(&mut self) {
        self.requests.close();
        while self.requests.try_recv().is_ok() {}
        self.channel.close();
        self.launcher.close();
    }
}

impl<G> Drop for TerminalSession<G> {
    fn drop(&mut self) {
        self.close();
    }
}
