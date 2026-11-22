// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).
//
// The upstream pattern ends every domain label with a lookbehind that forbids a trailing hyphen; the label is
// written here without it: a label is a letter or digit, followed by at most 61 letters, digits or hyphens and
// one letter or digit.

use regress::Regex;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

static IDN_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(
        r##"^(?:[A-Za-z0-9!#$%&'*+/=?^_`{|}~\u{0080}-\u{10FFFF}-]+(?:\.[A-Za-z0-9!#$%&'*+/=?^_`{|}~\u{0080}-\u{10FFFF}-]+)*|"(?:[^"\\]|\\.)*")@[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,61}[\p{L}\p{N}])?(?:\.[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,61}[\p{L}\p{N}])?)*$"##,
        "iu",
    )
    .expect("the pattern is valid")
});

/// Whether the value is an internationalized email address (RFC 4952).
pub fn is_idn_email(value: &str) -> bool {
    let normalized: String = value.nfc().collect();
    IDN_EMAIL.find(&normalized).is_some()
}
