//! Chord WebSocket link framing, the browser reconnect loop, and the p2-mac host bridge
//! (upstream `packages/chord/src/link/{websocket,binary}.ts`, `web/reconnect.ts`,
//! `web/theme.ts`, `web/shell-bridge.ts`).

// ---------------------------------------------------------------------------
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod replica;

// Binary link framing (the primitives of Chord's `binary.ts` codec).
// ---------------------------------------------------------------------------

/// The kind byte's flag for an origin (upstream `ORIGIN`).
pub const ORIGIN: u8 = 0x80;
/// The kind byte's flag for a trace carrier (`TRACE`).
pub const TRACE: u8 = 0x40;
/// The kind byte's third flag: `call.old` / an event ack (`OLD`).
pub const OLD: u8 = 0x20;
/// A payload part's deflated bit (its length prefix's low bit, upstream `DEFLATED`).
pub const DEFLATED: u8 = 0x01;

/// Encodes an unsigned integer as a base-128 varint (upstream `Writer.varint`).
pub fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value % 0x80) as u8) | 0x80);
        value /= 0x80;
    }
    out.push(value as u8);
}

/// Decodes a base-128 varint, returning the value and how many bytes it took.
/// A varint longer than 10 bytes is rejected as malformed.
pub fn decode_varint(bytes: &[u8]) -> Result<(u64, usize), FrameError> {
    let mut value = 0u64;
    for (index, byte) in bytes.iter().take(10).enumerate() {
        value = value
            .checked_add(
                ((*byte & 0x7f) as u64)
                    .checked_shl(7 * index as u32)
                    .ok_or(FrameError::VarintOverflow)?,
            )
            .ok_or(FrameError::VarintOverflow)?;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(FrameError::VarintOverflow)
}

/// A decoded frame header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// The base kind byte (without the flag bits).
    pub kind: u8,
    pub origin: Option<u64>,
    pub trace: Option<String>,
    pub old: bool,
}

/// An error decoding a binary frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    VarintOverflow,
    Malformed,
}

/// Encodes a frame header into its kind byte and flagged fields.
pub fn encode_header(header: &FrameHeader) -> Vec<u8> {
    let mut out = vec![header.kind & !(ORIGIN | TRACE | OLD)];
    if header.origin.is_some() {
        out[0] |= ORIGIN;
    }
    if header.trace.is_some() {
        out[0] |= TRACE;
    }
    if header.old {
        out[0] |= OLD;
    }
    if let Some(origin) = header.origin {
        encode_varint(origin, &mut out);
    }
    if let Some(trace) = &header.trace {
        encode_string(trace, &mut out);
    }
    out
}

/// Decodes a frame header from a binary frame (one WebSocket message).
pub fn decode_header(bytes: &[u8]) -> Result<FrameHeader, FrameError> {
    let first = *bytes.first().ok_or(FrameError::Empty)?;
    let kind = first & !(ORIGIN | TRACE | OLD);
    let mut offset = 1;
    let origin = if first & ORIGIN != 0 {
        let (value, used) = decode_varint(&bytes[offset..])?;
        offset += used;
        Some(value)
    } else {
        None
    };
    let trace = if first & TRACE != 0 {
        let (text, _) = decode_string(&bytes[offset..])?;
        Some(text)
    } else {
        None
    };
    Ok(FrameHeader {
        kind,
        origin,
        trace,
        old: first & OLD != 0,
    })
}

/// Encodes a length-prefixed UTF-8 string.
pub fn encode_string(text: &str, out: &mut Vec<u8>) {
    encode_varint(text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

/// Decodes a length-prefixed UTF-8 string (strict UTF-8).
pub fn decode_string(bytes: &[u8]) -> Result<(String, usize), FrameError> {
    let (length, used) = decode_varint(bytes)?;
    let length = usize::try_from(length).map_err(|_| FrameError::Malformed)?;
    let end = used.checked_add(length).ok_or(FrameError::Malformed)?;
    let slice = bytes.get(used..end).ok_or(FrameError::Malformed)?;
    Ok((
        std::str::from_utf8(slice)
            .map_err(|_| FrameError::Malformed)?
            .to_owned(),
        end,
    ))
}

/// Encodes a part: its length doubled, with the low bit set when deflated, then its bytes.
pub fn encode_part(bytes: &[u8], deflated: bool) -> Vec<u8> {
    let mut out = vec![];
    encode_varint((bytes.len() as u64) * 2 + u64::from(deflated), &mut out);
    out.extend_from_slice(bytes);
    out
}

/// Decodes a part's bytes and deflated flag.
pub fn decode_part(bytes: &[u8]) -> Result<(Vec<u8>, bool), FrameError> {
    let (prefix, used) = decode_varint(bytes)?;
    let deflated = prefix & DEFLATED as u64 != 0;
    let length = usize::try_from(prefix / 2).map_err(|_| FrameError::Malformed)?;
    let end = used.checked_add(length).ok_or(FrameError::Malformed)?;
    Ok((
        bytes.get(used..end).ok_or(FrameError::Malformed)?.to_vec(),
        deflated,
    ))
}

// ---------------------------------------------------------------------------
// Reconnect state machine (upstream `web/reconnect.ts`).
// ---------------------------------------------------------------------------

pub const RETRY_INITIAL_MS: u64 = 250;
pub const RETRY_MAX_MS: u64 = 5000;
pub const STABLE_MS: u64 = 10_000;
pub const CONNECT_TIMEOUT_MS: u64 = 20_000;
pub const STALE_MS: u64 = 25_000;
pub const WAKE_GRACE_MS: u64 = 1000;

/// The backoff before attempt `failures` (0-based), capped at `RETRY_MAX_MS`.
pub fn backoff(failures: u32) -> u64 {
    RETRY_INITIAL_MS
        .saturating_mul(1u64 << failures.min(20))
        .min(RETRY_MAX_MS)
}

/// The delay to wait before reconnecting.
pub fn retry_delay(failures: u32, now: bool, online: bool) -> u64 {
    if now {
        0
    } else if online {
        backoff(failures)
    } else {
        RETRY_MAX_MS
    }
}

/// Whether a connection that stayed open `open_ms` resets the failure counter.
pub fn is_stable(open_ms: u64) -> bool {
    open_ms >= STABLE_MS
}

/// An action the host executes for the reconnect loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectAction {
    /// Open a new socket.
    Open,
    /// Close the current socket with a reason.
    Close(String),
    /// Report connection state to the UI.
    Connected(bool),
    /// Retry after this many milliseconds.
    Retry(u64),
    /// Log a line.
    Log(String),
}

/// The portable state of the reconnect loop. Timers and socket I/O are host concerns;
/// the state emits actions the host performs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconnectState {
    failures: u32,
    opened_at: Option<u64>,
    started_at: Option<u64>,
    last_message: u64,
    stopped: bool,
    /// A retry is already scheduled (a wake should only hasten it).
    retry_pending: bool,
    /// Deciding whether to retry (`canRetry` is asynchronous).
    deciding: bool,
    woke_while_deciding: bool,
}

impl ReconnectState {
    /// Starts the loop at `now`, emitting an `Open`.
    pub fn start(&mut self, now: u64) -> Vec<ReconnectAction> {
        self.started_at = Some(now);
        self.last_message = now;
        vec![ReconnectAction::Open]
    }

    /// The socket opened at `now`; cancels the connect timeout.
    pub fn opened(&mut self, now: u64) -> Vec<ReconnectAction> {
        if self.opened_at.is_some() || self.stopped {
            return vec![];
        }
        self.opened_at = Some(now);
        self.last_message = now;
        vec![ReconnectAction::Connected(true)]
    }

    /// A message arrived at `now` (the server's 15-second ping counts).
    pub fn received(&mut self, now: u64) {
        self.last_message = now;
    }

    /// The connect timeout fired: give up on a socket that never opened.
    pub fn connect_timeout(&self) -> Option<ReconnectAction> {
        if self.opened_at.is_none() && !self.stopped {
            Some(ReconnectAction::Close(format!(
                "not open after {} s",
                CONNECT_TIMEOUT_MS as f64 / 1000.0
            )))
        } else {
            None
        }
    }

    /// The link closed at `now` with `reason`; decides whether and how to retry.
    pub fn closed(&mut self, now: u64, reason: &str) -> Vec<ReconnectAction> {
        if self.stopped || self.started_at.is_none() {
            return vec![];
        }
        self.started_at = None;
        let mut actions = vec![ReconnectAction::Connected(false)];
        let opened = if self.opened_at.is_none() {
            "before opening"
        } else {
            "after opening"
        };
        actions.push(ReconnectAction::Log(format!(
            "Socket closed {opened}: {reason}"
        )));
        if let Some(opened_at) = self.opened_at
            && is_stable(now.saturating_sub(opened_at))
        {
            self.failures = 0;
        }
        self.opened_at = None;
        self.deciding = true;
        self.woke_while_deciding = false;
        actions
    }

    /// Resolves `canRetry`: schedules the next attempt (`now` when a wake arrived meanwhile).
    pub fn retry_decided(&mut self, can_retry: bool, online: bool) -> Vec<ReconnectAction> {
        if self.stopped {
            return vec![];
        }
        let now = self.woke_while_deciding;
        self.deciding = false;
        if !can_retry {
            return vec![];
        }
        let delay = retry_delay(self.failures, now, online);
        self.failures = self.failures.saturating_add(1);
        self.retry_pending = true;
        vec![ReconnectAction::Retry(delay)]
    }

    /// A wake (online, visible, network change): hasten a pending retry, decide immediately
    /// while deciding, else check for staleness.
    pub fn wake(&mut self, now: u64) -> Vec<ReconnectAction> {
        if self.stopped {
            return vec![];
        }
        if self.retry_pending {
            self.retry_pending = false;
            return vec![ReconnectAction::Open];
        }
        if self.deciding {
            self.woke_while_deciding = true;
            return vec![];
        }
        if self.opened_at.is_none() {
            return vec![]; // still connecting: its own timeout applies
        }
        let silent = now.saturating_sub(self.last_message);
        if silent > STALE_MS {
            vec![ReconnectAction::Close(format!(
                "no message from the server for {} s",
                silent as f64 / 1000.0
            ))]
        } else {
            vec![]
        }
    }

    /// Stops the loop: closes the current link, cancels any retry.
    pub fn stop(&mut self) -> Vec<ReconnectAction> {
        if self.stopped {
            return vec![];
        }
        self.stopped = true;
        self.retry_pending = false;
        vec![ReconnectAction::Close("stopped".to_owned())]
    }
}

// ---------------------------------------------------------------------------
// p2-mac host bridge (`window.webkit.messageHandlers.piHost` / `pi-host-theme`).
// ---------------------------------------------------------------------------

/// `WKScriptMessageHandlerWithReply` name registered by `pi-rs-p2-mac`.
pub const PI_HOST_HANDLER: &str = "piHost";
/// Optional extended native shell handler name (`title`, `badge`, `appearance`).
pub const PI_SHELL_HANDLER: &str = "piShell";
/// CustomEvent dispatched by `pi-rs-p2-mac` when host appearance changes.
pub const HOST_THEME_EVENT: &str = "pi-host-theme";
/// CustomEvent dispatched by `pi-rs-p2-mac` when a native menu item is triggered.
pub const HOST_MENU_EVENT: &str = "pi-host-menu";
/// `localStorage` key for browser theme preference when not hosted in `p2-mac`.
pub const THEME_STORAGE_KEY: &str = "p2-theme";

/// Resolved concrete color scheme applied to `<html data-theme="..." data-color-scheme="...">`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedTheme {
    Light,
    Dark,
}

impl ResolvedTheme {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// Attributes synchronized onto `document.documentElement` (`<html>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtmlThemeAttributes {
    pub data_theme: &'static str,
    pub data_color_scheme: &'static str,
}

/// A theme preference (upstream `ThemePreference`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Resolves `System` against OS `prefers-color-scheme: dark`.
    pub const fn resolve(self, system_prefers_dark: bool) -> ResolvedTheme {
        match self {
            Self::System => {
                if system_prefers_dark {
                    ResolvedTheme::Dark
                } else {
                    ResolvedTheme::Light
                }
            }
            Self::Light => ResolvedTheme::Light,
            Self::Dark => ResolvedTheme::Dark,
        }
    }

    /// Returns synchronized `data-theme` and `data-color-scheme` attributes for `<html>`.
    pub const fn html_attributes(self, system_prefers_dark: bool) -> HtmlThemeAttributes {
        let resolved = self.resolve(system_prefers_dark).as_str();
        HtmlThemeAttributes {
            data_theme: resolved,
            data_color_scheme: resolved,
        }
    }

    /// Cycles `System -> Light -> Dark -> System` for the `.theme-toggle` control.
    pub const fn cycle(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    /// Human-readable label for the `.theme-toggle` button title.
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "Theme: System",
            Self::Light => "Theme: Light",
            Self::Dark => "Theme: Dark",
        }
    }
}

/// A `pi-host-menu` custom event's detail (`{ "item": "settings" }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMenuEvent {
    Settings,
    Unknown(String),
}

impl HostMenuEvent {
    pub fn from_value(value: Option<&serde_json::Value>) -> Option<Self> {
        let item = value?.get("item")?.as_str()?;
        Some(match item {
            "settings" => Self::Settings,
            other => Self::Unknown(other.to_owned()),
        })
    }
}

/// A message the page posts to `window.webkit.messageHandlers.*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellBridgeMessage {
    /// Posted to `window.webkit.messageHandlers.piHost`.
    Host(PiHostMessage),
    /// Posted to optional `window.webkit.messageHandlers.piShell`.
    Title(String),
    /// Posted to optional `window.webkit.messageHandlers.piShell`.
    Badge(u64),
    /// Posted to optional `window.webkit.messageHandlers.piShell`.
    Appearance {
        preference: ThemePreference,
        resolved: ResolvedTheme,
    },
}

impl ShellBridgeMessage {
    pub const fn handler_name(&self) -> &'static str {
        match self {
            Self::Host(_) => PI_HOST_HANDLER,
            Self::Title(_) | Self::Badge(_) | Self::Appearance { .. } => PI_SHELL_HANDLER,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Host(PiHostMessage::Theme) => serde_json::json!({ "type": "theme" }),
            Self::Host(PiHostMessage::SetTheme(pref)) => {
                serde_json::json!({ "type": "set-theme", "preference": pref.as_str() })
            }
            Self::Title(title) => serde_json::json!({ "type": "title", "title": title }),
            Self::Badge(count) => serde_json::json!({ "type": "badge", "count": count }),
            Self::Appearance {
                preference,
                resolved,
            } => serde_json::json!({
                "type": "appearance",
                "preference": preference.as_str(),
                "resolved": resolved.as_str(),
            }),
        }
    }

    pub fn to_json(&self) -> String {
        self.to_value().to_string()
    }
}

/// Pure-Rust coordinator for theme preference, `prefers-color-scheme`, and `piHost` sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeBridgeState {
    preference: ThemePreference,
    system_prefers_dark: bool,
    native_host: bool,
    latest_turn: u64,
    unanswered_set: u32,
}

impl ThemeBridgeState {
    pub const fn new(
        preference: ThemePreference,
        system_prefers_dark: bool,
        native_host: bool,
    ) -> Self {
        Self {
            preference,
            system_prefers_dark,
            native_host,
            latest_turn: 0,
            unanswered_set: 0,
        }
    }

    pub const fn preference(&self) -> ThemePreference {
        self.preference
    }

    pub const fn resolved(&self) -> ResolvedTheme {
        self.preference.resolve(self.system_prefers_dark)
    }

    pub const fn html_attributes(&self) -> HtmlThemeAttributes {
        self.preference.html_attributes(self.system_prefers_dark)
    }

    /// Initiates a `{"type":"theme"}` sync request when running under `piHost`.
    pub fn start_sync(&mut self) -> Option<(u64, PiHostMessage)> {
        if !self.native_host {
            return None;
        }
        self.latest_turn += 1;
        Some((self.latest_turn, PiHostMessage::Theme))
    }

    /// Updates local preference immediately and emits a `set-theme` request if hosted natively.
    pub fn user_select(&mut self, preference: ThemePreference) -> Option<(u64, PiHostMessage)> {
        self.preference = preference;
        if !self.native_host {
            return None;
        }
        self.latest_turn += 1;
        self.unanswered_set = self.unanswered_set.saturating_add(1);
        Some((self.latest_turn, PiHostMessage::SetTheme(preference)))
    }

    /// Applies a `piHost.postMessage(...)` reply if it matches the latest turn.
    pub fn handle_host_answer(
        &mut self,
        answer_turn: u64,
        is_set_reply: bool,
        answer: Option<HostAnswer>,
    ) -> bool {
        if is_set_reply {
            self.unanswered_set = self.unanswered_set.saturating_sub(1);
        }
        if let Some(pref) = applies_host_answer(
            self.latest_turn,
            answer_turn,
            self.native_host,
            answer.map(|a| a.preference),
        ) {
            self.preference = pref;
            return true;
        }
        false
    }

    /// Applies a `pi-host-theme` window event when no `set-theme` call is in flight.
    pub fn handle_host_event(&mut self, detail: Option<&serde_json::Value>) -> bool {
        if let Some(ev) = applies_host_event(self.native_host, self.unanswered_set, detail) {
            self.preference = ev.preference;
            return true;
        }
        false
    }

    /// Updates OS `prefers-color-scheme: dark` state; returns whether resolved theme changed.
    pub fn handle_system_media_change(&mut self, system_prefers_dark: bool) -> bool {
        let before = self.resolved();
        self.system_prefers_dark = system_prefers_dark;
        self.resolved() != before
    }
}

/// Formats the browser/native window title from active session title and busy count.
pub fn format_document_title(session_title: Option<&str>, busy_count: usize) -> String {
    let base = session_title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Pi");
    if busy_count > 0 {
        format!("{base} ({busy_count} running)")
    } else {
        base.to_owned()
    }
}

/// A message the page posts to p2-mac's `piHost` handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiHostMessage {
    /// Ask the app for its current preference.
    Theme,
    /// Tell the app the user picked a preference.
    SetTheme(ThemePreference),
}

impl PiHostMessage {
    /// The JSON body posted to `window.webkit.messageHandlers.piHost.postMessage`.
    pub fn to_json(self) -> String {
        match self {
            Self::Theme => r#"{"type":"theme"}"#.to_owned(),
            Self::SetTheme(preference) => format!(
                "{{\"type\":\"set-theme\",\"preference\":\"{}\"}}",
                preference.as_str()
            ),
        }
    }

    /// Parses a page-to-host message (used by the host-side test double).
    pub fn from_json(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        match value.get("type")?.as_str()? {
            "theme" => Some(Self::Theme),
            "set-theme" => Some(Self::SetTheme(ThemePreference::parse(
                value.get("preference")?.as_str()?,
            )?)),
            _ => None,
        }
    }
}

/// The host's answer to a `theme`/`set-theme` request: `{ preference }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostAnswer {
    pub preference: ThemePreference,
}

impl HostAnswer {
    pub fn from_json(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Self {
            preference: ThemePreference::parse(value.get("preference")?.as_str()?)?,
        })
    }
}

/// A `pi-host-theme` custom event's detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostThemeEvent {
    pub preference: ThemePreference,
}

impl HostThemeEvent {
    /// Parses the event detail, validating the preference (unknown values are ignored).
    pub fn from_value(value: Option<&serde_json::Value>) -> Option<Self> {
        let preference = value?.get("preference")?.as_str()?;
        Some(Self {
            preference: ThemePreference::parse(preference)?,
        })
    }
}

/// Whether the page runs in p2-mac's WKWebView (upstream `nativeHost()`).
pub fn is_native_host(webkit_handler_present: bool) -> bool {
    webkit_handler_present
}

/// Whether a `pi-host-theme` event should be applied: native host, no unanswered
/// `set-theme`, and a valid preference (upstream `hostReported`).
pub fn applies_host_event(
    native: bool,
    unanswered: u32,
    event: Option<&serde_json::Value>,
) -> Option<HostThemeEvent> {
    if !native || unanswered > 0 {
        return None;
    }
    HostThemeEvent::from_value(event)
}

/// An answer counts only when it is the latest request and no unanswered request is newer
/// (upstream `ask`).
pub fn applies_host_answer(
    latest_turn: u64,
    answer_turn: u64,
    native: bool,
    given: Option<ThemePreference>,
) -> Option<ThemePreference> {
    if latest_turn == answer_turn && native {
        given
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 16_383, 1_000_000, u64::MAX] {
            let mut encoded = vec![];
            encode_varint(value, &mut encoded);
            assert_eq!(decode_varint(&encoded).unwrap(), (value, encoded.len()));
        }
        assert_eq!(decode_varint(&[0x80; 11]), Err(FrameError::VarintOverflow));
    }

    #[test]
    fn header_flags_round_trip() {
        let header = FrameHeader {
            kind: 3,
            origin: Some(300),
            trace: Some("abc".into()),
            old: true,
        };
        let encoded = encode_header(&header);
        assert_eq!(decode_header(&encoded).unwrap(), header);
        assert_eq!(decode_header(&[]), Err(FrameError::Empty));
    }

    #[test]
    fn part_length_prefix_and_deflate_flag() {
        let encoded = encode_part(b"hello", true);
        assert_eq!(decode_part(&encoded).unwrap(), (b"hello".to_vec(), true));
        let encoded = encode_part(b"plain", false);
        assert_eq!(decode_part(&encoded).unwrap(), (b"plain".to_vec(), false));
        assert!(decode_part(&[0x80]).is_err());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0), 250);
        assert_eq!(backoff(1), 500);
        assert_eq!(backoff(2), 1000);
        assert_eq!(backoff(20), 5000);
        assert_eq!(retry_delay(1, true, true), 0);
        assert_eq!(retry_delay(1, false, false), 5000);
    }

    #[test]
    fn reconnect_stable_resets_failures() {
        assert!(is_stable(STABLE_MS));
        assert!(!is_stable(STABLE_MS - 1));
        let mut state = ReconnectState::default();
        state.start(0);
        state.opened(10);
        assert_eq!(
            state.closed(20_010, "lost"),
            vec![
                ReconnectAction::Connected(false),
                ReconnectAction::Log("Socket closed after opening: lost".into()),
            ]
        );
        let actions = state.retry_decided(true, true);
        assert_eq!(actions, vec![ReconnectAction::Retry(250)]);
    }

    #[test]
    fn reconnect_connect_timeout_closes_before_open() {
        let mut state = ReconnectState::default();
        state.start(0);
        assert_eq!(
            state.connect_timeout(),
            Some(ReconnectAction::Close("not open after 20 s".into()))
        );
        state.opened(100);
        assert_eq!(state.connect_timeout(), None);
    }

    #[test]
    fn reconnect_wake_hastens_pending_retry() {
        let mut state = ReconnectState::default();
        state.start(0);
        state.opened(10);
        state.closed(20, "x");
        state.retry_decided(true, true);
        assert_eq!(state.wake(21), vec![ReconnectAction::Open]);
    }

    #[test]
    fn reconnect_wake_closes_stale_socket() {
        let mut state = ReconnectState::default();
        state.start(0);
        state.opened(10);
        state.received(100);
        let actions = state.wake(100 + STALE_MS + 1);
        assert!(
            matches!(actions.as_slice(), [ReconnectAction::Close(reason)] if reason.contains("no message"))
        );
    }

    #[test]
    fn host_message_codecs() {
        assert_eq!(PiHostMessage::Theme.to_json(), r#"{"type":"theme"}"#);
        assert_eq!(
            PiHostMessage::from_json(r#"{"type":"set-theme","preference":"dark"}"#),
            Some(PiHostMessage::SetTheme(ThemePreference::Dark))
        );
        assert_eq!(
            PiHostMessage::from_json(r#"{"type":"set-theme","preference":"bad"}"#),
            None
        );
        assert_eq!(
            HostAnswer::from_json(r#"{"preference":"light"}"#).map(|a| a.preference),
            Some(ThemePreference::Light)
        );
    }

    #[test]
    fn host_events_validate_preference_and_unanswered() {
        let detail = serde_json::json!({"preference": "dark"});
        assert!(applies_host_event(true, 0, Some(&detail)).is_some());
        assert!(applies_host_event(false, 0, Some(&detail)).is_none());
        assert!(applies_host_event(true, 1, Some(&detail)).is_none());
        assert!(
            HostThemeEvent::from_value(Some(&serde_json::json!({"preference":"bogus"}))).is_none()
        );
    }

    #[test]
    fn host_answers_only_count_for_latest_turn() {
        assert_eq!(
            applies_host_answer(3, 3, true, Some(ThemePreference::Dark)),
            Some(ThemePreference::Dark)
        );
        assert_eq!(
            applies_host_answer(3, 2, true, Some(ThemePreference::Dark)),
            None
        );
        assert_eq!(
            applies_host_answer(3, 3, false, Some(ThemePreference::Dark)),
            None
        );
    }

    #[test]
    fn native_host_detection() {
        assert!(is_native_host(true));
        assert!(!is_native_host(false));
    }

    #[test]
    fn theme_resolution_syncs_data_theme_and_color_scheme() {
        assert_eq!(
            ThemePreference::Light.html_attributes(true),
            HtmlThemeAttributes {
                data_theme: "light",
                data_color_scheme: "light",
            }
        );
        assert_eq!(
            ThemePreference::Dark.html_attributes(false),
            HtmlThemeAttributes {
                data_theme: "dark",
                data_color_scheme: "dark",
            }
        );
        assert_eq!(
            ThemePreference::System.html_attributes(false),
            HtmlThemeAttributes {
                data_theme: "light",
                data_color_scheme: "light",
            }
        );
        assert_eq!(
            ThemePreference::System.html_attributes(true),
            HtmlThemeAttributes {
                data_theme: "dark",
                data_color_scheme: "dark",
            }
        );
        assert_eq!(ThemePreference::System.cycle(), ThemePreference::Light);
        assert_eq!(ThemePreference::Light.cycle(), ThemePreference::Dark);
        assert_eq!(ThemePreference::Dark.cycle(), ThemePreference::System);
        assert_eq!(ThemePreference::Dark.label(), "Theme: Dark");
    }

    #[test]
    fn shell_bridge_messages_format_exact_handler_and_payloads() {
        let host_msg = ShellBridgeMessage::Host(PiHostMessage::SetTheme(ThemePreference::Dark));
        assert_eq!(host_msg.handler_name(), PI_HOST_HANDLER);
        assert_eq!(
            host_msg.to_value(),
            serde_json::json!({"type": "set-theme", "preference": "dark"})
        );

        let title_msg = ShellBridgeMessage::Title("Pi — Session".into());
        assert_eq!(title_msg.handler_name(), PI_SHELL_HANDLER);
        assert_eq!(
            title_msg.to_value(),
            serde_json::json!({"type": "title", "title": "Pi — Session"})
        );

        let badge_msg = ShellBridgeMessage::Badge(2);
        assert_eq!(badge_msg.handler_name(), PI_SHELL_HANDLER);
        assert_eq!(
            badge_msg.to_value(),
            serde_json::json!({"type": "badge", "count": 2})
        );

        let app_msg = ShellBridgeMessage::Appearance {
            preference: ThemePreference::System,
            resolved: ResolvedTheme::Dark,
        };
        assert_eq!(
            app_msg.to_value(),
            serde_json::json!({"type": "appearance", "preference": "system", "resolved": "dark"})
        );

        assert_eq!(
            HostMenuEvent::from_value(Some(&serde_json::json!({"item": "settings"}))),
            Some(HostMenuEvent::Settings)
        );
        assert_eq!(format_document_title(Some("Demo"), 2), "Demo (2 running)");
        assert_eq!(format_document_title(None, 0), "Pi");
    }

    #[test]
    fn theme_bridge_state_respects_in_flight_set_and_media_changes() {
        let mut bridge = ThemeBridgeState::new(ThemePreference::System, false, true);
        let (t1, msg1) = bridge.start_sync().expect("sync");
        assert_eq!(msg1, PiHostMessage::Theme);

        // User changes theme before initial `theme` reply arrives; stale reply is ignored.
        let (t2, msg2) = bridge
            .user_select(ThemePreference::Dark)
            .expect("set-theme");
        assert_eq!(msg2, PiHostMessage::SetTheme(ThemePreference::Dark));
        assert!(!bridge.handle_host_answer(
            t1,
            false,
            Some(HostAnswer {
                preference: ThemePreference::Light
            })
        ));
        assert_eq!(bridge.preference(), ThemePreference::Dark);

        // Stale pi-host-theme event while set-theme is unanswered is ignored.
        assert!(!bridge.handle_host_event(Some(&serde_json::json!({"preference": "light"}))));
        assert!(bridge.handle_host_answer(
            t2,
            true,
            Some(HostAnswer {
                preference: ThemePreference::Dark
            })
        ));
        assert!(bridge.handle_host_event(Some(&serde_json::json!({"preference": "system"}))));
        assert_eq!(bridge.preference(), ThemePreference::System);
        assert!(bridge.handle_system_media_change(true));
        assert_eq!(bridge.resolved(), ResolvedTheme::Dark);
    }
}
