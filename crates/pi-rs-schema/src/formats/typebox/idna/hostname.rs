// The ASCII hostname rules, ported from `packages/schema/src/formats/typebox/idna/hostname.ts`.

use super::label::ascii::is_ascii_label;
use super::label::puny::is_puny_label;

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

/// Whether the label is either a valid ACE label or a valid ASCII label.
fn is_label(value: &str) -> bool {
    is_valid_label_length(value) && (is_puny_label(value) || is_ascii_label(value))
}

/// Whether the value is a valid ASCII hostname.
pub fn is_hostname(value: &str) -> bool {
    if value.is_empty() || utf16_length(value) > MAX_HOSTNAME_LENGTH {
        return false;
    }
    if value.ends_with('.') {
        return false;
    }
    value.split('.').all(is_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hostnames() {
        assert!(is_hostname("example.com"));
        assert!(is_hostname("example"));
        assert!(is_hostname("a.b.c.d.example.com"));
        assert!(is_hostname("xn--bcher-kva.example.com"));
    }

    #[test]
    fn rejects_an_empty_or_trailing_dot_hostname() {
        assert!(!is_hostname(""));
        assert!(!is_hostname("example.com."));
    }

    #[test]
    fn rejects_an_overlong_hostname() {
        let label = "a".repeat(63);
        let hostname = std::iter::repeat_n(label.as_str(), 4)
            .collect::<Vec<_>>()
            .join(".");
        assert!(hostname.len() > MAX_HOSTNAME_LENGTH);
        assert!(!is_hostname(&hostname));
    }

    #[test]
    fn rejects_an_overlong_label() {
        let label = "a".repeat(64);
        assert!(!is_hostname(&format!("{label}.example.com")));
    }

    #[test]
    fn rejects_an_empty_label() {
        assert!(!is_hostname("example..com"));
        assert!(!is_hostname(".example.com"));
    }
}
