mod capture;
pub use capture::{ProcessCapture, PublishedLogs};
mod redactor;
mod snapshot;
pub use redactor::LogRedactor;
pub use snapshot::Publication;

use indexmap::IndexMap;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Stdout,
    Stderr,
    Event,
    Error,
}
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub id: u64,
    pub time: u64,
    pub level: LogLevel,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogNotice {
    pub id: u64,
    pub time: u64,
    pub source: String,
    pub message: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSource {
    pub id: String,
    pub lines: usize,
    pub updated_at: u64,
}
#[derive(Default)]
struct Buffer {
    lines: VecDeque<LogLine>,
    bytes: usize,
    time: u64,
}
type Listener = Arc<dyn Fn(&str) + Send + Sync>;
#[derive(Clone, Copy)]
pub struct LogLimits {
    pub lines: usize,
    pub bytes: usize,
    pub sources: usize,
    pub line_bytes: usize,
}
impl Default for LogLimits {
    fn default() -> Self {
        Self {
            lines: 1000,
            bytes: 256 * 1024,
            sources: 128,
            line_bytes: 16 * 1024,
        }
    }
}
#[derive(Default)]
struct State {
    limits: LogLimits,
    redactor: LogRedactor,
    buffers: IndexMap<String, Buffer>,
    notices: VecDeque<LogNotice>,
    id: u64,
    notice_id: u64,
    listeners: IndexMap<u64, Listener>,
    listener_id: u64,
}
#[derive(Clone, Default)]
pub struct LogStore(Arc<Mutex<State>>);
pub fn process_logs() -> &'static LogStore {
    static STORE: LazyLock<LogStore> = LazyLock::new(LogStore::default);
    &STORE
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl LogStore {
    pub fn with_limits(limits: LogLimits) -> std::io::Result<Self> {
        if [
            limits.lines,
            limits.bytes,
            limits.sources,
            limits.line_bytes,
        ]
        .contains(&0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid log bounds",
            ));
        }
        Ok(Self(Arc::new(Mutex::new(State {
            limits,
            ..State::default()
        }))))
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn add_directory(&self, path: impl Into<std::path::PathBuf>) {
        self.state().redactor.add_directory(path.into())
    }
    pub fn add_secret(&self, value: &str) {
        self.state().redactor.add_secret(value)
    }
    pub fn append(&self, source: &str, level: LogLevel, text: &str) {
        self.append_at(source, level, text, now())
    }
    pub fn append_at(&self, source: &str, level: LogLevel, text: &str, time: u64) {
        let mut s = self.state();
        let safe = s.redactor.redact(text);
        let mut b = s.buffers.shift_remove(source).unwrap_or_default();
        if s.buffers.len() >= s.limits.sources {
            s.buffers.shift_remove_index(0);
        }
        for raw in safe
            .split_inclusive('\n')
            .chain(safe.ends_with('\n').then_some(""))
            .chain(safe.is_empty().then_some(""))
        {
            let line = raw
                .strip_suffix("\r\n")
                .or_else(|| raw.strip_suffix('\n'))
                .unwrap_or(raw);
            let text = if line.len() > s.limits.line_bytes {
                "[oversized log line omitted]"
            } else {
                line
            };
            s.id += 1;
            b.bytes += text.len();
            b.lines.push_back(LogLine {
                id: s.id,
                time,
                level,
                text: text.into(),
            });
            while b.bytes > s.limits.bytes || b.lines.len() > s.limits.lines {
                if let Some(old) = b.lines.pop_front() {
                    b.bytes -= old.text.len();
                }
            }
        }
        b.time = time;
        s.buffers.insert(source.into(), b);
        drop(s);
        self.notify(source)
    }
    fn notify(&self, source: &str) {
        let listeners: Vec<_> = self.state().listeners.values().cloned().collect();
        for f in listeners {
            f(source)
        }
    }
    pub fn subscribe(&self, f: Listener) -> u64 {
        let mut s = self.state();
        s.listener_id += 1;
        let id = s.listener_id;
        s.listeners.insert(id, f);
        id
    }
    pub fn unsubscribe(&self, id: u64) {
        self.state().listeners.shift_remove(&id);
    }
    pub fn lines(&self, source: &str) -> Vec<LogLine> {
        self.state()
            .buffers
            .get(source)
            .map(|b| b.lines.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn sources(&self) -> Vec<LogSource> {
        self.state()
            .buffers
            .iter()
            .map(|(id, b)| LogSource {
                id: id.clone(),
                lines: b.lines.len(),
                updated_at: b.time,
            })
            .collect()
    }
    pub fn notices(&self) -> Vec<LogNotice> {
        self.state().notices.iter().cloned().collect()
    }
    pub fn notice(&self, source: &str, message: &str) {
        let mut s = self.state();
        let mut message = s.redactor.redact(message);
        if message.len() > s.limits.line_bytes {
            message = "[oversized error message omitted]".into()
        }
        s.notice_id += 1;
        let id = s.notice_id;
        s.notices.retain(|n| n.source != source);
        s.notices.push_back(LogNotice {
            id,
            time: now(),
            source: source.into(),
            message: message.clone(),
        });
        if s.notices.len() > 50 {
            s.notices.pop_front();
        }
        drop(s);
        self.append(source, LogLevel::Error, &message)
    }
    pub fn dismiss_notice(&self, id: u64) {
        let mut s = self.state();
        if let Some(i) = s.notices.iter().position(|n| n.id == id)
            && let Some(n) = s.notices.remove(i)
        {
            drop(s);
            self.notify(&n.source)
        }
    }
}
pub struct LogWriter {
    store: LogStore,
    source: String,
    level: LogLevel,
    pending: Vec<u8>,
    oversized: bool,
}
impl LogWriter {
    pub fn new(store: LogStore, source: String, level: LogLevel) -> Self {
        Self {
            store,
            source,
            level,
            pending: Vec::new(),
            oversized: false,
        }
    }
    pub fn write(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                if self.pending.last() == Some(&b'\r') {
                    self.pending.pop();
                }
                self.publish()
            } else if !self.oversized {
                if self.pending.len() == 65536 {
                    self.discard()
                } else {
                    self.pending.push(byte)
                }
            }
        }
    }
    pub fn discard(&mut self) {
        self.pending.clear();
        self.oversized = true
    }
    fn publish(&mut self) {
        let text = String::from_utf8_lossy(&self.pending);
        self.store.append(
            &self.source,
            self.level,
            if self.oversized || text.len() > 65536 {
                "[oversized log line omitted]"
            } else {
                &text
            },
        );
        self.pending.clear();
        self.oversized = false
    }
    pub fn end(&mut self) {
        if self.oversized || !self.pending.is_empty() {
            self.publish()
        }
    }
}

pub fn log_notices_contract() -> pi_rs_chord::Result<pi_rs_chord::contracts::Contract> {
    crate::api::v1::p2_contracts::lookup("p2.log-notices")
}
