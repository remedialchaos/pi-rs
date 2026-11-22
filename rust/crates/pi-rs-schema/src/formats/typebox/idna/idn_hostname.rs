// The IDN hostname rules, ported from `packages/schema/src/formats/typebox/idna/idn-hostname.ts`.

use super::format::bidi::{has_bidi_chars, satisfies_bidi_rule};
use super::label::puny::is_puny_label;
use super::label::unicode::is_unicode_label;
use unicode_normalization::UnicodeNormalization;

/// The maximum length of a hostname.
const MAX_HOSTNAME_LENGTH: usize = 253;
/// The maximum length of a label.
const MAX_LABEL_LENGTH: usize = 63;

/// The length of a value in UTF-16 code units, which is what JavaScript's `String.length` counts.
fn utf16_length(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        value.chars().map(char::len_utf16).sum()
    }
}

/// Whether the label has a valid length.
fn is_valid_label_length(value: &str) -> bool {
    let length = utf16_length(value);
    length > 0 && length <= MAX_LABEL_LENGTH
}

/// Whether the label is either a valid ACE label or a valid Unicode label.
fn is_label(value: &str) -> bool {
    is_valid_label_length(value) && (is_puny_label(value) || is_unicode_label(value))
}

/// Whether the character is mapped to an ASCII character by the fullwidth mapping.
fn is_fullwidth_mappable(character: char) -> bool {
    ('\u{ff01}'..='\u{ff5e}').contains(&character)
}

/// Whether the character is one of the code points IDNA mapping ignores.
fn is_ignored(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{180b}'..='\u{180d}'
            | '\u{200b}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// Whether the character is one of the full stop variants.
fn is_full_stop(character: char) -> bool {
    matches!(character, '\u{002e}' | '\u{3002}' | '\u{ff0e}' | '\u{ff61}')
}

/// Normalizes a hostname for label validation.
///
/// Maps fullwidth code points to their ASCII equivalent (e.g. 'ａ' U+FF41 maps to 'a'), then normalizes for
/// NFC such that equivalent code point sequences can be uniformly compared. It also removes the code points
/// that IDNA mapping ignores, and converts the full stop variants to ASCII '.' so the hostname can be split
/// into labels.
pub fn normalize_hostname(value: &str) -> String {
    let mapped = value
        .chars()
        .map(|character| {
            if is_fullwidth_mappable(character) {
                // The fullwidth range maps down onto the ASCII range, so the result is always a valid scalar.
                char::from_u32(character as u32 - 0xfee0).unwrap_or(character)
            } else {
                character
            }
        })
        .collect::<String>()
        .nfc()
        .collect::<String>();
    let ignored = mapped.chars().filter(|character| !is_ignored(*character));
    ignored
        .map(|character| {
            if is_full_stop(character) {
                '.'
            } else {
                character
            }
        })
        .collect()
}

/// Whether the value is a valid IDN hostname.
pub fn is_idn_hostname(value: &str) -> bool {
    if value.is_empty() || value.contains(' ') {
        return false;
    }
    let normalized = normalize_hostname(value);
    if utf16_length(&normalized) > MAX_HOSTNAME_LENGTH {
        return false;
    }
    let labels: Vec<&str> = normalized.split('.').collect();
    let contains_bidi_chars = labels.iter().any(|label| has_bidi_chars(label));
    labels
        .iter()
        .all(|label| is_label(label) && (!contains_bidi_chars || satisfies_bidi_rule(label)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_idn_hostnames() {
        assert!(is_idn_hostname("example.com"));
        assert!(is_idn_hostname("bücher.example.com"));
        assert!(is_idn_hostname("日本語.example.com"));
        assert!(is_idn_hostname("xn--bcher-kva.example.com"));
    }

    #[test]
    fn rejects_an_empty_or_spaced_hostname() {
        assert!(!is_idn_hostname(""));
        assert!(!is_idn_hostname("example .com"));
    }

    #[test]
    fn normalizes_the_fullwidth_and_full_stop_variants() {
        assert_eq!(normalize_hostname("ｅｘａｍｐｌｅ"), "example");
        assert_eq!(normalize_hostname("example\u{3002}com"), "example.com");
        assert_eq!(normalize_hostname("example\u{ff61}com"), "example.com");
        assert_eq!(normalize_hostname("example\u{ff0e}com"), "example.com");
        assert_eq!(normalize_hostname("example\u{00ad}.com"), "example.com");
    }

    #[test]
    fn accepts_a_hostname_with_the_fullwidth_variants() {
        assert!(is_idn_hostname("ｅｘａｍｐｌｅ.com"));
        assert!(is_idn_hostname("example。com"));
    }
}
