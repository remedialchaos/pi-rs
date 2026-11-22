// Ported from packages/tui/src/program-status.ts
//
// Program Status Protocol (OSC 7501): a program tells the terminal whether it is idle, working,
// blocked on the user, done, or failed. Only the root record is supported.
//
// Spec: https://www.superlogical.com/rex/docs/build/program-status

use crate::utils::replace_control_characters;

/// Feature detection query. A supporting terminal replies with the same body.
pub const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

/// Decoded `msg` limit. Its base64 encoding stays under the 2732-byte encoded limit.
const MAX_MESSAGE_BYTES: usize = 2048;

/// The state of a program status report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgramStatusState {
    /// The program waits for nothing.
    #[default]
    Idle,
    /// The program is busy.
    Working,
    /// The program waits for the user.
    Blocked,
    /// The program is done.
    Done,
    /// The program failed.
    Error,
    /// `clear` removes the status instead of reporting one.
    Clear,
}

impl ProgramStatusState {
    /// The name of the state in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Error => "error",
            Self::Clear => "clear",
        }
    }
}

/// What a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    /// The program waits for a permission.
    Permission,
    /// The program waits for an answer.
    Question,
    /// The program waits for credentials.
    Auth,
}

impl ProgramStatusKind {
    /// The name of the kind in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// A program status report.
#[derive(Debug, Clone, Default)]
pub struct ProgramStatus {
    /// `clear` removes the status instead of reporting one.
    pub state: ProgramStatusState,
    /// Stable program name, `[A-Za-z0-9_.+-]{1,32}`. Other values are omitted.
    pub app: Option<String>,
    /// What a blocked program waits for. Omitted for other states.
    pub kind: Option<ProgramStatusKind>,
    /// One human-readable line. Control characters become spaces; longer text is cut to the spec limit.
    pub message: Option<String>,
}

/// The reply to [`PROGRAM_STATUS_QUERY`]. Later spec revisions may add pairs after the `?`.
pub fn is_program_status_reply(sequence: &str) -> bool {
    let Some(rest) = sequence.strip_prefix("\x1b]7501;?") else {
        return false;
    };
    if let Some(body) = rest.strip_suffix('\x07') {
        return !body.contains('\x07') && !body.contains('\x1b');
    }
    if let Some(body) = rest.strip_suffix("\x1b\\") {
        return !body.contains('\x07') && !body.contains('\x1b');
    }
    false
}

/// Encode a status report. Terminals discard reports whose text contains control characters, so they are replaced.
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = &status.app
        && is_valid_app_name(app)
    {
        pairs.push(format!("app={app}"));
    }
    if status.state == ProgramStatusState::Blocked
        && let Some(kind) = status.kind
    {
        pairs.push(format!("kind={}", kind.as_str()));
    }
    let message = truncate_utf8(
        trim_javascript(&replace_control_characters(
            status.message.as_deref().unwrap_or_default(),
        )),
        MAX_MESSAGE_BYTES,
    );
    if !message.is_empty() {
        pairs.push(format!("msg={}", base64_encode(message.as_bytes())));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

/// Whether the app name matches `[A-Za-z0-9_.+-]{1,32}`.
fn is_valid_app_name(app: &str) -> bool {
    !app.is_empty()
        && app.chars().count() <= 32
        && app.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '+' | '-')
        })
}

/// The text cut to the given number of UTF-8 bytes, on a character boundary.
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut bytes = 0;
    let mut end = 0;
    for character in text.chars() {
        let size = character.len_utf8();
        if bytes + size > max_bytes {
            break;
        }
        bytes += size;
        end += character.len_utf16();
    }
    truncate_to_utf16_units(text, end)
}

/// The text cut to the given number of UTF-16 code units.
fn truncate_to_utf16_units(text: &str, units: usize) -> String {
    let mut result = String::new();
    let mut count = 0;
    for character in text.chars() {
        if count >= units {
            break;
        }
        result.push(character);
        count += character.len_utf16();
    }
    result
}

/// The text without the leading and trailing whitespace, as JavaScript trims it.
fn trim_javascript(text: &str) -> &str {
    text.trim_matches(|character: char| {
        matches!(
            character,
            '\u{9}'..='\u{d}'
                | '\u{20}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// The base64 encoding of the bytes, with the padding.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or_default(),
            chunk.get(2).copied().unwrap_or_default(),
        ];
        let group = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        result.push(ALPHABET[(group >> 18 & 0x3f) as usize] as char);
        result.push(ALPHABET[(group >> 12 & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[(group >> 6 & 0x3f) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[(group & 0x3f) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{PROGRAM_STATUS_QUERY, ProgramStatus, ProgramStatusKind, ProgramStatusState};
    use super::{format_program_status, is_program_status_reply};

    fn base64_decode(text: &str) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bytes = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for character in text.chars() {
            if character == '=' {
                break;
            }
            let Some(index) = ALPHABET.iter().position(|byte| *byte as char == character) else {
                continue;
            };
            buffer = buffer << 6 | u32::try_from(index).unwrap_or_default();
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                bytes.push((buffer >> bits & 0xff) as u8);
            }
        }
        String::from_utf8(bytes).unwrap_or_default()
    }

    fn message_of(sequence: &str) -> Option<String> {
        let start = sequence.find(":msg=")? + ":msg=".len();
        let end = sequence[start..]
            .find(':')
            .map_or(sequence.len(), |offset| start + offset);
        Some(base64_decode(&sequence[start..end]))
    }

    #[test]
    fn encodes_state_app_kind_and_a_base64_message() {
        let status = ProgramStatus {
            state: ProgramStatusState::Blocked,
            app: Some("pi".to_string()),
            kind: Some(ProgramStatusKind::Permission),
            message: Some("Allow bash?".to_string()),
        };
        assert_eq!(
            format_program_status(&status),
            "\x1b]7501;state=blocked:app=pi:kind=permission:msg=QWxsb3cgYmFzaD8=\x1b\\"
        );
        let status = ProgramStatus {
            state: ProgramStatusState::Clear,
            ..Default::default()
        };
        assert_eq!(
            format_program_status(&status),
            "\x1b]7501;state=clear\x1b\\"
        );
    }

    #[test]
    fn omits_kind_outside_blocked_invalid_app_names_and_empty_messages() {
        let status = ProgramStatus {
            state: ProgramStatusState::Working,
            app: Some("my app".to_string()),
            kind: Some(ProgramStatusKind::Auth),
            message: Some(" \n ".to_string()),
        };
        assert_eq!(
            format_program_status(&status),
            "\x1b]7501;state=working\x1b\\"
        );
        let status = ProgramStatus {
            state: ProgramStatusState::Idle,
            app: Some("a".repeat(32)),
            ..Default::default()
        };
        assert!(format_program_status(&status).contains(&format!(":app={}\x1b", "a".repeat(32))));
        let status = ProgramStatus {
            state: ProgramStatusState::Idle,
            app: Some("a".repeat(33)),
            ..Default::default()
        };
        assert!(!format_program_status(&status).contains("app="));
    }

    #[test]
    fn replaces_control_characters_which_make_terminals_discard_the_report() {
        let status = ProgramStatus {
            state: ProgramStatusState::Error,
            message: Some("first\nsecond\x1b[31m\u{9b}third\t".to_string()),
            ..Default::default()
        };
        let sequence = format_program_status(&status);
        assert_eq!(
            message_of(&sequence),
            Some("first second [31m third".to_string())
        );
    }

    #[test]
    fn cuts_long_messages_at_a_utf8_boundary_within_the_spec_limits() {
        let status = ProgramStatus {
            state: ProgramStatusState::Working,
            app: Some("pi".to_string()),
            message: Some("é".repeat(2000)),
            ..Default::default()
        };
        let sequence = format_program_status(&status);
        let message = message_of(&sequence).unwrap_or_default();
        assert_eq!(message, "é".repeat(1024));
        assert!(message.len() <= 2048);
        assert!(sequence.len() <= 4096);
    }

    #[test]
    fn accepts_the_query_echo_with_either_terminator_and_future_pairs() {
        assert!(is_program_status_reply("\x1b]7501;?\x1b\\"));
        assert!(is_program_status_reply("\x1b]7501;?\x07"));
        assert!(is_program_status_reply("\x1b]7501;?version=2\x1b\\"));
        assert!(!is_program_status_reply("\x1b]7501;state=idle\x1b\\"));
        assert!(!is_program_status_reply("\x1b]11;rgb:0000/0000/0000\x07"));
        assert_eq!(PROGRAM_STATUS_QUERY, "\x1b]7501;?\x1b\\");
    }
}
