//! Terminal I/O, keyboard protocol negotiation, and lifecycle.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::keys::set_kitty_protocol_active;
use crate::program_status::{
    PROGRAM_STATUS_QUERY, ProgramStatus, ProgramStatusState, format_program_status,
    is_program_status_reply,
};
use crate::stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
use crate::utils::replace_control_characters;

mod platform;

#[cfg(unix)]
pub(crate) use platform::SuspendSignal;

const PASTE_ON: &str = "\x1b[?2004h";
const PASTE_OFF: &str = "\x1b[?2004l";
const KITTY_QUERY: &str = "\x1b[>7u\x1b[?u";
const KITTY_OFF: &str = "\x1b[<u";
const MODIFY_ON: &str = "\x1b[>4;2m";
const MODIFY_OFF: &str = "\x1b[>4;0m";
const PROGRESS_ON: &str = "\x1b]9;4;3\x07";
const PROGRESS_OFF: &str = "\x1b]9;4;0\x07";
const NEGOTIATION_DELAY: Duration = Duration::from_millis(150);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

pub trait Terminal: Send {
    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn write(&mut self, data: &str);
    fn read_available(&mut self) -> Vec<u8>;
    fn is_tty(&self) -> bool;

    /// Enter terminal-specific input modes. An already active transport needs no setup.
    fn start_session(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Restore any modes owned by this transport.
    fn stop_session(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Set mode resets written around keyboard cleanup on process exit.
    fn set_exit_restore(&mut self, _before: &str, _after: &str) {}

    fn poll_input(&mut self, _timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        let bytes = self.read_available();
        if bytes.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![TerminalEvent::Input(
                String::from_utf8_lossy(&bytes).into_owned(),
            )])
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    KittyFlags(u32),
    DeviceAttributes,
}

pub fn parse_keyboard_protocol_negotiation_sequence(
    data: &str,
) -> Option<KeyboardProtocolNegotiationSequence> {
    let data = data.strip_prefix("\x1b[?")?;
    if let Some(digits) = data.strip_suffix('u')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        let flags = digits.parse().unwrap_or(u32::MAX);
        return Some(KeyboardProtocolNegotiationSequence::KittyFlags(flags));
    }
    let attributes = data.strip_suffix('c')?;
    attributes
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b';')
        .then_some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
}

fn negotiation_prefix(data: &str) -> bool {
    data == "\x1b["
        || data
            .strip_prefix("\x1b[?")
            .is_some_and(|body| body.bytes().all(|b| b.is_ascii_digit() || b == b';'))
}

pub fn is_apple_terminal_session() -> bool {
    cfg!(target_os = "macos") && std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal")
}

pub fn normalize_native_shift_enter_input(
    data: &str,
    detect_shift: bool,
    shift_pressed: bool,
) -> &str {
    if detect_shift && shift_pressed && matches!(data, "\r" | "\x1b\r") {
        "\x1b[13;2u"
    } else {
        data
    }
}

pub fn resolve_escape_timeout_ms(env: impl Fn(&str) -> Option<String>) -> f64 {
    if let Some(ms) = env("PI_TUI_ESC_TIMEOUT").and_then(|value| value.trim().parse::<f64>().ok())
        && ms.is_finite()
        && ms > 0.0
    {
        return ms;
    }
    if ["SSH_CONNECTION", "SSH_TTY"]
        .into_iter()
        .any(|key| env(key).is_some_and(|s| !s.is_empty()))
    {
        100.0
    } else {
        10.0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    Input(String),
    Resize { columns: usize, rows: usize },
}

pub struct ProcessTerminal {
    raw: Option<platform::RawMode>,
    exit: Option<platform::ExitRegistration>,
    exit_before: String,
    exit_after: String,
    started: bool,
    pushed: bool,
    kitty: bool,
    modify: bool,
    pending_da: usize,
    negotiation: String,
    negotiation_deadline: Option<Instant>,
    output_error: Option<io::Error>,
    input: StdinBuffer,
    utf8_tail: Vec<u8>,
    size: (usize, usize),
    progress: Option<Instant>,
    status: Option<ProgramStatus>,
    status_supported: bool,
    status_pending: bool,
    write_log: Option<PathBuf>,
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        let escape_ms = resolve_escape_timeout_ms(|key| std::env::var(key).ok());
        let escape_timeout = Duration::try_from_secs_f64(escape_ms / 1000.0)
            .unwrap_or(Duration::from_secs(24 * 24 * 60 * 60));
        let write_log = std::env::var_os("PI_TUI_WRITE_LOG")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                if path.is_dir() {
                    let time = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
                    path.join(format!("tui-{time}-{}.log", std::process::id()))
                } else {
                    path
                }
            });
        Self {
            raw: None,
            exit: None,
            exit_before: "\x1b[?25h".into(),
            exit_after: String::new(),
            started: false,
            pushed: false,
            kitty: false,
            modify: false,
            pending_da: 0,
            negotiation: String::new(),
            negotiation_deadline: None,
            output_error: None,
            input: StdinBuffer::new(StdinBufferOptions {
                escape_timeout,
                ..Default::default()
            }),
            utf8_tail: Vec::new(),
            size: terminal_size(),
            progress: None,
            status: None,
            status_supported: false,
            status_pending: false,
            write_log,
        }
    }
}

impl ProcessTerminal {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn kitty_protocol_active(&self) -> bool {
        self.kitty
    }
    pub fn modify_other_keys_active(&self) -> bool {
        self.modify
    }

    pub fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        if io::stdin().is_terminal() {
            self.raw = Some(platform::RawMode::enter()?);
        }
        match platform::ExitRegistration::begin(
            self.raw.clone(),
            &self.exit_before,
            &self.exit_after,
        ) {
            Ok(exit) => self.exit = Some(exit),
            Err(error) => {
                if let Some(raw) = self.raw.take() {
                    let _ = raw.restore();
                }
                return Err(error);
            }
        }
        self.started = true;
        self.size = terminal_size();
        self.pushed = true;
        self.update_exit_protocols();
        self.pending_da += 1;
        self.clear_negotiation();
        let status_override = std::env::var("PI_PROGRAM_STATUS").ok();
        self.status_supported = status_override.as_deref() == Some("1");
        self.status_pending = !matches!(status_override.as_deref(), Some("1" | "0"));
        platform::enable_virtual_terminal_input();
        self.send(PASTE_ON);
        let status_query = if self.status_pending {
            PROGRAM_STATUS_QUERY
        } else {
            ""
        };
        self.send(&format!("{KITTY_QUERY}{status_query}\x1b[c"));
        self.send_program_status();
        if let Some(error) = self.take_output_error() {
            let _ = self.stop();
            return Err(error);
        }
        Ok(())
    }

    pub fn stop(&mut self) -> io::Result<()> {
        if !self.started {
            return Ok(());
        }
        if self.progress.take().is_some() {
            self.send(PROGRESS_OFF);
        }
        if self.status_supported && self.status.is_some() {
            self.send(&format_program_status(&ProgramStatus {
                state: ProgramStatusState::Clear,
                ..Default::default()
            }));
        }
        self.status_supported = false;
        self.status_pending = false;
        self.send(PASTE_OFF);
        self.disable_protocols();
        self.input.clear();
        self.utf8_tail.clear();
        self.started = false;
        let raw = self.raw.take().map_or(Ok(()), platform::RawMode::restore);
        self.exit.take();
        self.take_output_error().map_or(raw, Err)
    }

    pub fn restore_on_exit(&mut self, before: &str, after: &str) -> io::Result<()> {
        self.send(before);
        if self.progress.take().is_some() {
            self.send(PROGRESS_OFF);
        }
        self.send(PASTE_OFF);
        self.disable_protocols();
        self.send(after);
        let raw = self.raw.take().map_or(Ok(()), platform::RawMode::restore);
        self.started = false;
        self.exit.take();
        self.take_output_error().map_or(raw, Err)
    }

    fn disable_protocols(&mut self) {
        self.clear_negotiation();
        if self.pushed || self.kitty {
            self.send(KITTY_OFF);
            self.pushed = false;
            self.kitty = false;
            set_kitty_protocol_active(false);
        }
        if self.modify {
            self.send(MODIFY_OFF);
            self.modify = false;
        }
        self.update_exit_protocols();
    }

    fn clear_negotiation(&mut self) {
        self.negotiation.clear();
        self.negotiation_deadline = None;
    }

    fn update_exit_protocols(&self) {
        if let Some(exit) = &self.exit {
            exit.protocols([
                if self.progress.is_some() {
                    PROGRESS_OFF
                } else {
                    ""
                },
                PASTE_OFF,
                if self.pushed || self.kitty {
                    KITTY_OFF
                } else {
                    ""
                },
                if self.modify { MODIFY_OFF } else { "" },
            ]);
        }
    }

    fn consume_negotiation(&mut self, parsed: KeyboardProtocolNegotiationSequence) -> bool {
        match parsed {
            KeyboardProtocolNegotiationSequence::KittyFlags(flags) if flags != 0 => {
                if self.modify {
                    self.send(MODIFY_OFF);
                    self.modify = false;
                }
                self.kitty = true;
                set_kitty_protocol_active(true);
            }
            other => {
                if other == KeyboardProtocolNegotiationSequence::DeviceAttributes {
                    if self.pending_da == 0 {
                        return false;
                    }
                    self.pending_da -= 1;
                    if self.pending_da == 0 {
                        self.status_pending = false;
                    }
                }
                if !self.kitty && !self.modify {
                    self.send(MODIFY_ON);
                    self.modify = true;
                }
            }
        }
        self.update_exit_protocols();
        true
    }

    fn route(&mut self, event: StdinEvent, now: Instant, events: &mut Vec<TerminalEvent>) {
        let mut sequence = match event {
            StdinEvent::Paste(paste) => {
                events.push(TerminalEvent::Input(format!("\x1b[200~{paste}\x1b[201~")));
                return;
            }
            StdinEvent::Data(sequence) => sequence,
        };
        if is_program_status_reply(&sequence) {
            if self.status_pending {
                self.status_pending = false;
                self.status_supported = true;
                self.send_program_status();
            }
            return;
        }
        if !self.negotiation.is_empty() {
            let previous_length = self.negotiation.len();
            self.negotiation.push_str(&sequence);
            if let Some(reply) = parse_keyboard_protocol_negotiation_sequence(&self.negotiation) {
                sequence = std::mem::take(&mut self.negotiation);
                self.negotiation_deadline = None;
                if self.consume_negotiation(reply) {
                    return;
                }
                events.push(TerminalEvent::Input(sequence));
                return;
            }
            if negotiation_prefix(&self.negotiation) {
                self.negotiation_deadline = now.checked_add(NEGOTIATION_DELAY);
                return;
            }
            self.negotiation.truncate(previous_length);
            self.negotiation_deadline = None;
            events.push(TerminalEvent::Input(std::mem::take(&mut self.negotiation)));
        }
        if let Some(reply) = parse_keyboard_protocol_negotiation_sequence(&sequence) {
            if self.consume_negotiation(reply) {
                return;
            }
        } else if negotiation_prefix(&sequence) {
            self.negotiation = sequence;
            self.negotiation_deadline = now.checked_add(NEGOTIATION_DELAY);
            return;
        }
        let detect_shift = cfg!(windows) || is_apple_terminal_session();
        if detect_shift && matches!(sequence.as_str(), "\r" | "\x1b\r") && platform::shift_pressed()
        {
            sequence = "\x1b[13;2u".to_owned();
        }
        events.push(TerminalEvent::Input(sequence));
    }

    /// Poll at most `timeout`, including timeouts for input framing and negotiated protocols.
    pub fn poll_events(&mut self, timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        let now = Instant::now();
        let timeout = [
            self.input.deadline(),
            self.negotiation_deadline,
            self.progress,
        ]
        .into_iter()
        .flatten()
        .map(|when| when.saturating_duration_since(now))
        .fold(timeout, Duration::min);
        let data = platform::read_input(timeout)?;
        let now = Instant::now();
        let mut events = Vec::new();
        for input in self.input.expire(now) {
            self.route(input, now, &mut events);
        }
        if self
            .negotiation_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.negotiation_deadline = None;
            events.push(TerminalEvent::Input(std::mem::take(&mut self.negotiation)));
        }
        if !data.is_empty() {
            let mut bytes = if self.utf8_tail.is_empty() {
                data
            } else {
                let mut bytes = std::mem::take(&mut self.utf8_tail);
                bytes.extend(data);
                bytes
            };
            if let Err(error) = std::str::from_utf8(&bytes)
                && error.error_len().is_none()
            {
                self.utf8_tail
                    .extend_from_slice(&bytes[error.valid_up_to()..]);
                bytes.truncate(error.valid_up_to());
            }
            if !bytes.is_empty() {
                for input in self.input.process(&String::from_utf8_lossy(&bytes), now) {
                    self.route(input, now, &mut events);
                }
            }
        }
        let size = terminal_size();
        if self.size != size {
            self.size = size;
            events.push(TerminalEvent::Resize {
                columns: size.0,
                rows: size.1,
            });
        }
        if self.progress.is_some_and(|deadline| now >= deadline) {
            self.send(PROGRESS_ON);
            self.progress = now.checked_add(PROGRESS_INTERVAL);
        }
        self.take_output_error().map_or(Ok(events), Err)
    }

    pub fn drain_input(&mut self, max: Duration, idle: Duration) -> io::Result<()> {
        self.disable_protocols();
        let end = Instant::now().checked_add(max);
        while let Some(left) = end.and_then(|end| end.checked_duration_since(Instant::now())) {
            if left.is_zero() || idle.is_zero() || platform::read_input(left.min(idle))?.is_empty()
            {
                break;
            }
        }
        self.take_output_error().map_or(Ok(()), Err)
    }

    fn send(&mut self, data: &str) {
        let mut stdout = io::stdout().lock();
        if let Err(error) = stdout
            .write_all(data.as_bytes())
            .and_then(|()| stdout.flush())
            && self.output_error.is_none()
        {
            self.output_error = Some(error);
        }
    }

    pub fn take_output_error(&mut self) -> Option<io::Error> {
        self.output_error.take()
    }

    pub fn move_by(&mut self, lines: isize) {
        if lines > 0 {
            self.send(&format!("\x1b[{lines}B"));
        }
        if lines < 0 {
            self.send(&format!("\x1b[{}A", lines.unsigned_abs()));
        }
    }
    pub fn hide_cursor(&mut self) {
        self.send("\x1b[?25l");
    }
    pub fn show_cursor(&mut self) {
        self.send("\x1b[?25h");
    }
    pub fn clear_line(&mut self) {
        self.send("\x1b[K");
    }
    pub fn clear_from_cursor(&mut self) {
        self.send("\x1b[J");
    }
    pub fn clear_screen(&mut self) {
        self.send("\x1b[2J\x1b[H");
    }
    pub fn set_title(&mut self, title: &str) {
        self.send(&format!("\x1b]0;{}\x07", replace_control_characters(title)));
    }

    pub fn set_program_status(&mut self, status: ProgramStatus) {
        if self.status_supported {
            self.send(&format_program_status(&status));
        }
        self.status = (status.state != ProgramStatusState::Clear).then_some(status);
    }

    fn send_program_status(&mut self) {
        if self.status_supported
            && let Some(status) = &self.status
        {
            self.send(&format_program_status(status));
        }
    }

    pub fn set_progress(&mut self, active: bool) {
        self.send(if active { PROGRESS_ON } else { PROGRESS_OFF });
        if active {
            if self.progress.is_none() {
                self.progress = Instant::now().checked_add(PROGRESS_INTERVAL);
            }
        } else {
            self.progress = None;
        }
        self.update_exit_protocols();
    }
}

impl Drop for ProcessTerminal {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Terminal for ProcessTerminal {
    fn start_session(&mut self) -> io::Result<()> {
        self.start()
    }
    fn stop_session(&mut self) -> io::Result<()> {
        self.stop()
    }
    fn set_exit_restore(&mut self, before: &str, after: &str) {
        before.clone_into(&mut self.exit_before);
        after.clone_into(&mut self.exit_after);
        if let Some(exit) = &self.exit {
            exit.framing(before, after);
        }
    }
    fn poll_input(&mut self, timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        self.poll_events(timeout)
    }
    fn width(&self) -> usize {
        self.size.0
    }
    fn height(&self) -> usize {
        self.size.1
    }
    fn write(&mut self, data: &str) {
        self.send(data);
        if let Some(path) = &self.write_log
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
        {
            let _ = file.write_all(data.as_bytes());
        }
    }
    fn read_available(&mut self) -> Vec<u8> {
        match platform::read_input(Duration::ZERO) {
            Ok(bytes) => bytes,
            Err(error) => {
                if self.output_error.is_none() {
                    self.output_error = Some(error);
                }
                Vec::new()
            }
        }
    }
    fn is_tty(&self) -> bool {
        io::stdin().is_terminal() && io::stdout().is_terminal()
    }
}

fn terminal_size() -> (usize, usize) {
    let (width, height) = platform::dimensions().unwrap_or_default();
    let env = |value: usize, key, default| {
        if value != 0 {
            value
        } else {
            std::env::var(key)
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n != 0)
                .unwrap_or(default)
        }
    };
    (env(width, "COLUMNS", 80), env(height, "LINES", 24))
}
