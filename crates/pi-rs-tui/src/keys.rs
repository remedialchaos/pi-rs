//! Keyboard input handling for terminal applications.
//!
//! Ported from `packages/tui/src/keys.ts`.
//!
//! Supports the legacy terminal sequences and the Kitty keyboard protocol
//! (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>).
//!
//! The upstream keeps the Kitty protocol state and the type of the last parsed
//! event in module-level variables; here they live in atomics.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// The kind of a key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventType {
    /// The key was pressed.
    Press,
    /// The key repeated (auto-repeat).
    Repeat,
    /// The key was released.
    Release,
}

/// The name of a key, as the keybindings use it.
pub type KeyId = String;

/// Whether the Kitty keyboard protocol is active.
static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The type of the last parsed Kitty event (`0` press, `1` repeat, `2` release).
static LAST_EVENT_TYPE: AtomicU8 = AtomicU8::new(0);

const MODIFIER_SHIFT: u32 = 1;
const MODIFIER_ALT: u32 = 2;
const MODIFIER_CTRL: u32 = 4;
const MODIFIER_SUPER: u32 = 8;

/// Caps Lock + Num Lock.
const LOCK_MASK: u32 = 64 + 128;

const CODEPOINT_ESCAPE: i64 = 27;
const CODEPOINT_TAB: i64 = 9;
const CODEPOINT_ENTER: i64 = 13;
const CODEPOINT_SPACE: i64 = 32;
const CODEPOINT_BACKSPACE: i64 = 127;
/// Numpad Enter (Kitty protocol).
const CODEPOINT_KP_ENTER: i64 = 57414;

const ARROW_UP: i64 = -1;
const ARROW_DOWN: i64 = -2;
const ARROW_RIGHT: i64 = -3;
const ARROW_LEFT: i64 = -4;

const FUNCTIONAL_DELETE: i64 = -10;
const FUNCTIONAL_INSERT: i64 = -11;
const FUNCTIONAL_PAGE_UP: i64 = -12;
const FUNCTIONAL_PAGE_DOWN: i64 = -13;
const FUNCTIONAL_HOME: i64 = -14;
const FUNCTIONAL_END: i64 = -15;
const FUNCTIONAL_F1: i64 = -21;
const FUNCTIONAL_F12: i64 = -32;

/// Kitty reserves this Private Use Area range for functional keys (F13-F35,
/// media keys, modifier keys, ...). CSI-u codepoints in it are never text.
const KITTY_FUNCTIONAL_PUA_START: i64 = 57344;
const KITTY_FUNCTIONAL_PUA_END: i64 = 63743;

/// The Kitty functional keys that have a printable or navigational equivalent.
fn kitty_functional_equivalent(codepoint: i64) -> i64 {
    match codepoint {
        57399 => 48, // KP_0 -> 0
        57400 => 49, // KP_1 -> 1
        57401 => 50, // KP_2 -> 2
        57402 => 51, // KP_3 -> 3
        57403 => 52, // KP_4 -> 4
        57404 => 53, // KP_5 -> 5
        57405 => 54, // KP_6 -> 6
        57406 => 55, // KP_7 -> 7
        57407 => 56, // KP_8 -> 8
        57408 => 57, // KP_9 -> 9
        57409 => 46, // KP_DECIMAL -> .
        57410 => 47, // KP_DIVIDE -> /
        57411 => 42, // KP_MULTIPLY -> *
        57412 => 45, // KP_SUBTRACT -> -
        57413 => 43, // KP_ADD -> +
        57415 => 61, // KP_EQUAL -> =
        57416 => 44, // KP_SEPARATOR -> ,
        57417 => ARROW_LEFT,
        57418 => ARROW_RIGHT,
        57419 => ARROW_UP,
        57420 => ARROW_DOWN,
        57421 => FUNCTIONAL_PAGE_UP,
        57422 => FUNCTIONAL_PAGE_DOWN,
        57423 => FUNCTIONAL_HOME,
        57424 => FUNCTIONAL_END,
        57425 => FUNCTIONAL_INSERT,
        57426 => FUNCTIONAL_DELETE,
        _ => codepoint,
    }
}

/// Whether the character is one of the symbol keys.
fn is_symbol_key(character: char) -> bool {
    matches!(
        character,
        '`' | '-'
            | '='
            | '['
            | ']'
            | '\\'
            | ';'
            | '\''
            | ','
            | '.'
            | '/'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '|'
            | '~'
            | '{'
            | '}'
            | ':'
            | '<'
            | '>'
            | '?'
    )
}

/// Whether the character is one of the symbols a shifted key produces.
fn is_shifted_symbol_key(character: char) -> bool {
    matches!(
        character,
        '~' | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '|'
            | '{'
            | '}'
            | ':'
            | '<'
            | '>'
            | '?'
    )
}

/// A Kitty keyboard protocol key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyKey {
    /// The raw input data the key was parsed from.
    pub data: String,
    /// The key identifier, e.g. `ctrl+c`.
    pub id: String,
    /// The primary codepoint of the key.
    pub codepoint: i64,
    /// The shifted codepoint, when the terminal reports one.
    pub shifted_key: Option<i64>,
    /// The base layout key (the PC-101 position), when the terminal reports one.
    pub base_layout_key: Option<i64>,
    /// The modifier bitmask: shift 1, alt 2, ctrl 4, super 8.
    pub modifier: u32,
    /// The event type of the sequence.
    pub event_type: KeyEventType,
}

/// An xterm `modifyOtherKeys` key (`CSI 27 ; modifiers ; keycode ~`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModifyOtherKeysKey {
    /// The raw input data the key was parsed from.
    pub data: String,
    /// The key identifier, e.g. `ctrl+c`.
    pub id: String,
    /// The codepoint of the key.
    pub codepoint: i64,
    /// The modifier bitmask: shift 1, alt 2, ctrl 4, super 8.
    pub modifier: u32,
}

/// A legacy terminal sequence or a raw control byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyKey {
    /// The raw input data the key was parsed from.
    pub data: String,
    /// The key identifier, e.g. `ctrl+c`.
    pub id: String,
}

/// A key parsed from a terminal input chunk.
///
/// The variants mirror the input encodings the upstream recognizes. The raw
/// data is kept so that a key can be matched against any binding, exactly
/// like the upstream's data-driven matching, and so that the caller can
/// insert the text the key produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A Kitty keyboard protocol sequence (`CSI u` and its variants).
    Kitty(KittyKey),
    /// An xterm `modifyOtherKeys` sequence.
    ModifyOtherKeys(ModifyOtherKeysKey),
    /// A legacy terminal sequence or a raw control byte.
    Legacy(LegacyKey),
}

impl Key {
    /// The key identifier, e.g. `ctrl+c`.
    pub fn id(&self) -> &str {
        match self {
            Key::Kitty(key) => &key.id,
            Key::ModifyOtherKeys(key) => &key.id,
            Key::Legacy(key) => &key.id,
        }
    }

    /// The raw input data the key was parsed from.
    pub fn data(&self) -> &str {
        match self {
            Key::Kitty(key) => &key.data,
            Key::ModifyOtherKeys(key) => &key.data,
            Key::Legacy(key) => &key.data,
        }
    }

    /// The event type of the key.
    ///
    /// Only the Kitty protocol reports the event type; the other encodings
    /// always report a press.
    pub fn event_type(&self) -> KeyEventType {
        match self {
            Key::Kitty(key) => key.event_type,
            Key::ModifyOtherKeys(_) | Key::Legacy(_) => KeyEventType::Press,
        }
    }

    /// The modifier bitmask of the key: shift 1, alt 2, ctrl 4, super 8.
    ///
    /// The legacy encodings do not report their modifiers.
    pub fn modifier(&self) -> u32 {
        match self {
            Key::Kitty(key) => key.modifier,
            Key::ModifyOtherKeys(key) => key.modifier,
            Key::Legacy(_) => 0,
        }
    }
}

/// Sets the global Kitty keyboard protocol state.
///
/// The terminal calls this after it detected that the protocol is supported.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Relaxed);
}

/// Whether the Kitty keyboard protocol is active.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Relaxed)
}

/// The event type of the last parsed Kitty sequence.
pub fn last_event_type() -> KeyEventType {
    match LAST_EVENT_TYPE.load(Ordering::Relaxed) {
        1 => KeyEventType::Repeat,
        2 => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

/// The raw backspace byte is ambiguous in legacy terminals.
///
/// Windows Terminal uses it for Ctrl+Backspace, some legacy terminals and
/// tmux setups send it for a plain Backspace.
fn matches_raw_backspace(data: &str, expected_modifier: u32) -> bool {
    if data == "\x7f" {
        return expected_modifier == 0;
    }
    if data != "\x08" {
        return false;
    }
    if is_windows_terminal_session() {
        expected_modifier == MODIFIER_CTRL
    } else {
        expected_modifier == 0
    }
}

/// Whether the session runs in a local Windows Terminal.
fn is_windows_terminal_session() -> bool {
    thread_local! {
        /// The test override of the environment-based detection.
        static WINDOWS_TERMINAL_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    WINDOWS_TERMINAL_OVERRIDE.with(|override_value| {
        if let Some(active) = override_value.get() {
            return active;
        }
        let has_session = std::env::var("WT_SESSION").is_ok_and(|value| !value.is_empty());
        has_session
            && std::env::var_os("SSH_CONNECTION").is_none()
            && std::env::var_os("SSH_CLIENT").is_none()
            && std::env::var_os("SSH_TTY").is_none()
    })
}

/// Overrides the Windows Terminal session detection for the current thread.
///
/// `None` restores the environment-based detection. The upstream reads the
/// environment variables directly; the override keeps the tests independent
/// of the machine they run on.
pub fn set_windows_terminal_session_override(active: Option<bool>) {
    thread_local! {
        /// The test override of the environment-based detection.
        static WINDOWS_TERMINAL_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }
    WINDOWS_TERMINAL_OVERRIDE.with(|override_value| override_value.set(active));
}

/// A key identifier split into its key and its modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedKeyId {
    key: String,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_key: bool,
}

/// Splits a key identifier such as `ctrl+shift+p` into its parts.
fn parse_key_id(key_id: &str) -> Option<ParsedKeyId> {
    let mut key = key_id.to_lowercase();
    let mut shift = false;
    let mut alt = false;
    let mut ctrl = false;
    let mut super_key = false;

    loop {
        let modifier = if key.starts_with("shift+") {
            Some("shift")
        } else if key.starts_with("alt+") {
            Some("alt")
        } else if key.starts_with("ctrl+") {
            Some("ctrl")
        } else if key.starts_with("super+") {
            Some("super")
        } else {
            None
        };

        let Some(modifier) = modifier else { break };
        let already_set = match modifier {
            "shift" => shift,
            "alt" => alt,
            "ctrl" => ctrl,
            _ => super_key,
        };
        if already_set {
            return None;
        }
        match modifier {
            "shift" => shift = true,
            "alt" => alt = true,
            "ctrl" => ctrl = true,
            _ => super_key = true,
        }
        key = key[modifier.len() + 1..].to_string();
    }

    if key.is_empty() || (key.contains('+') && key != "+") {
        return None;
    }

    Some(ParsedKeyId {
        key,
        ctrl,
        shift,
        alt,
        super_key,
    })
}

/// The modifier bitmask of a parsed key identifier.
fn modifier_mask(parsed: &ParsedKeyId) -> u32 {
    let mut modifier = 0;
    if parsed.shift {
        modifier |= MODIFIER_SHIFT;
    }
    if parsed.alt {
        modifier |= MODIFIER_ALT;
    }
    if parsed.ctrl {
        modifier |= MODIFIER_CTRL;
    }
    if parsed.super_key {
        modifier |= MODIFIER_SUPER;
    }
    modifier
}

/// The control character a key produces when it is pressed with Ctrl.
fn raw_ctrl_char(key: &str) -> Option<char> {
    let mut characters = key.chars();
    let first = characters.next()?;
    let lower = first.to_lowercase().next()?;
    let code = lower as u32;
    if (97..=122).contains(&code) || matches!(lower, '[' | '\\' | ']' | '_') {
        return char::from_u32(code & 0x1f);
    }
    // `-` shares the physical key with `_` on US keyboards.
    if lower == '-' {
        return char::from_u32(31);
    }
    None
}

/// Normalizes a Kitty functional codepoint to the key it stands for.
fn normalize_kitty_functional_codepoint(codepoint: i64) -> i64 {
    kitty_functional_equivalent(codepoint)
}

/// A shifted uppercase letter is the same key as its lowercase letter.
fn normalize_shifted_letter_identity_codepoint(codepoint: i64, modifier: u32) -> i64 {
    let effective_modifier = modifier & !LOCK_MASK;
    if effective_modifier & MODIFIER_SHIFT != 0 && (65..=90).contains(&codepoint) {
        return codepoint + 32;
    }
    codepoint
}

/// The symbol a shifted key produces, without the implicit Shift modifier.
fn logical_shifted_symbol(
    codepoint: Option<i64>,
    modifier: u32,
    shifted_symbol: bool,
) -> Option<(i64, u32)> {
    let codepoint = codepoint?;
    if modifier & MODIFIER_SHIFT == 0 {
        return None;
    }
    let normalized = normalize_kitty_functional_codepoint(codepoint);
    let character = char::from_u32(u32::try_from(normalized).ok()?)?;
    let allowed = if shifted_symbol {
        is_shifted_symbol_key(character)
    } else {
        is_symbol_key(character)
    };
    if !allowed {
        return None;
    }
    Some((normalized, modifier & !MODIFIER_SHIFT))
}

/// Reads a run of decimal digits.
fn read_digits(bytes: &[u8], index: &mut usize) -> Option<u32> {
    let start = *index;
    while *index < bytes.len() && bytes[*index].is_ascii_digit() {
        *index += 1;
    }
    if *index == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..*index])
        .ok()?
        .parse::<u32>()
        .ok()
}

/// A parsed Kitty keyboard protocol sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedKittySequence {
    codepoint: i64,
    shifted_key: Option<i64>,
    base_layout_key: Option<i64>,
    modifier: u32,
    event_type: KeyEventType,
}

/// The event type of a Kitty sequence.
fn parse_event_type(value: Option<u32>) -> KeyEventType {
    match value {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

/// Parses a Kitty CSI-u sequence.
///
/// `CSI codepoint[:shifted[:base]];modifier[:event] u`
fn parse_csi_u_sequence(data: &str) -> Option<ParsedKittySequence> {
    let rest = data.strip_prefix("\x1b[")?;
    let bytes = rest.as_bytes();
    let mut index = 0;

    let codepoint = read_digits(bytes, &mut index)?;

    let shifted_key = if bytes.get(index) == Some(&b':') {
        index += 1;
        if bytes.get(index).is_some_and(u8::is_ascii_digit) {
            Some(i64::from(read_digits(bytes, &mut index)?))
        } else {
            None
        }
    } else {
        None
    };

    let base_layout_key = if bytes.get(index) == Some(&b':') {
        index += 1;
        Some(read_digits(bytes, &mut index)? as i64)
    } else {
        None
    };

    let modifier_value = if bytes.get(index) == Some(&b';') {
        index += 1;
        read_digits(bytes, &mut index)?
    } else {
        1
    };

    let event_value = if bytes.get(index) == Some(&b':') {
        index += 1;
        Some(read_digits(bytes, &mut index)?)
    } else {
        None
    };

    if bytes.get(index) != Some(&b'u') || index + 1 != bytes.len() {
        return None;
    }

    Some(ParsedKittySequence {
        codepoint: codepoint as i64,
        shifted_key,
        base_layout_key,
        modifier: modifier_value.wrapping_sub(1),
        event_type: parse_event_type(event_value),
    })
}

/// Parses an arrow key sequence with modifiers: `CSI 1 ; modifier [:event] A`.
fn parse_arrow_sequence(data: &str) -> Option<(u32, Option<u32>, char)> {
    let rest = data.strip_prefix("\x1b[1;")?;
    let bytes = rest.as_bytes();
    let mut index = 0;

    let modifier_value = read_digits(bytes, &mut index)?;
    let event_value = if bytes.get(index) == Some(&b':') {
        index += 1;
        Some(read_digits(bytes, &mut index)?)
    } else {
        None
    };

    let final_byte = *bytes.get(index)?;
    if index + 1 != bytes.len() || !matches!(final_byte, b'A' | b'B' | b'C' | b'D') {
        return None;
    }

    Some((modifier_value, event_value, final_byte as char))
}

/// Parses a functional key sequence: `CSI number [; modifier] [:event] ~`.
fn parse_function_sequence(data: &str) -> Option<(u32, u32, Option<u32>)> {
    let rest = data.strip_prefix("\x1b[")?;
    let body = rest.strip_suffix('~')?;
    let bytes = body.as_bytes();
    let mut index = 0;

    let key_number = read_digits(bytes, &mut index)?;
    let modifier_value = if bytes.get(index) == Some(&b';') {
        index += 1;
        read_digits(bytes, &mut index)?
    } else {
        1
    };
    let event_value = if bytes.get(index) == Some(&b':') {
        index += 1;
        Some(read_digits(bytes, &mut index)?)
    } else {
        None
    };

    if index != bytes.len() {
        return None;
    }

    Some((key_number, modifier_value, event_value))
}

/// Parses a Home/End sequence with modifiers: `CSI 1 ; modifier [:event] H|F`.
fn parse_home_end_sequence(data: &str) -> Option<(u32, Option<u32>, char)> {
    let rest = data.strip_prefix("\x1b[1;")?;
    let bytes = rest.as_bytes();
    let mut index = 0;

    let modifier_value = read_digits(bytes, &mut index)?;
    let event_value = if bytes.get(index) == Some(&b':') {
        index += 1;
        Some(read_digits(bytes, &mut index)?)
    } else {
        None
    };

    let final_byte = *bytes.get(index)?;
    if index + 1 != bytes.len() || !matches!(final_byte, b'H' | b'F') {
        return None;
    }

    Some((modifier_value, event_value, final_byte as char))
}

/// Parses an F1, F2 or F4 sequence: `CSI [1 ; modifier [:event]] P|Q|S`.
///
/// F3 is sent as `CSI 13 ~` because `CSI 1 ; modifier R` collides with the
/// cursor position reports.
fn parse_f_key_sequence(data: &str) -> Option<(u32, Option<u32>, char)> {
    let rest = data.strip_prefix("\x1b[")?;
    let bytes = rest.as_bytes();
    let mut index = 0;

    let mut modifier_value = 1;
    let mut event_value = None;
    if bytes.get(index) == Some(&b'1') && bytes.get(index + 1) == Some(&b';') {
        index += 2;
        modifier_value = read_digits(bytes, &mut index)?;
        if bytes.get(index) == Some(&b':') {
            index += 1;
            event_value = Some(read_digits(bytes, &mut index)?);
        }
    }

    let final_byte = *bytes.get(index)?;
    if index + 1 != bytes.len() || !matches!(final_byte, b'P' | b'Q' | b'S') {
        return None;
    }

    Some((modifier_value, event_value, final_byte as char))
}

/// The codepoint of a functional key number.
fn functional_codepoint(key_number: u32) -> Option<i64> {
    Some(match key_number {
        2 => FUNCTIONAL_INSERT,
        3 => FUNCTIONAL_DELETE,
        5 => FUNCTIONAL_PAGE_UP,
        6 => FUNCTIONAL_PAGE_DOWN,
        7 => FUNCTIONAL_HOME,
        8 => FUNCTIONAL_END,
        11 => FUNCTIONAL_F1,
        12 => FUNCTIONAL_F1 - 1,
        13 => FUNCTIONAL_F1 - 2,
        14 => FUNCTIONAL_F1 - 3,
        15 => FUNCTIONAL_F1 - 4,
        17 => FUNCTIONAL_F1 - 5,
        18 => FUNCTIONAL_F1 - 6,
        19 => FUNCTIONAL_F1 - 7,
        20 => FUNCTIONAL_F1 - 8,
        21 => FUNCTIONAL_F1 - 9,
        23 => FUNCTIONAL_F1 - 10,
        24 => FUNCTIONAL_F1 - 11,
        _ => return None,
    })
}

/// The codepoint of an F1, F2 or F4 sequence.
fn f_key_codepoint(final_char: char) -> Option<i64> {
    Some(match final_char {
        'P' => FUNCTIONAL_F1,
        'Q' => FUNCTIONAL_F1 - 1,
        'S' => FUNCTIONAL_F1 - 3,
        _ => return None,
    })
}

/// Parses a Kitty keyboard protocol sequence.
fn parse_kitty_sequence(data: &str) -> Option<ParsedKittySequence> {
    if let Some(parsed) = parse_csi_u_sequence(data) {
        store_event_type(parsed.event_type);
        return Some(parsed);
    }

    if let Some((modifier_value, event_value, final_char)) = parse_arrow_sequence(data) {
        let codepoint = match final_char {
            'A' => ARROW_UP,
            'B' => ARROW_DOWN,
            'C' => ARROW_RIGHT,
            _ => ARROW_LEFT,
        };
        let event_type = parse_event_type(event_value);
        store_event_type(event_type);
        return Some(ParsedKittySequence {
            codepoint,
            shifted_key: None,
            base_layout_key: None,
            modifier: modifier_value.wrapping_sub(1),
            event_type,
        });
    }

    if let Some((key_number, modifier_value, event_value)) = parse_function_sequence(data)
        && let Some(codepoint) = functional_codepoint(key_number)
    {
        let event_type = parse_event_type(event_value);
        store_event_type(event_type);
        return Some(ParsedKittySequence {
            codepoint,
            shifted_key: None,
            base_layout_key: None,
            modifier: modifier_value.wrapping_sub(1),
            event_type,
        });
    }

    if let Some((modifier_value, event_value, final_char)) = parse_home_end_sequence(data) {
        let codepoint = if final_char == 'H' {
            FUNCTIONAL_HOME
        } else {
            FUNCTIONAL_END
        };
        let event_type = parse_event_type(event_value);
        store_event_type(event_type);
        return Some(ParsedKittySequence {
            codepoint,
            shifted_key: None,
            base_layout_key: None,
            modifier: modifier_value.wrapping_sub(1),
            event_type,
        });
    }

    if let Some((modifier_value, event_value, final_char)) = parse_f_key_sequence(data) {
        let codepoint = f_key_codepoint(final_char)?;
        let event_type = parse_event_type(event_value);
        store_event_type(event_type);
        return Some(ParsedKittySequence {
            codepoint,
            shifted_key: None,
            base_layout_key: None,
            modifier: modifier_value.wrapping_sub(1),
            event_type,
        });
    }

    None
}

/// Stores the event type of the last parsed Kitty sequence.
fn store_event_type(event_type: KeyEventType) {
    let value = match event_type {
        KeyEventType::Press => 0,
        KeyEventType::Repeat => 1,
        KeyEventType::Release => 2,
    };
    LAST_EVENT_TYPE.store(value, Ordering::Relaxed);
}

/// A parsed xterm `modifyOtherKeys` sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedModifyOtherKeysSequence {
    codepoint: i64,
    modifier: u32,
}

/// Parses an xterm `modifyOtherKeys` sequence: `CSI 27 ; modifier ; keycode ~`.
fn parse_modify_other_keys_sequence(data: &str) -> Option<ParsedModifyOtherKeysSequence> {
    let rest = data.strip_prefix("\x1b[27;")?;
    let body = rest.strip_suffix('~')?;
    let bytes = body.as_bytes();
    let mut index = 0;

    let modifier_value = read_digits(bytes, &mut index)?;
    if bytes.get(index) != Some(&b';') {
        return None;
    }
    index += 1;
    let codepoint = read_digits(bytes, &mut index)?;
    if index != bytes.len() {
        return None;
    }

    Some(ParsedModifyOtherKeysSequence {
        codepoint: codepoint as i64,
        modifier: modifier_value.wrapping_sub(1),
    })
}

/// Whether the input data matches a Kitty keyboard protocol sequence.
fn matches_kitty_sequence(data: &str, expected_codepoint: i64, expected_modifier: u32) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_modifier = parsed.modifier & !LOCK_MASK;
    let expected_modifier = expected_modifier & !LOCK_MASK;

    let normalized_codepoint = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(parsed.codepoint),
        parsed.modifier,
    );
    let normalized_expected = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(expected_codepoint),
        expected_modifier,
    );

    // Physical match: the primary codepoint and every reported modifier match.
    if actual_modifier == expected_modifier && normalized_codepoint == normalized_expected {
        return true;
    }

    // Logical match: Kitty reports symbols such as `+` as Shift plus the
    // physical `=` key. The bindings name the produced symbol, so consume that
    // implicit Shift while preserving Ctrl, Alt and Super.
    if let Some((codepoint, modifier)) =
        logical_shifted_symbol(parsed.shifted_key, actual_modifier, false)
        && (modifier & !LOCK_MASK) == expected_modifier
        && codepoint == normalize_kitty_functional_codepoint(expected_codepoint)
    {
        return true;
    }

    if actual_modifier != expected_modifier {
        return false;
    }

    // Alternate match: use the base layout key for non-Latin keyboard layouts.
    // This allows Ctrl+C (Cyrillic) to match Ctrl+c (Latin) when the terminal
    // reports the base layout key (the key in the standard PC-101 layout).
    //
    // The fallback only applies when the codepoint is not a recognized Latin
    // letter (a-z) or symbol. When the codepoint is a recognized key it is
    // authoritative regardless of the physical key position, so that remapped
    // layouts (Dvorak, Colemak, xremap, ...) do not cause false matches.
    if parsed.base_layout_key == Some(expected_codepoint) {
        let is_latin_letter = (97..=122).contains(&normalized_codepoint);
        let is_known_symbol = u32::try_from(normalized_codepoint)
            .ok()
            .and_then(char::from_u32)
            .is_some_and(is_symbol_key);
        return !is_latin_letter && !is_known_symbol;
    }

    false
}

/// Whether the input data matches an xterm `modifyOtherKeys` sequence.
fn matches_modify_other_keys(data: &str, expected_keycode: i64, expected_modifier: u32) -> bool {
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    parsed.codepoint == expected_keycode && parsed.modifier == expected_modifier
}

/// Whether the input data matches a printable xterm `modifyOtherKeys` sequence.
fn matches_printable_modify_other_keys(
    data: &str,
    expected_keycode: i64,
    expected_modifier: u32,
) -> bool {
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    let physical_match = parsed.modifier == expected_modifier
        && normalize_shifted_letter_identity_codepoint(parsed.codepoint, parsed.modifier)
            == normalize_shifted_letter_identity_codepoint(expected_keycode, expected_modifier);
    if physical_match {
        return true;
    }
    logical_shifted_symbol(Some(parsed.codepoint), parsed.modifier, true).is_some_and(
        |(codepoint, modifier)| modifier == expected_modifier && codepoint == expected_keycode,
    )
}

/// The legacy sequences of a key.
fn legacy_key_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageUp" => &["\x1b[5~", "\x1b[[5~"],
        "pageDown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => return None,
    })
}

/// The legacy sequences of a key with the Shift modifier.
fn legacy_shift_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[a"],
        "down" => &["\x1b[b"],
        "right" => &["\x1b[c"],
        "left" => &["\x1b[d"],
        "clear" => &["\x1b[e"],
        "insert" => &["\x1b[2$"],
        "delete" => &["\x1b[3$"],
        "pageUp" => &["\x1b[5$"],
        "pageDown" => &["\x1b[6$"],
        "home" => &["\x1b[7$"],
        "end" => &["\x1b[8$"],
        _ => return None,
    })
}

/// The legacy sequences of a key with the Ctrl modifier.
fn legacy_ctrl_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1bOa"],
        "down" => &["\x1bOb"],
        "right" => &["\x1bOc"],
        "left" => &["\x1bOd"],
        "clear" => &["\x1bOe"],
        "insert" => &["\x1b[2^"],
        "delete" => &["\x1b[3^"],
        "pageUp" => &["\x1b[5^"],
        "pageDown" => &["\x1b[6^"],
        "home" => &["\x1b[7^"],
        "end" => &["\x1b[8^"],
        _ => return None,
    })
}

/// Whether the input data is one of the legacy sequences of the key.
fn matches_legacy_sequence(data: &str, key: &str) -> bool {
    legacy_key_sequences(key).is_some_and(|sequences| sequences.contains(&data))
}

/// Whether the input data is one of the legacy sequences of the modified key.
fn matches_legacy_modifier_sequence(data: &str, key: &str, modifier: u32) -> bool {
    if modifier == MODIFIER_SHIFT {
        return legacy_shift_sequences(key).is_some_and(|sequences| sequences.contains(&data));
    }
    if modifier == MODIFIER_CTRL {
        return legacy_ctrl_sequences(key).is_some_and(|sequences| sequences.contains(&data));
    }
    false
}

/// The key identifier of a legacy sequence.
fn legacy_sequence_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" => "clear",
        "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" => "f1",
        "\x1bOQ" => "f2",
        "\x1bOR" => "f3",
        "\x1bOS" => "f4",
        "\x1b[11~" => "f1",
        "\x1b[12~" => "f2",
        "\x1b[13~" => "f3",
        "\x1b[14~" => "f4",
        "\x1b[[A" => "f1",
        "\x1b[[B" => "f2",
        "\x1b[[C" => "f3",
        "\x1b[[D" => "f4",
        "\x1b[[E" => "f5",
        "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

/// The name of a key with its modifiers, e.g. `ctrl+shift+p`.
fn format_key_name_with_modifiers(key_name: &str, modifier: u32) -> Option<String> {
    let effective_modifier = modifier & !LOCK_MASK;
    let supported_modifier_mask = MODIFIER_SHIFT | MODIFIER_CTRL | MODIFIER_ALT | MODIFIER_SUPER;
    if effective_modifier & !supported_modifier_mask != 0 {
        return None;
    }

    let mut modifiers = Vec::new();
    if effective_modifier & MODIFIER_SHIFT != 0 {
        modifiers.push("shift");
    }
    if effective_modifier & MODIFIER_CTRL != 0 {
        modifiers.push("ctrl");
    }
    if effective_modifier & MODIFIER_ALT != 0 {
        modifiers.push("alt");
    }
    if effective_modifier & MODIFIER_SUPER != 0 {
        modifiers.push("super");
    }

    if modifiers.is_empty() {
        Some(key_name.to_string())
    } else {
        Some(format!("{}+{key_name}", modifiers.join("+")))
    }
}

/// The key identifier of a parsed codepoint and its modifiers.
fn format_parsed_key(
    codepoint: i64,
    modifier: u32,
    base_layout_key: Option<i64>,
) -> Option<String> {
    let normalized_codepoint = normalize_kitty_functional_codepoint(codepoint);
    let identity_codepoint =
        normalize_shifted_letter_identity_codepoint(normalized_codepoint, modifier);

    // The base layout key is only used when the codepoint is not a recognized
    // Latin letter (a-z), digit (0-9) or symbol. For those the codepoint is
    // authoritative regardless of the physical key position, so that remapped
    // layouts do not report the wrong key name.
    let is_latin_letter = (97..=122).contains(&identity_codepoint);
    let is_digit = (48..=57).contains(&identity_codepoint);
    let is_known_symbol = char::from_u32(u32::try_from(identity_codepoint).ok().unwrap_or(0))
        .is_some_and(is_symbol_key);
    let effective_codepoint = if is_latin_letter || is_digit || is_known_symbol {
        identity_codepoint
    } else {
        base_layout_key.unwrap_or(identity_codepoint)
    };

    let key_name = if effective_codepoint == CODEPOINT_ESCAPE {
        "escape".to_string()
    } else if effective_codepoint == CODEPOINT_TAB {
        "tab".to_string()
    } else if effective_codepoint == CODEPOINT_ENTER || effective_codepoint == CODEPOINT_KP_ENTER {
        "enter".to_string()
    } else if effective_codepoint == CODEPOINT_SPACE {
        "space".to_string()
    } else if effective_codepoint == CODEPOINT_BACKSPACE {
        "backspace".to_string()
    } else if effective_codepoint == FUNCTIONAL_DELETE {
        "delete".to_string()
    } else if effective_codepoint == FUNCTIONAL_INSERT {
        "insert".to_string()
    } else if effective_codepoint == FUNCTIONAL_HOME {
        "home".to_string()
    } else if effective_codepoint == FUNCTIONAL_END {
        "end".to_string()
    } else if effective_codepoint == FUNCTIONAL_PAGE_UP {
        "pageUp".to_string()
    } else if effective_codepoint == FUNCTIONAL_PAGE_DOWN {
        "pageDown".to_string()
    } else if (FUNCTIONAL_F12..=FUNCTIONAL_F1).contains(&effective_codepoint) {
        format!("f{}", FUNCTIONAL_F1 - effective_codepoint + 1)
    } else if effective_codepoint == ARROW_UP {
        "up".to_string()
    } else if effective_codepoint == ARROW_DOWN {
        "down".to_string()
    } else if effective_codepoint == ARROW_LEFT {
        "left".to_string()
    } else if effective_codepoint == ARROW_RIGHT {
        "right".to_string()
    } else {
        let ch = char::from_u32(u32::try_from(effective_codepoint).ok()?)?;
        if !ch.is_ascii_digit() && !ch.is_ascii_lowercase() && !is_symbol_key(ch) {
            return None;
        }
        ch.to_string()
    };

    format_key_name_with_modifiers(&key_name, modifier)
}

/// Whether the raw input data matches the key identifier.
///
/// This is the port of the upstream's `matchesKey(data, keyId)`.
pub fn matches_key_data(data: &str, key_id: &str) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };

    let key = parsed.key.as_str();
    let modifier = modifier_mask(&parsed);
    let kitty_active = is_kitty_protocol_active();

    match key {
        "escape" | "esc" => {
            if modifier != 0 {
                return false;
            }
            return data == "\x1b"
                || matches_kitty_sequence(data, CODEPOINT_ESCAPE, 0)
                || matches_modify_other_keys(data, CODEPOINT_ESCAPE, 0);
        }
        "space" => {
            if !kitty_active {
                if modifier == MODIFIER_CTRL && data == "\x00" {
                    return true;
                }
                if modifier == MODIFIER_ALT && data == "\x1b " {
                    return true;
                }
            }
            if modifier == 0 {
                return data == " "
                    || matches_kitty_sequence(data, CODEPOINT_SPACE, 0)
                    || matches_modify_other_keys(data, CODEPOINT_SPACE, 0);
            }
            return matches_kitty_sequence(data, CODEPOINT_SPACE, modifier)
                || matches_modify_other_keys(data, CODEPOINT_SPACE, modifier);
        }
        "tab" => {
            if modifier == MODIFIER_SHIFT {
                return data == "\x1b[Z"
                    || matches_kitty_sequence(data, CODEPOINT_TAB, MODIFIER_SHIFT)
                    || matches_modify_other_keys(data, CODEPOINT_TAB, MODIFIER_SHIFT);
            }
            if modifier == 0 {
                return data == "\t" || matches_kitty_sequence(data, CODEPOINT_TAB, 0);
            }
            return matches_kitty_sequence(data, CODEPOINT_TAB, modifier)
                || matches_modify_other_keys(data, CODEPOINT_TAB, modifier);
        }
        "enter" | "return" => {
            if modifier == MODIFIER_SHIFT {
                // The CSI-u sequences of the Kitty protocol.
                if matches_kitty_sequence(data, CODEPOINT_ENTER, MODIFIER_SHIFT)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, MODIFIER_SHIFT)
                {
                    return true;
                }
                // The xterm modifyOtherKeys format, for terminals without the
                // Kitty protocol.
                if matches_modify_other_keys(data, CODEPOINT_ENTER, MODIFIER_SHIFT) {
                    return true;
                }
                // With the Kitty protocol active the legacy sequences are
                // custom terminal mappings: `\x1b\r` is Kitty's
                // "map shift+enter send_text all \e\r" and `\n` is Ghostty's
                // "keybind = shift+enter=text:\n".
                if kitty_active {
                    return data == "\x1b\r" || data == "\n";
                }
                return false;
            }
            if modifier == MODIFIER_ALT {
                if matches_kitty_sequence(data, CODEPOINT_ENTER, MODIFIER_ALT)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, MODIFIER_ALT)
                {
                    return true;
                }
                if matches_modify_other_keys(data, CODEPOINT_ENTER, MODIFIER_ALT) {
                    return true;
                }
                // `\x1b\r` is alt+enter only in the legacy mode.
                if !kitty_active {
                    return data == "\x1b\r";
                }
                return false;
            }
            if modifier == 0 {
                return data == "\r"
                    || (!kitty_active && data == "\n")
                    || data == "\x1bOM"
                    || matches_kitty_sequence(data, CODEPOINT_ENTER, 0)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, 0);
            }
            return matches_kitty_sequence(data, CODEPOINT_ENTER, modifier)
                || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, modifier)
                || matches_modify_other_keys(data, CODEPOINT_ENTER, modifier);
        }
        "backspace" => {
            if modifier == MODIFIER_ALT {
                if data == "\x1b\x7f" || data == "\x1b\x08" {
                    return true;
                }
                return matches_kitty_sequence(data, CODEPOINT_BACKSPACE, MODIFIER_ALT)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, MODIFIER_ALT);
            }
            if modifier == MODIFIER_CTRL {
                // The legacy raw 0x08 is ambiguous: it can be Ctrl+Backspace
                // on Windows Terminal or a plain Backspace on other terminals,
                // while also overlapping with Ctrl+H.
                if matches_raw_backspace(data, MODIFIER_CTRL) {
                    return true;
                }
                return matches_kitty_sequence(data, CODEPOINT_BACKSPACE, MODIFIER_CTRL)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, MODIFIER_CTRL);
            }
            if modifier == 0 {
                return matches_raw_backspace(data, 0)
                    || matches_kitty_sequence(data, CODEPOINT_BACKSPACE, 0)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, 0);
            }
            return matches_kitty_sequence(data, CODEPOINT_BACKSPACE, modifier)
                || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, modifier);
        }
        "insert" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "insert")
                    || matches_kitty_sequence(data, FUNCTIONAL_INSERT, 0);
            }
            if matches_legacy_modifier_sequence(data, "insert", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_INSERT, modifier);
        }
        "delete" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "delete")
                    || matches_kitty_sequence(data, FUNCTIONAL_DELETE, 0);
            }
            if matches_legacy_modifier_sequence(data, "delete", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_DELETE, modifier);
        }
        "clear" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "clear");
            }
            return matches_legacy_modifier_sequence(data, "clear", modifier);
        }
        "home" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "home")
                    || matches_kitty_sequence(data, FUNCTIONAL_HOME, 0);
            }
            if matches_legacy_modifier_sequence(data, "home", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_HOME, modifier);
        }
        "end" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "end")
                    || matches_kitty_sequence(data, FUNCTIONAL_END, 0);
            }
            if matches_legacy_modifier_sequence(data, "end", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_END, modifier);
        }
        "pageup" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "pageUp")
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "pageUp", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, modifier);
        }
        "pagedown" => {
            if modifier == 0 {
                return matches_legacy_sequence(data, "pageDown")
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "pageDown", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, modifier);
        }
        "up" => {
            if modifier == MODIFIER_ALT {
                return data == "\x1bp" || matches_kitty_sequence(data, ARROW_UP, MODIFIER_ALT);
            }
            if modifier == 0 {
                return matches_legacy_sequence(data, "up")
                    || matches_kitty_sequence(data, ARROW_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "up", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, ARROW_UP, modifier);
        }
        "down" => {
            if modifier == MODIFIER_ALT {
                return data == "\x1bn" || matches_kitty_sequence(data, ARROW_DOWN, MODIFIER_ALT);
            }
            if modifier == 0 {
                return matches_legacy_sequence(data, "down")
                    || matches_kitty_sequence(data, ARROW_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "down", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, ARROW_DOWN, modifier);
        }
        "left" => {
            if modifier == MODIFIER_ALT {
                return data == "\x1b[1;3D"
                    || (!kitty_active && data == "\x1bB")
                    || data == "\x1bb"
                    || matches_kitty_sequence(data, ARROW_LEFT, MODIFIER_ALT);
            }
            if modifier == MODIFIER_CTRL {
                return data == "\x1b[1;5D"
                    || matches_legacy_modifier_sequence(data, "left", MODIFIER_CTRL)
                    || matches_kitty_sequence(data, ARROW_LEFT, MODIFIER_CTRL);
            }
            if modifier == 0 {
                return matches_legacy_sequence(data, "left")
                    || matches_kitty_sequence(data, ARROW_LEFT, 0);
            }
            if matches_legacy_modifier_sequence(data, "left", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, ARROW_LEFT, modifier);
        }
        "right" => {
            if modifier == MODIFIER_ALT {
                return data == "\x1b[1;3C"
                    || (!kitty_active && data == "\x1bF")
                    || data == "\x1bf"
                    || matches_kitty_sequence(data, ARROW_RIGHT, MODIFIER_ALT);
            }
            if modifier == MODIFIER_CTRL {
                return data == "\x1b[1;5C"
                    || matches_legacy_modifier_sequence(data, "right", MODIFIER_CTRL)
                    || matches_kitty_sequence(data, ARROW_RIGHT, MODIFIER_CTRL);
            }
            if modifier == 0 {
                return matches_legacy_sequence(data, "right")
                    || matches_kitty_sequence(data, ARROW_RIGHT, 0);
            }
            if matches_legacy_modifier_sequence(data, "right", modifier) {
                return true;
            }
            return matches_kitty_sequence(data, ARROW_RIGHT, modifier);
        }
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            let function_key = match key {
                "f1" => "f1",
                "f2" => "f2",
                "f3" => "f3",
                "f4" => "f4",
                "f5" => "f5",
                "f6" => "f6",
                "f7" => "f7",
                "f8" => "f8",
                "f9" => "f9",
                "f10" => "f10",
                "f11" => "f11",
                _ => "f12",
            };
            if modifier == 0 && matches_legacy_sequence(data, function_key) {
                return true;
            }
            let functional = match function_key {
                "f1" => FUNCTIONAL_F1,
                "f2" => FUNCTIONAL_F1 - 1,
                "f3" => FUNCTIONAL_F1 - 2,
                "f4" => FUNCTIONAL_F1 - 3,
                "f5" => FUNCTIONAL_F1 - 4,
                "f6" => FUNCTIONAL_F1 - 5,
                "f7" => FUNCTIONAL_F1 - 6,
                "f8" => FUNCTIONAL_F1 - 7,
                "f9" => FUNCTIONAL_F1 - 8,
                "f10" => FUNCTIONAL_F1 - 9,
                "f11" => FUNCTIONAL_F1 - 10,
                _ => FUNCTIONAL_F12,
            };
            return matches_kitty_sequence(data, functional, modifier);
        }
        _ => {}
    }

    // Single letter, digit and symbol keys.
    if key.len() == 1 {
        let character = key.chars().next().unwrap_or('\0');
        let is_letter = character.is_ascii_lowercase();
        let is_digit = character.is_ascii_digit();
        if is_letter || is_digit || is_symbol_key(character) {
            let codepoint = character as i64;
            let raw_ctrl = raw_ctrl_char(key);

            if modifier == MODIFIER_CTRL + MODIFIER_ALT && !kitty_active {
                // The legacy form of ctrl+alt+key is ESC followed by the control
                // character. When it does not match, the CSI-u and
                // modifyOtherKeys sequences of tmux can still be recognized.
                if let Some(raw_ctrl) = raw_ctrl
                    && data
                        .strip_prefix('\x1b')
                        .is_some_and(|tail| tail.len() == 1 && tail.starts_with(raw_ctrl))
                {
                    return true;
                }
            }

            if modifier == MODIFIER_ALT && !kitty_active {
                // The legacy form of alt+printable is ESC followed by the key.
                if data == format!("\x1b{character}") {
                    return true;
                }
            }

            if modifier == MODIFIER_CTRL {
                // The legacy form of ctrl+key sends the control character.
                if let Some(raw_ctrl) = raw_ctrl
                    && data.len() == 1
                    && data.starts_with(raw_ctrl)
                {
                    return true;
                }
                return matches_kitty_sequence(data, codepoint, MODIFIER_CTRL)
                    || matches_printable_modify_other_keys(data, codepoint, MODIFIER_CTRL);
            }

            if modifier == MODIFIER_SHIFT + MODIFIER_CTRL {
                return matches_kitty_sequence(data, codepoint, MODIFIER_SHIFT + MODIFIER_CTRL)
                    || matches_printable_modify_other_keys(
                        data,
                        codepoint,
                        MODIFIER_SHIFT + MODIFIER_CTRL,
                    );
            }

            if modifier == MODIFIER_SHIFT {
                // The legacy form of shift+letter produces the uppercase letter.
                if is_letter {
                    let uppercase = character.to_ascii_uppercase();
                    if data == uppercase.to_string() {
                        return true;
                    }
                }
                return matches_kitty_sequence(data, codepoint, MODIFIER_SHIFT)
                    || matches_printable_modify_other_keys(data, codepoint, MODIFIER_SHIFT);
            }

            if modifier != 0 {
                return matches_kitty_sequence(data, codepoint, modifier)
                    || matches_printable_modify_other_keys(data, codepoint, modifier);
            }

            // The raw character and the Kitty sequence are both checked, the
            // latter because the release events only arrive as a sequence.
            return data == key
                || matches_kitty_sequence(data, codepoint, 0)
                || matches_printable_modify_other_keys(data, codepoint, 0);
        }
    }

    false
}

/// Whether the key matches the key identifier.
///
/// The upstream matches the raw input data against the identifier; the parsed
/// key keeps the data it was parsed from, so the comparison is the same.
pub fn matches_key(key: &Key, id: &str) -> bool {
    matches_key_data(key.data(), id)
}

/// Parses the input data and returns the key, when it is recognized.
pub fn parse_key(data: &str) -> Option<Key> {
    if let Some(kitty) = parse_kitty_sequence(data) {
        let (codepoint, modifier) =
            logical_shifted_symbol(kitty.shifted_key, kitty.modifier, false)
                .unwrap_or((kitty.codepoint, kitty.modifier));
        let id = format_parsed_key(codepoint, modifier, kitty.base_layout_key)?;
        return Some(Key::Kitty(KittyKey {
            data: data.to_string(),
            id,
            codepoint: kitty.codepoint,
            shifted_key: kitty.shifted_key,
            base_layout_key: kitty.base_layout_key,
            modifier: kitty.modifier,
            event_type: kitty.event_type,
        }));
    }

    if let Some(modify_other_keys) = parse_modify_other_keys_sequence(data) {
        let (codepoint, modifier) = logical_shifted_symbol(
            Some(modify_other_keys.codepoint),
            modify_other_keys.modifier,
            true,
        )
        .unwrap_or((modify_other_keys.codepoint, modify_other_keys.modifier));
        let id = format_parsed_key(codepoint, modifier, None)?;
        return Some(Key::ModifyOtherKeys(ModifyOtherKeysKey {
            data: data.to_string(),
            id,
            codepoint: modify_other_keys.codepoint,
            modifier: modify_other_keys.modifier,
        }));
    }

    let id = parse_legacy_key_id(data)?;
    Some(Key::Legacy(LegacyKey {
        data: data.to_string(),
        id,
    }))
}

/// The key identifier of a legacy sequence or a raw control byte.
fn parse_legacy_key_id(data: &str) -> Option<String> {
    let kitty_active = is_kitty_protocol_active();

    // With the Kitty protocol active the ambiguous sequences are custom
    // terminal mappings: `\x1b\r` is shift+enter (the Kitty mapping), not
    // alt+enter, and `\n` is shift+enter (the Ghostty mapping).
    if kitty_active && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".to_string());
    }

    if let Some(id) = legacy_sequence_key_id(data) {
        return Some(id.to_string());
    }

    match data {
        "\x1b" => return Some("escape".to_string()),
        "\x1c" => return Some("ctrl+\\".to_string()),
        "\x1d" => return Some("ctrl+]".to_string()),
        "\x1f" => return Some("ctrl+-".to_string()),
        "\x1b\x1b" => return Some("ctrl+alt+[".to_string()),
        "\x1b\x1c" => return Some("ctrl+alt+\\".to_string()),
        "\x1b\x1d" => return Some("ctrl+alt+]".to_string()),
        "\x1b\x1f" => return Some("ctrl+alt+-".to_string()),
        "\t" => return Some("tab".to_string()),
        "\r" => return Some("enter".to_string()),
        "\n" if !kitty_active => return Some("enter".to_string()),
        "\x1bOM" => return Some("enter".to_string()),
        "\x00" => return Some("ctrl+space".to_string()),
        " " => return Some("space".to_string()),
        "\x7f" => return Some("backspace".to_string()),
        "\x08" => {
            return Some(
                if is_windows_terminal_session() {
                    "ctrl+backspace"
                } else {
                    "backspace"
                }
                .to_string(),
            );
        }
        "\x1b[Z" => return Some("shift+tab".to_string()),
        "\x1b\r" if !kitty_active => return Some("alt+enter".to_string()),
        "\x1b " if !kitty_active => return Some("alt+space".to_string()),
        "\x1b\x7f" | "\x1b\x08" => return Some("alt+backspace".to_string()),
        "\x1bB" if !kitty_active => return Some("alt+left".to_string()),
        "\x1bF" if !kitty_active => return Some("alt+right".to_string()),
        "\x1b[A" => return Some("up".to_string()),
        "\x1b[B" => return Some("down".to_string()),
        "\x1b[C" => return Some("right".to_string()),
        "\x1b[D" => return Some("left".to_string()),
        "\x1b[H" | "\x1bOH" => return Some("home".to_string()),
        "\x1b[F" | "\x1bOF" => return Some("end".to_string()),
        "\x1b[3~" => return Some("delete".to_string()),
        "\x1b[5~" => return Some("pageUp".to_string()),
        "\x1b[6~" => return Some("pageDown".to_string()),
        _ => {}
    }

    if !kitty_active && data.chars().count() == 2 {
        let mut characters = data.chars();
        if characters.next() == Some('\x1b') {
            let second = characters.next()?;
            let code = second as u32;
            if (1..=26).contains(&code) {
                return Some(format!("ctrl+alt+{}", char::from_u32(code + 96)?));
            }
            // The legacy alt+letter, alt+digit and alt+symbol: ESC followed by
            // the key.
            if second.is_ascii_lowercase() || second.is_ascii_digit() || is_symbol_key(second) {
                return Some(format!("alt+{second}"));
            }
        }
    }

    if data.chars().count() == 1 {
        let character = data.chars().next()?;
        let code = character as u32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", char::from_u32(code + 96)?));
        }
        if (32..=126).contains(&code) {
            return Some(data.to_string());
        }
    }

    None
}

/// Whether the last parsed key event was a key release.
///
/// Only meaningful when the Kitty keyboard protocol with flag 2 is active.
pub fn is_key_release(key: &Key) -> bool {
    is_key_release_data(key.data())
}

/// Whether the raw input data is a key release.
pub fn is_key_release_data(data: &str) -> bool {
    // The bracketed paste content is not a key release, even when it contains
    // patterns like `:3F` (a Bluetooth MAC address, for example). The terminal
    // re-wraps the paste content with the bracketed paste markers before it
    // passes it to the TUI, so pasted data always contains `\x1b[200~`.
    if data.contains("\x1b[200~") {
        return false;
    }

    // The release events with flag 2 contain `:3`.
    data.contains(":3u")
        || data.contains(":3~")
        || data.contains(":3A")
        || data.contains(":3B")
        || data.contains(":3C")
        || data.contains(":3D")
        || data.contains(":3H")
        || data.contains(":3F")
        || data.contains(":3P")
        || data.contains(":3Q")
        || data.contains(":3S")
}

/// Whether the last parsed key event was a key repeat.
///
/// Only meaningful when the Kitty keyboard protocol with flag 2 is active.
pub fn is_key_repeat(key: &Key) -> bool {
    is_key_repeat_data(key.data())
}

/// Whether the raw input data is a key repeat.
pub fn is_key_repeat_data(data: &str) -> bool {
    if data.contains("\x1b[200~") {
        return false;
    }

    data.contains(":2u")
        || data.contains(":2~")
        || data.contains(":2A")
        || data.contains(":2B")
        || data.contains(":2C")
        || data.contains(":2D")
        || data.contains(":2H")
        || data.contains(":2F")
        || data.contains(":2P")
        || data.contains(":2Q")
        || data.contains(":2S")
}

/// Decodes a Kitty CSI-u sequence into a printable character, if applicable.
///
/// When the Kitty keyboard protocol flag 1 (disambiguate) is active, the
/// terminals send CSI-u sequences for all keys, including the plain printable
/// characters. This extracts the printable character from such a sequence.
///
/// Only plain and Shift-modified keys are accepted. Ctrl, Alt and the
/// unsupported modifier combinations are rejected; the keybinding matching
/// handles those.
pub fn decode_kitty_printable(data: &str) -> Option<String> {
    let parsed = parse_csi_u_sequence(data)?;

    // The CSI-u groups: <codepoint>[:<shifted>[:<base>]];<mod>[:<event>]u
    let codepoint = parsed.codepoint;
    let shifted_key = parsed.shifted_key;
    // The modifiers are 1-indexed in CSI-u; normalize them to the bitmask.
    let modifier = parsed.modifier;

    // Reject the unsupported modifier bits (e.g. Super/Meta) so that the
    // characters of modifier-only terminal events are not inserted.
    let allowed_modifiers = MODIFIER_SHIFT | LOCK_MASK;
    if modifier & !allowed_modifiers != 0 {
        return None;
    }
    if modifier & (MODIFIER_ALT | MODIFIER_CTRL) != 0 {
        return None;
    }

    // The shifted keycode is preferred when Shift is held.
    let mut effective_codepoint = codepoint;
    if modifier & MODIFIER_SHIFT != 0
        && let Some(shifted_key) = shifted_key
    {
        effective_codepoint = shifted_key;
    }
    effective_codepoint = normalize_kitty_functional_codepoint(effective_codepoint);

    // Drop the control characters and the invalid codepoints.
    if effective_codepoint < 32 {
        return None;
    }
    // Drop the unmapped Kitty functional keys (F13-F35, media keys, ...).
    if (KITTY_FUNCTIONAL_PUA_START..=KITTY_FUNCTIONAL_PUA_END).contains(&effective_codepoint) {
        return None;
    }

    char::from_u32(u32::try_from(effective_codepoint).ok()?).map(|character| character.to_string())
}

/// Decodes an xterm `modifyOtherKeys` sequence into a printable character.
fn decode_modify_other_keys_printable(data: &str) -> Option<String> {
    let parsed = parse_modify_other_keys_sequence(data)?;
    let modifier = parsed.modifier & !LOCK_MASK;
    if modifier & !MODIFIER_SHIFT != 0 {
        return None;
    }
    if parsed.codepoint < 32 {
        return None;
    }
    char::from_u32(u32::try_from(parsed.codepoint).ok()?).map(|character| character.to_string())
}

/// Decodes the input data into a printable character, if applicable.
pub fn decode_printable_key(data: &str) -> Option<String> {
    decode_kitty_printable(data).or_else(|| decode_modify_other_keys_printable(data))
}
