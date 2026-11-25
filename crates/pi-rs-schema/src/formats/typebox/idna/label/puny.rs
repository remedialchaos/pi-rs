// The ACE (punycode) label rules, ported from `packages/schema/src/formats/typebox/idna/label/puny.ts`.

use super::super::format::puny::{decode, is_ace_prefixed};
use super::super::pattern::pattern::RE_NON_ASCII;
use super::unicode::is_unicode_label;

/// Whether the value is a valid ACE label.
pub fn is_puny_label(value: &str) -> bool {
    if !is_ace_prefixed(value) {
        return false;
    }
    let body = value[4..].to_lowercase();
    // A payload consisting of only a hyphen, or starting with the delimiter (like "-9uc"), has 0 basic ASCII
    // characters preceding it and is malformed.
    if body.rfind('-') == Some(0) {
        return false;
    }
    let Ok(decoded) = decode(&body) else {
        return false;
    };
    // RFC 5890 §2.3.2.1 - a U-label must contain at least one non-ASCII character.
    if RE_NON_ASCII.find(&decoded).is_none() {
        return false;
    }
    is_unicode_label(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ace_labels() {
        assert!(is_puny_label("xn--bcher-kva"));
        assert!(is_puny_label("XN--bcher-kva"));
    }

    #[test]
    fn rejects_labels_that_are_not_ace_prefixed() {
        assert!(!is_puny_label("bcher-kva"));
        assert!(!is_puny_label("example"));
        assert!(!is_puny_label(""));
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert!(!is_puny_label("xn--"));
        assert!(!is_puny_label("xn---"));
        assert!(!is_puny_label("xn--bcher-kv!"));
    }

    #[test]
    fn rejects_a_payload_without_non_ascii_characters() {
        assert!(!is_puny_label("xn--example-"));
    }
}
