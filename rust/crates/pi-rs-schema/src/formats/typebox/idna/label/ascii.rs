// The ASCII label rules, ported from `packages/schema/src/formats/typebox/idna/label/ascii.ts`.

use super::super::pattern::pattern::{
    RE_ASCII_LDH, RE_RULE_HYPHEN_PLACEMENT, RE_RULE_NOT_RESERVED_ACE,
};

/// Whether the value is a valid ASCII LDH label, per RFC 5891 §4.2.3.1.
pub fn is_ascii_label(value: &str) -> bool {
    RE_RULE_HYPHEN_PLACEMENT.find(value).is_some()
        && RE_RULE_NOT_RESERVED_ACE.find(value).is_some()
        && RE_ASCII_LDH.find(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ldh_labels() {
        assert!(is_ascii_label("example"));
        assert!(is_ascii_label("a"));
        assert!(is_ascii_label("a-b"));
        assert!(is_ascii_label("123"));
    }

    #[test]
    fn rejects_labels_with_invalid_hyphens() {
        assert!(!is_ascii_label("-example"));
        assert!(!is_ascii_label("example-"));
        assert!(!is_ascii_label("xn--example"));
        assert!(!is_ascii_label("ab--example"));
    }

    #[test]
    fn rejects_non_ascii_labels() {
        assert!(!is_ascii_label("bücher"));
        assert!(!is_ascii_label("example.com"));
    }

    #[test]
    fn accepts_the_empty_label() {
        // Upstream: the hyphen placement, reserved ACE and LDH patterns all match the empty string.
        assert!(is_ascii_label(""));
    }
}
