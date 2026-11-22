//! Native terminal launcher, bounded pipe codec, credential storage and output filtering.
use crate::{
    Error, Result,
    link::Link,
    tui::{LauncherMessage, SandboxMessage},
};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::Arc};
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
