// The Unicode (U-label) rules, ported from `packages/schema/src/formats/typebox/idna/label/unicode.ts`.

use super::super::format::bidi::{has_right_to_left_characters, satisfies_bidi_rule};
use super::super::format::puny::encode;
use super::super::pattern::pattern::{
    RE_COMBINING_MARK, RE_PERMITTED_CATEGORY, RE_RFC5892_DISALLOWED, RE_SCRIPT_GREEK,
    RE_SCRIPT_HEBREW, RE_SCRIPT_JAPANESE, RE_VIRAMA,
};

/// The maximum length of an A-label, in octets.
const MAX_A_LABEL_LENGTH: usize = 63;
/// The length of the `xn--` ACE prefix.
const ACE_PREFIX_LENGTH: usize = 4;

/// Whether the A-label form of the value exceeds the maximum length, per RFC 5890 §2.3.2.1.
///
/// Only labels containing non-ASCII characters need checking: a pure-ASCII label has no `xn--` prefix and is
/// used as is, so its punycode-encoded length is irrelevant.
fn exceeds_max_a_label_length(value: &str) -> bool {
    !value.is_ascii() && encode(value).len() + ACE_PREFIX_LENGTH > MAX_A_LABEL_LENGTH
}

/// Whether the label has invalid hyphens, per RFC 5891 §4.2.3.2.
///
/// The label must not start or end with a hyphen, and must not have hyphens in the third and fourth code point
/// positions (the ACE prefix reservation, e.g. "xn--").
fn has_invalid_hyphens(value: &str) -> bool {
    if value.starts_with('-') || value.ends_with('-') {
        return true;
    }
    let third = value.chars().nth(2);
    let fourth = value.chars().nth(3);
    third == Some('-') && fourth == Some('-')
}

/// Whether the character is a combining mark.
fn is_combining_mark(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_COMBINING_MARK.find(text).is_some()
}

/// Whether the character is a Greek character.
fn is_greek(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_SCRIPT_GREEK.find(text).is_some()
}

/// Whether the character is a Hebrew character.
fn is_hebrew(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_SCRIPT_HEBREW.find(text).is_some()
}

/// Whether the character is a Japanese character.
fn is_japanese(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_SCRIPT_JAPANESE.find(text).is_some()
}

/// Whether the character is a virama.
fn is_virama(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_VIRAMA.find(text).is_some()
}

/// Whether the character is in a permitted general category.
fn is_permitted_category(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_PERMITTED_CATEGORY.find(text).is_some()
}

/// Whether the character is disallowed by RFC 5892.
fn is_rfc5892_disallowed(character: char) -> bool {
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    RE_RFC5892_DISALLOWED.find(text).is_some()
}

/// Whether the value is a valid Unicode (U-label) label.
pub fn is_unicode_label(value: &str) -> bool {
    // RFC 5890 §2.3.2.1: Ensure Max Length
    if exceeds_max_a_label_length(value) {
        return false;
    }
    // RFC 5893 - Bidi Rule (applies whenever this label itself is RTL)
    if has_right_to_left_characters(value) && !satisfies_bidi_rule(value) {
        return false;
    }
    // RFC 5891 §4.2.3.2: Hyphen rules
    if has_invalid_hyphens(value) {
        return false;
    }
    // RFC 5891 §4.2.3.2 - Must not begin with a combining mark
    if let Some(first) = value.chars().next()
        && is_combining_mark(first)
    {
        return false;
    }
    let mut has_japanese = false;
    let mut previous: Option<char> = None;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        let next = characters.peek().copied();
        let code_point = character as u32;
        // 1. DISALLOWED exceptions
        if is_rfc5892_disallowed(character) {
            return false;
        }
        // 1b. General category (PVALID) enforcement
        if !is_permitted_category(character) {
            return false;
        }
        // 2. Collect Flags
        if is_japanese(character) {
            has_japanese = true;
        }
        // 3. CONTEXTO / CONTEXTJ Neighbor Rules
        match code_point {
            // MIDDLE DOT (Catalan)
            0x00b7 => {
                if previous != Some('l') || next != Some('l') {
                    return false;
                }
            }
            // Greek KERAIA
            0x0375 => {
                if !next.is_some_and(is_greek) {
                    return false;
                }
            }
            // Hebrew GERESH
            0x05f3 | 0x05f4 => {
                if !previous.is_some_and(is_hebrew) {
                    return false;
                }
            }
            // ZWNJ - RFC 5892 Appendix A.1
            // Note: Full validation requires a Unicode joining type table. Upstream rejects only when preceded by
            // ASCII (U+0000-U+007F), which can never satisfy the Virama or cursive-joining rules.
            0x200c => {
                let Some(previous) = previous else {
                    return false;
                };
                if (previous as u32) < 0x0080 && !is_virama(previous) {
                    return false;
                }
            }
            // ZWJ: RFC 5892 Appendix A.2
            0x200d => {
                if !previous.is_some_and(is_virama) {
                    return false;
                }
            }
            // KATAKANA MIDDLE DOT, checked at the end
            0x30fb => {}
            _ => {}
        }
        previous = Some(character);
    }
    // 4. Global Context Validations (Post-loop)
    // RFC 5892 Appendix A.7 - Katakana Middle Dot requirement
    if value.contains('\u{30fb}') && !has_japanese {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_labels() {
        assert!(is_unicode_label("bücher"));
        assert!(is_unicode_label("münchen"));
        assert!(is_unicode_label("日本語"));
        assert!(is_unicode_label("مثال"));
        assert!(is_unicode_label("שלום"));
    }

    #[test]
    fn rejects_invalid_hyphens() {
        assert!(!is_unicode_label("-bücher"));
        assert!(!is_unicode_label("bücher-"));
        assert!(!is_unicode_label("bü--cher"));
    }

    #[test]
    fn rejects_a_leading_combining_mark() {
        assert!(!is_unicode_label("\u{0301}abc"));
    }

    #[test]
    fn rejects_a_label_that_exceeds_the_maximum_length() {
        let long = "日本語".repeat(20);
        assert!(!is_unicode_label(&long));
    }

    #[test]
    fn enforces_the_context_rules() {
        // MIDDLE DOT requires two 'l' around it.
        assert!(is_unicode_label("l·l"));
        assert!(!is_unicode_label("l·x"));
        // GREEK KERAIA requires a Greek character after it.
        assert!(is_unicode_label("͵α"));
        assert!(!is_unicode_label("͵a"));
        // HEBREW GERESH requires a Hebrew character before it.
        assert!(is_unicode_label("א׳"));
        assert!(!is_unicode_label("a׳"));
        // ZWNJ may not follow an ASCII character.
        assert!(!is_unicode_label("a\u{200c}b"));
        // ZWJ requires a virama before it.
        assert!(!is_unicode_label("a\u{200d}b"));
    }

    #[test]
    fn enforces_the_katakana_middle_dot_rule() {
        assert!(is_unicode_label("日本語・語"));
        assert!(!is_unicode_label("abc・def"));
    }
}
