use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result, context::Context};
use pi_rs_env::{RemoteTerminal, TerminalForeground, TerminalOptions, open_terminal};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::runtime::{
    session_worker::Location,
    worker_environment::{EnvironmentProvider, Machine},
};

const RING: usize = 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;
const SCROLLBACK: usize = 5000;
const MATCH_WINDOW: u64 = 64 * 1024;
const MAX_AGENT_TERMINALS: usize = 8;

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub foreground: u32,
    pub background: u32,
    pub dark: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: 0xe6e6e6,
            background: 0x141414,
            dark: true,
        }
    }
}

struct VtEvents {
    theme: Theme,
    title: Option<String>,
    replies: Vec<u8>,
    report_theme: bool,
}

impl vt100::Callbacks for VtEvents {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).chars().take(256).collect());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let (query, color) = match params {
            [b"10", b"?"] => (10, self.theme.foreground),
            [b"11", b"?"] => (11, self.theme.background),
            _ => return,
        };
        let [_, r, g, b] = color.to_be_bytes();
        self.replies.extend_from_slice(
            format!("\x1b]{query};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\")
                .as_bytes(),
        );
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        match (first, params, command) {
            (Some(b'?'), [[2031]], 'h' | 'l') => self.report_theme = command == 'h',
            (Some(b'?'), [[996]], 'n') => self
                .replies
                .extend_from_slice(color_scheme(self.theme).as_bytes()),
            (None, [[6]], 'n') => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            _ => {}
        }
    }
}

fn color_scheme(theme: Theme) -> &'static str {
    if theme.dark {
        "\x1b[?997;1n"
    } else {
        "\x1b[?997;2n"
    }
}

enum Process {
    #[cfg(unix)]
    Local(pi_rs_env::daemon::LocalTerminal),
    Remote(RemoteTerminal),
}

impl Process {
    fn write(&self, bytes: &[u8]) {
        match self {
            #[cfg(unix)]
            Self::Local(process) => process.write(bytes),
            Self::Remote(process) => process.write(bytes),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        match self {
            #[cfg(unix)]
            Self::Local(process) => process.resize(cols, rows).map_err(invalid)?,
            Self::Remote(process) => process.resize(cols, rows),
        }
        Ok(())
    }

    fn foreground(&self) -> Option<TerminalForeground> {
        match self {
            #[cfg(unix)]
            Self::Local(process) => process.foreground().ok().flatten(),
            Self::Remote(process) => process.foreground().ok().flatten(),
        }
    }

    fn close(&self) {
        match self {
            #[cfg(unix)]
            Self::Local(process) => process.close(),
            Self::Remote(process) => process.close(),
        }
    }

    fn wait(&self) -> Result<i32> {
        match self {
            #[cfg(unix)]
            Self::Local(process) => process.wait().map_err(invalid),
            Self::Remote(process) => process.wait().map_err(invalid),
        }
    }
}

pub(super) fn invalid(message: impl ToString) -> Error {
    Error::Invalid(message.to_string())
}

pub(super) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Info {
    pub id: String,
    pub session_id: String,
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    #[serde(flatten)]
    pub location: Location,
    pub title: String,
    pub created_at: u64,
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited: Option<Value>,
}

struct Output {
    bytes: VecDeque<u8>,
    end: u64,
    cursor: u64,
    latest: Instant,
    vt: vt100::Parser<VtEvents>,
}

impl Output {
    fn append(&mut self, bytes: &[u8]) {
        self.vt.process(bytes);
        self.end += bytes.len() as u64;
        self.latest = Instant::now();
        let kept = &bytes[bytes.len().saturating_sub(RING)..];
        let evict = (self.bytes.len() + kept.len()).saturating_sub(RING);
        self.bytes.drain(..evict);
        self.bytes.extend(kept);
    }

    fn since(&self, from: u64, limit: usize) -> (Vec<u8>, u64) {
        let start = self.end - self.bytes.len() as u64;
        let skip = start.saturating_sub(from);
        let index = from.saturating_sub(start).min(self.bytes.len() as u64) as usize;
        (
            self.bytes.iter().skip(index).take(limit).copied().collect(),
            skip,
        )
    }
}

pub(super) struct Terminal {
    pub info: Mutex<Info>,
    output: Mutex<Output>,
    process: Mutex<Option<Arc<Process>>>,
    closed: AtomicBool,
    changed: watch::Sender<u64>,
}

impl Terminal {
    fn publish(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn bytes(&self, from: u64, limit: usize) -> (Vec<u8>, u64) {
        lock(&self.output).since(from, limit)
    }

    pub fn snapshot(&self) -> (u64, Vec<u8>) {
        let output = lock(&self.output);
        let screen = output.vt.screen();
        let mut bytes = b"\x1bc".to_vec();
        if screen.alternate_screen() {
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        bytes.extend(screen.state_formatted());
        (output.end, bytes)
    }

    pub fn offset(&self) -> u64 {
        lock(&self.output).cursor
    }

    pub async fn send(&self, bytes: Vec<u8>) -> Result<()> {
        let process = lock(&self.process).clone();
        if let Some(process) = process {
            tokio::task::spawn_blocking(move || process.write(&bytes))
                .await
                .map_err(invalid)?;
        }
        Ok(())
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let process = lock(&self.process).clone();
        if let Some(process) = process {
            tokio::task::spawn_blocking(move || process.resize(cols, rows))
                .await
                .map_err(invalid)??;
        }
        lock(&self.output).vt.screen_mut().set_size(rows, cols);
        let mut info = lock(&self.info);
        info.cols = cols;
        info.rows = rows;
        drop(info);
        self.publish();
        Ok(())
    }

    pub async fn foreground(&self) -> Option<String> {
        let process = lock(&self.process).clone()?;
        tokio::task::spawn_blocking(move || process.foreground())
            .await
            .ok()
            .flatten()
            .map(|job| job.name.unwrap_or_else(|| format!("process {}", job.pid)))
    }

    pub async fn theme(&self, theme: Theme) -> Result<()> {
        let notify = {
            let mut output = lock(&self.output);
            let events = output.vt.callbacks_mut();
            let notify = events.report_theme && events.theme.dark != theme.dark;
            events.theme = theme;
            notify
        };
        if notify {
            self.send(color_scheme(theme).as_bytes().to_vec()).await?;
        }
        Ok(())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(process) = lock(&self.process).take() {
            process.close();
        }
        self.publish();
    }

    pub async fn read(&self, args: &Value, ctx: &Context) -> Result<Value> {
        let mut changes = self.subscribe();
        let from = args["from"].as_u64().unwrap_or_else(|| self.offset());
        let wait = &args["wait"];
        let until = wait["until"].as_str();
        let regex = if until == Some("match") {
            let pattern = wait["pattern"].as_str().unwrap_or_default();
            Some(
                regex::RegexBuilder::new(pattern)
                    .multi_line(true)
                    .case_insensitive(wait["ignoreCase"].as_bool().unwrap_or(false))
                    .build()
                    .map_err(invalid)?,
            )
        } else {
            None
        };
        let deadline =
            Instant::now() + Duration::from_millis(wait["timeoutMs"].as_u64().unwrap_or(10_000));
        let quiet = Duration::from_millis(wait["quietMs"].as_u64().unwrap_or(500));
        let waited = loop {
            let Some(until) = until else { break None };
            changes.borrow_and_update();
            if self.closed.load(Ordering::Acquire) {
                break Some("closed");
            }
            if lock(&self.info).exited.is_some() {
                break Some("exit");
            }
            let elapsed = {
                let output = lock(&self.output);
                if let Some(regex) = &regex
                    && regex.is_match(&plain(&String::from_utf8_lossy(
                        &output
                            .since(from.max(output.end.saturating_sub(MATCH_WINDOW)), RING)
                            .0,
                    )))
                {
                    break Some("match");
                }
                output.latest.elapsed()
            };
            if until == "quiet" && elapsed >= quiet {
                break Some("quiet");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Some("timeout");
            }
            let sleep = if until == "quiet" {
                remaining.min(quiet.saturating_sub(elapsed))
            } else {
                remaining
            };
            ctx.await_with(tokio::time::timeout(sleep, changes.changed()))
                .await
                .map_err(invalid)?
                .ok();
        };
        let (bytes, skipped, to) = {
            let mut output = lock(&self.output);
            let (bytes, skipped) = output.since(from, RING);
            output.cursor = output.end;
            (bytes, skipped, output.end)
        };
        let text = plain(&String::from_utf8_lossy(&bytes));
        let (text, truncated) = tail(&text, TEXT_LIMIT);
        let mut value = json!({"terminal":*lock(&self.info),"from":from,"to":to,"text":text,"truncated":truncated,"skipped":skipped});
        if let Some(waited) = waited {
            value["waited"] = json!(waited);
        }
        if lock(&self.output).vt.screen().alternate_screen() && !self.closed.load(Ordering::Acquire)
        {
            value["screen"] = json!(self.capture(0, false));
        }
        if let Some(foreground) = self.foreground().await {
            value["foreground"] = json!(foreground);
        }
        Ok(value)
    }

    pub fn capture(&self, lines: usize, escaped: bool) -> String {
        let mut output = lock(&self.output);
        let screen = output.vt.screen_mut();
        let (_, cols) = screen.size();
        let mut captured = Vec::new();
        screen.set_scrollback(lines.min(SCROLLBACK));
        while screen.scrollback() > 0 {
            captured.push(screen_row(screen, cols, escaped));
            screen.set_scrollback(screen.scrollback() - 1);
        }
        if escaped {
            captured.extend(
                screen
                    .rows_formatted(0, cols)
                    .map(|row| String::from_utf8_lossy(&row).into_owned()),
            );
        } else {
            captured.extend(screen.rows(0, cols));
        }
        while captured.last().is_some_and(|line| plain(line).is_empty()) {
            captured.pop();
        }
        captured.join("\n")
    }
}

pub(super) struct Service {
    pub terminals: Mutex<Vec<Arc<Terminal>>>,
    pub changes: watch::Sender<u64>,
    host: pi_rs_chord::host::WeakHost,
    catalog_watch: OnceLock<pi_rs_chord::host::Held>,
    calls: AsyncMutex<VecDeque<MemoizedCall>>,
}

type MemoizedCall = ((String, String, String), Value);

impl Service {
    pub(super) fn new(host: pi_rs_chord::host::WeakHost) -> Self {
        Self {
            host,
            catalog_watch: OnceLock::new(),
            terminals: Mutex::default(),
            changes: watch::channel(0).0,
            calls: AsyncMutex::default(),
        }
    }

    pub fn list(&self, session: &str) -> Vec<Arc<Terminal>> {
        lock(&self.terminals)
            .iter()
            .filter(|terminal| lock(&terminal.info).session_id == session)
            .cloned()
            .collect()
    }

    pub fn get(&self, session: &str, target: &str) -> Result<Arc<Terminal>> {
        let terminals = self.list(session);
        let name = target.strip_prefix(':').unwrap_or(target);
        let number = name
            .as_bytes()
            .first()
            .filter(|byte| (b'1'..=b'9').contains(byte))
            .and_then(|_| name.parse::<usize>().ok());
        terminals
            .iter()
            .find(|terminal| lock(&terminal.info).id == target)
            .or_else(|| {
                terminals
                    .iter()
                    .find(|terminal| Some(lock(&terminal.info).index) == number)
            })
            .or_else(|| {
                terminals
                    .iter()
                    .find(|terminal| lock(&terminal.info).name.as_deref() == Some(name))
            })
            .cloned()
            .ok_or_else(|| invalid(format!("can't find window: {target}")))
    }

    fn open_process(&self, env: Option<&str>, mut options: TerminalOptions) -> Result<Process> {
        if let Some(env) = env {
            let host = self.host.upgrade().ok_or(Error::Closed)?;
            let provider = crate::api::v1::server::service_of::<Arc<dyn EnvironmentProvider>>(
                &host,
                super::ENVIRONMENT_PROVIDER_SERVICE,
            )?;
            match provider.open(env).map_err(invalid)? {
                Machine::Remote {
                    connection,
                    shell_path,
                    ..
                } => {
                    if let Some(shell_path) = shell_path {
                        options
                            .env
                            .get_or_insert_with(Default::default)
                            .insert("SHELL".into(), json!(shell_path));
                    }
                    return open_terminal(connection, options)
                        .map(Process::Remote)
                        .map_err(invalid);
                }
                Machine::Local(Some(connection)) => {
                    return open_terminal(connection, options)
                        .map(Process::Remote)
                        .map_err(invalid);
                }
                Machine::Local(None) => {}
            }
        }
        #[cfg(unix)]
        {
            pi_rs_env::daemon::LocalTerminal::open(options)
                .map(Process::Local)
                .map_err(invalid)
        }
        #[cfg(not(unix))]
        {
            let _ = options;
            Err(invalid("Local terminals are unavailable on this platform"))
        }
    }

    pub async fn open(
        self: &Arc<Self>,
        session: String,
        location: Location,
        size: (u16, u16),
        name: Option<String>,
        agent: bool,
    ) -> Result<Arc<Terminal>> {
        // Keep the catalog subscription alive after the last panel or worker detaches.
        if self.catalog_watch.get().is_none() {
            let host = self.host.upgrade().ok_or(Error::Closed)?;
            let contract = Arc::new(crate::api::v1::p2_contracts::lookup("p2.terminal-service")?);
            let held = host
                .get(contract, json!({}).into(), &Context::background())
                .await?;
            let _ = self.catalog_watch.set(held);
        }
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let (cols, rows) = size;
            let terminal = Arc::new(Terminal {
                info: Mutex::new(Info {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session.clone(),
                    index: 0,
                    title: String::new(),
                    name,
                    agent: agent.then_some(true),
                    location: location.clone(),
                    cols,
                    rows,
                    exited: None,
                    created_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                }),
                output: Mutex::new(Output {
                    bytes: VecDeque::new(),
                    end: 0,
                    cursor: 0,
                    latest: Instant::now(),
                    vt: vt100::Parser::new_with_callbacks(
                        rows,
                        cols,
                        SCROLLBACK,
                        VtEvents {
                            theme: Theme::default(),
                            title: None,
                            replies: Vec::new(),
                            report_theme: false,
                        },
                    ),
                }),
                process: Mutex::new(None),
                closed: AtomicBool::new(false),
                changed: watch::channel(0).0,
            });
            let output = Arc::downgrade(&terminal);
            let changed = service.changes.clone();
            let runtime = tokio::runtime::Handle::current();
            let process = Arc::new(service.open_process(
                location.env.as_deref(),
                TerminalOptions {
                    cwd: location.cwd,
                    argv: None,
                    env: Some(serde_json::Map::from_iter([
                        ("TERM".into(), json!("xterm-256color")),
                        ("COLORTERM".into(), json!("truecolor")),
                        ("COLORFGBG".into(), json!("15;0")),
                    ])),
                    inherit_env: None,
                    cols,
                    rows,
                    on_data: Arc::new(move |bytes| {
                        if let Some(terminal) = output.upgrade() {
                            let (title, reply) = {
                                let mut output = lock(&terminal.output);
                                output.append(bytes);
                                let events = output.vt.callbacks_mut();
                                (events.title.take(), std::mem::take(&mut events.replies))
                            };
                            if let Some(title) = title {
                                lock(&terminal.info).title = title;
                                changed.send_modify(|version| *version = version.wrapping_add(1));
                            }
                            if !reply.is_empty() {
                                let terminal = terminal.clone();
                                runtime.spawn(async move {
                                    let _ = terminal.send(reply).await;
                                });
                            }
                            terminal.publish();
                        }
                    }),
                },
            )?);
            *lock(&terminal.process) = Some(process.clone());
            {
                let mut terminals = lock(&service.terminals);
                let mut index = 1;
                while terminals.iter().any(|terminal| {
                    let info = lock(&terminal.info);
                    info.session_id == session && info.index == index
                }) {
                    index += 1;
                }
                lock(&terminal.info).index = index;
                terminals.push(terminal.clone());
            }
            let ended = Arc::downgrade(&terminal);
            let changed = service.changes.clone();
            std::thread::spawn(move || {
                let exit = process.wait();
                if let Some(terminal) = ended.upgrade() {
                    lock(&terminal.process).take();
                    lock(&terminal.info).exited = Some(match exit {
                        Ok(code) => json!({"code":code}),
                        Err(error) => json!({"error":error.to_string()}),
                    });
                    terminal.publish();
                    changed.send_modify(|version| *version = version.wrapping_add(1));
                }
            });
            service
                .changes
                .send_modify(|version| *version = version.wrapping_add(1));
            Ok(terminal)
        })
        .await
        .map_err(invalid)?
    }

    pub fn remove(&self, terminal: &Arc<Terminal>) -> String {
        lock(&self.terminals).retain(|item| !Arc::ptr_eq(item, terminal));
        terminal.close();
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
        lock(&terminal.info).id.clone()
    }

    pub fn close_session(&self, session: &str) {
        for terminal in self.list(session) {
            self.remove(&terminal);
        }
    }

    pub fn close_all(&self) {
        for terminal in std::mem::take(&mut *lock(&self.terminals)) {
            terminal.close();
        }
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub async fn agent(
        self: &Arc<Self>,
        session: String,
        method: &str,
        args: Value,
        ctx: &Context,
    ) -> Result<Value> {
        if method == "list" {
            let mut windows = Vec::new();
            for terminal in self.list(&session) {
                let mut window = json!({"terminal":*lock(&terminal.info)});
                if let Some(job) = terminal.foreground().await {
                    window["foreground"] = json!(job);
                }
                windows.push(window);
            }
            return Ok(json!({"windows":windows}));
        }
        let target = args["target"].as_str().unwrap_or_default();
        let mut memo = if args.get("key").is_some() {
            Some(ctx.await_with(self.calls.lock()).await.map_err(invalid)?)
        } else {
            None
        };
        let key = (
            session.clone(),
            method.to_owned(),
            args["key"].as_str().unwrap_or_default().to_owned(),
        );
        if let Some(value) = memo
            .as_ref()
            .and_then(|memo| memo.iter().find(|(entry, _)| *entry == key))
            .map(|(_, value)| value)
        {
            return Ok(value.clone());
        }
        let result = if method == "open" {
            let running = self
                .list(&session)
                .into_iter()
                .filter(|terminal| {
                    let info = lock(&terminal.info);
                    info.agent == Some(true) && info.exited.is_none()
                })
                .count();
            if running >= MAX_AGENT_TERMINALS {
                return Err(invalid(format!(
                    "the agent has {MAX_AGENT_TERMINALS} terminals running in this session already: close one with kill-window"
                )));
            }
            let location = serde_json::from_value(args["location"].clone())?;
            let name = args["name"].as_str().map(str::to_owned);
            let terminal = self.open(session, location, (120, 30), name, true).await?;
            if let Some(command) = args["command"]
                .as_str()
                .filter(|command| !command.is_empty())
            {
                let mut changes = terminal.subscribe();
                // Let the login shell render its first prompt before typing the command.
                let _ = tokio::time::timeout(
                    Duration::from_millis(250),
                    changes.wait_for(|_| lock(&terminal.output).end > 0),
                )
                .await;
                if let Err(error) = terminal.send(format!("{command}\r").into_bytes()).await {
                    self.remove(&terminal);
                    return Err(error);
                }
            }
            json!(*lock(&terminal.info))
        } else {
            let terminal = self.get(&session, target)?;
            match method {
                "send" => {
                    {
                        let info = lock(&terminal.info);
                        if info.exited.is_some() {
                            return Err(invalid(format!("window {} has exited", info.index)));
                        }
                    }
                    let keys: Vec<String> = serde_json::from_value(args["keys"].clone())?;
                    let literal = args["literal"] == true;
                    let application = lock(&terminal.output).vt.screen().application_cursor();
                    let input = keys
                        .iter()
                        .flat_map(|key| key_bytes(key, literal, application))
                        .collect();
                    terminal.send(input).await?;
                    json!(*lock(&terminal.info))
                }
                "cursor" => json!({"terminal":*lock(&terminal.info),"offset":terminal.offset()}),
                "read" => terminal.read(&args, ctx).await?,
                "capture" => {
                    let text = terminal.capture(
                        args["scrollback"].as_u64().unwrap_or(0) as usize,
                        args["escapes"] == true,
                    );
                    json!({"terminal":*lock(&terminal.info),"text":text})
                }
                "close" => json!({"terminalId":self.remove(&terminal)}),
                _ => return Err(invalid("Unknown terminal method")),
            }
        };
        if let Some(memo) = &mut memo {
            if memo.len() == 1024 {
                memo.pop_front();
            }
            memo.push_back((key, result.clone()));
        }
        Ok(result)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn screen_row(screen: &vt100::Screen, cols: u16, escaped: bool) -> String {
    if escaped {
        screen
            .rows_formatted(0, cols)
            .next()
            .map(|row| String::from_utf8_lossy(&row).into_owned())
            .unwrap_or_default()
    } else {
        screen.rows(0, cols).next().unwrap_or_default()
    }
}

pub(super) fn tail(text: &str, limit: usize) -> (&str, usize) {
    let omitted = text.encode_utf16().count().saturating_sub(limit);
    let mut removed = 0;
    let boundary = text
        .char_indices()
        .find(|(_, c)| {
            if removed >= omitted {
                return true;
            }
            removed += c.len_utf16();
            false
        })
        .map_or(text.len(), |(index, _)| index);
    (&text[boundary..], removed)
}

fn key_bytes(key: &str, literal: bool, application_cursor: bool) -> Vec<u8> {
    if literal {
        return key.as_bytes().to_vec();
    }
    let mut name = key;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    while let Some((prefix, rest)) = name.split_once('-').filter(|(_, rest)| !rest.is_empty()) {
        match prefix {
            "C" => ctrl = true,
            "M" => alt = true,
            "S" => shift = true,
            _ => break,
        }
        name = rest;
    }
    if let Some(rest) = name.strip_prefix('^').filter(|rest| rest.len() == 1) {
        ctrl = true;
        name = rest;
    }
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let cursor = match name {
        "Up" => Some('A'),
        "Down" => Some('B'),
        "Right" => Some('C'),
        "Left" => Some('D'),
        "Home" => Some('H'),
        "End" => Some('F'),
        _ => None,
    };
    if let Some(code) = cursor {
        return if modifier > 1 {
            format!("\x1b[1;{modifier}{code}")
        } else if application_cursor {
            format!("\x1bO{code}")
        } else {
            format!("\x1b[{code}")
        }
        .into_bytes();
    }
    let keypad = match name {
        "IC" | "Insert" => Some(2),
        "DC" | "Delete" => Some(3),
        "PageUp" | "PgUp" | "PPage" => Some(5),
        "PageDown" | "PgDn" | "NPage" => Some(6),
        "F5" => Some(15),
        "F6" => Some(17),
        "F7" => Some(18),
        "F8" => Some(19),
        "F9" => Some(20),
        "F10" => Some(21),
        "F11" => Some(23),
        "F12" => Some(24),
        _ => None,
    };
    if let Some(code) = keypad {
        return if modifier > 1 {
            format!("\x1b[{code};{modifier}~")
        } else {
            format!("\x1b[{code}~")
        }
        .into_bytes();
    }
    if let Some(code) = match name {
        "F1" => Some('P'),
        "F2" => Some('Q'),
        "F3" => Some('R'),
        "F4" => Some('S'),
        _ => None,
    } {
        return if modifier > 1 {
            format!("\x1b[1;{modifier}{code}")
        } else {
            format!("\x1bO{code}")
        }
        .into_bytes();
    }
    let known: &[u8] = match name {
        "Enter" => b"\r",
        "Tab" if shift => b"\x1b[Z",
        "BTab" => b"\x1b[Z",
        "Tab" => b"\t",
        "Escape" => b"\x1b",
        "BSpace" => b"\x7f",
        "Space" if ctrl => b"\0",
        "Space" => b" ",
        _ if name != key && name.chars().count() == 1 => {
            let mut bytes = if ctrl && name.len() == 1 {
                let byte = name.as_bytes()[0].to_ascii_uppercase();
                match byte {
                    b'@'..=b'_' => vec![byte & 31],
                    b'?' => vec![127],
                    _ => name.as_bytes().to_vec(),
                }
            } else if shift {
                name.to_uppercase().into_bytes()
            } else {
                name.as_bytes().to_vec()
            };
            if alt {
                bytes.insert(0, 27);
            }
            return bytes;
        }
        _ => return key.as_bytes().to_vec(),
    };
    if alt {
        [b"\x1b", known].concat()
    } else {
        known.to_vec()
    }
}

fn plain(input: &str) -> String {
    let mut text = String::new();
    let mut row = Vec::new();
    let mut column = 0_usize;
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\n' => {
                let line: String = row.iter().collect();
                text.push_str(line.trim_end());
                text.push('\n');
                row.clear();
                column = 0;
            }
            '\r' => column = 0,
            '\u{8}' => column = column.saturating_sub(1),
            '\u{1b}' | '\u{9b}' | '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                let kind = match character {
                    '\u{9b}' => Some('['),
                    '\u{9d}' => Some(']'),
                    '\u{90}' => Some('P'),
                    '\u{98}' => Some('X'),
                    '\u{9e}' => Some('^'),
                    '\u{9f}' => Some('_'),
                    _ => chars.next(),
                };
                match kind {
                    Some('[') => {
                        let mut parameters = String::new();
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                if c == 'K' && character == '\u{1b}' {
                                    match parameters.as_str() {
                                        "" | "0" => row.truncate(column),
                                        "1" => row.iter_mut().take(column).for_each(|c| *c = ' '),
                                        "2" => {
                                            row.clear();
                                            row.resize(column, ' ');
                                        }
                                        _ => {}
                                    }
                                }
                                break;
                            }
                            parameters.push(c);
                        }
                    }
                    Some(kind @ (']' | 'P' | 'X' | '^' | '_')) => {
                        while let Some(c) = chars.next() {
                            if c == '\u{9c}'
                                || (kind == ']' && c == '\u{7}')
                                || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    Some(c) if (' '..='/').contains(&c) => {
                        for c in chars.by_ref() {
                            if !(' '..='/').contains(&c) {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            c if !c.is_control() || c == '\t' => {
                if column < row.len() {
                    row[column] = c;
                } else {
                    row.push(c);
                }
                column += 1;
            }
            _ => {}
        }
    }
    text.extend(row);
    text
}
