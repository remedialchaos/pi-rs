//! Terminal question framing. Upstream `questionTerminal` uses readline for the prompt; the
//! OSC-side protocol implemented here is the wire format used when a question is sent as a
//! terminal control sequence: `ESC ] 52 ; q ; <base64 UTF-8> BEL` (or `ESC \` as the terminator).
//! It mirrors OSC 52 framing (same escape, same base64 payload) with a `q` selector so terminals
//! and the Rust decoder can tell questions from clipboard writes.

use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

pub const OSC_START: &str = "\x1b]";
pub const OSC_BEL: char = '\x07';
pub const OSC_ST: &str = "\x1b\\";
/// OSC 52 is the clipboard/escape-sequence channel used by the rest of the port.
pub const QUESTION_OSC_CODE: u16 = 52;
pub const QUESTION_SELECTOR: &str = "q";

/// Strip control characters that could terminate or inject an OSC sequence.
pub fn sanitize_osc_payload(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// Encode an OSC sequence with the numeric code and a raw payload.
pub fn encode_osc(code: u16, payload: &str) -> String {
    format!("{OSC_START}{code};{payload}{OSC_BEL}")
}

/// Decode an OSC sequence terminated by BEL or ST. Returns `(code, payload)`.
pub fn decode_osc(sequence: &str) -> Option<(u16, String)> {
    let rest = sequence.strip_prefix(OSC_START)?;
    let (code, rest) = rest.split_once(';')?;
    let code = code.parse::<u16>().ok()?;
    let payload = if let Some((payload, suffix)) = rest.split_once(OSC_ST) {
        if !suffix.is_empty() {
            return None;
        }
        payload
    } else {
        rest.strip_suffix(OSC_BEL)?
    };
    if payload.contains('\x1b') || payload.contains(OSC_BEL) {
        return None;
    }
    Some((code, payload.to_string()))
}

/// Encode a question using the `52;q;<base64>` selector payload.
pub fn encode_question(message: &str) -> String {
    let payload = STANDARD.encode(sanitize_osc_payload(message).as_bytes());
    encode_osc(QUESTION_OSC_CODE, &format!("{QUESTION_SELECTOR};{payload}"))
}

/// Decode a question produced by [`encode_question`]. Other OSC codes/selectors decode to `None`.
pub fn decode_question(sequence: &str) -> Option<String> {
    let (code, payload) = decode_osc(sequence)?;
    if code != QUESTION_OSC_CODE {
        return None;
    }
    let encoded = payload.strip_prefix(&format!("{QUESTION_SELECTOR};"))?;
    let bytes = STANDARD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

/// Write a question prompt followed by its OSC sequence to a terminal stream.
pub fn write_question<W: Write>(output: &mut W, message: &str) -> io::Result<()> {
    output.write_all(message.as_bytes())?;
    output.write_all(encode_question(message).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_encoding_is_exact() {
        assert_eq!(encode_question("Ready?"), "\x1b]52;q;UmVhZHk/\x07");
    }

    #[test]
    fn question_roundtrip_handles_unicode() {
        let encoded = encode_question("確認しますか？");
        assert_eq!(decode_question(&encoded).as_deref(), Some("確認しますか？"));
    }

    #[test]
    fn decode_accepts_st_terminator() {
        let sequence = "\x1b]52;q;SGVsbG8=\x1b\\";
        assert_eq!(decode_question(sequence).as_deref(), Some("Hello"));
        assert_eq!(
            decode_osc("\x1b]0;my title\x1b\\"),
            Some((0, "my title".to_string()))
        );
    }

    #[test]
    fn decode_rejects_bad_framing() {
        assert_eq!(decode_question("no escape"), None);
        assert_eq!(decode_question("\x1b]52;q;not base64!\x07"), None);
        assert_eq!(decode_question("\x1b]9;q;SGVsbG8=\x07"), None);
        assert_eq!(decode_question("\x1b]52;c;SGVsbG8=\x07"), None);
        assert_eq!(decode_osc("\x1b]52;q;abc"), None);
    }

    #[test]
    fn sanitize_removes_control_characters() {
        assert_eq!(sanitize_osc_payload("a\x07b\x1bc"), "abc");
        assert_eq!(sanitize_osc_payload("日本語"), "日本語");
    }

    #[test]
    fn write_question_writes_prompt_and_sequence() -> Result<(), String> {
        let mut output = Vec::new();
        write_question(&mut output, "Go?").map_err(|e| e.to_string())?;
        let text = String::from_utf8(output).map_err(|e| e.to_string())?;
        assert_eq!(text, format!("Go?{}", encode_question("Go?")));
        assert_eq!(
            decode_question(&text["Go?".len()..]).as_deref(),
            Some("Go?")
        );
        Ok(())
    }
}
