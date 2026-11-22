//! ANSI escape handling: the Rust port of `packages/coding-agent/src/utils/ansi.ts`.

use std::sync::OnceLock;

use fancy_regex::Regex;

const ST: &str = r"(?:\x07|\x1b\x5c|\x9c)";
const OSC_START: &str = r"\x1b\]";
const CSI_START: &str =
    r"(?:(?:\x1b\[|\x9b)[<=>]|[\x1b\x9b][\[\]()#;?]*)(?:\d{1,4}(?:[;:]\d{0,4})*)?";
const CSI_FINAL: &str = r"[\dA-PR-TZcf-nq-uy=><~]";
const MAX_PENDING_ANSI_LENGTH: usize = 256;

fn regex(pending: bool) -> Option<&'static Regex> {
    static CACHE: OnceLock<[Option<Regex>; 2]> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        [
            Regex::new(&format!(
                "(?:{OSC_START}[\\s\\S]*?{ST})|{CSI_START}{CSI_FINAL}"
            ))
            .ok(),
            Regex::new(&format!(
                "(?:{OSC_START}(?:[^\\x07\\x9c\\x1b]|\\x1b(?!\\\\))*|{CSI_START})$"
            ))
            .ok(),
        ]
    });
    cache[usize::from(pending)].as_ref()
}

fn unit_offset(value: &str, units: usize) -> usize {
    if units == 0 {
        return 0;
    }
    let mut seen = 0;
    for (byte, character) in value.char_indices() {
        seen += character.len_utf16();
        if seen >= units {
            return byte + character.len_utf8();
        }
    }
    value.len()
}

/// Removes every complete ANSI escape sequence, like upstream's `stripAnsi`.
pub fn strip_ansi(value: &str) -> String {
    if !value.contains('\u{1b}') && !value.contains('\u{9b}') {
        return value.to_string();
    }
    match regex(false) {
        Some(pattern) => pattern.replace_all(value, "").into_owned(),
        None => value.to_string(),
    }
}

/// Splits streamed text into a complete prefix and a trailing unfinished escape sequence.
pub fn split_incomplete_ansi_suffix(value: &str) -> (String, String) {
    if !value.contains('\u{1b}') && !value.contains('\u{9b}') {
        return (value.to_string(), String::new());
    }
    let units = value.encode_utf16().count();
    let base = unit_offset(value, units.saturating_sub(MAX_PENDING_ANSI_LENGTH));
    let window = &value[base..];
    match regex(true).and_then(|pattern| pattern.find(window).ok().flatten()) {
        Some(found) => {
            let split = base + found.start();
            (value[..split].to_string(), value[split..].to_string())
        }
        None => (value.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        let input = "a\u{1b}[31mred\u{1b}[0m\u{1b}]8;;https://example.com\u{7}linkz";
        assert_eq!(strip_ansi(input), "aredlinkz");
    }

    #[test]
    fn holds_back_unfinished_csi() {
        assert_eq!(
            split_incomplete_ansi_suffix("x\u{1b}[<35;59"),
            ("x".to_string(), "\u{1b}[<35;59".to_string())
        );
    }
}
