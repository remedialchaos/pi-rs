// The Bidi Rule (RFC 5893), ported from `packages/schema/src/formats/typebox/idna/format/bidi.ts`.
//
// The Bidi class mapping upstream uses is deliberately approximate: a code point is classified with a handful
// of regular expressions, and everything that is not matched is "ON". The mapping is reproduced here as it is,
// character by character.

use super::super::pattern::pattern::{
    RE_ARABIC_INDIC_DIGIT, RE_EUROPEAN_NUMBER, RE_LETTER, RE_MARK_NONSPACING,
    RE_SCRIPT_ARABIC_LETTER, RE_SCRIPT_HEBREW,
};
use super::puny::{decode, is_ace_prefixed};
use regress::Regex;
use std::sync::LazyLock;

/// The Bidi classes a right-to-left label may contain.
const SOURCE_RTL_ALLOWED: &str = r"^(?:R|AL|AN|EN|ES|CS|ET|ON|BN|NSM)$";
/// The Bidi classes a left-to-right label may contain.
const SOURCE_LTR_ALLOWED: &str = r"^(?:L|EN|ES|CS|ET|ON|BN|NSM)$";
/// The Bidi classes that make a label right-to-left.
const SOURCE_RTL_CLASSES: &str = r"^(?:R|AL|AN)$";

/// Compiles a pattern source.
fn compile(source: &str) -> Regex {
    // INFALLIBLE: the sources are compile-time constants, and the unit tests assert that they compile.
    Regex::with_flags(source, "").expect("static regular expression must compile")
}

/// The Bidi classes a right-to-left label may contain.
static RE_RTL_ALLOWED: LazyLock<Regex> = LazyLock::new(|| compile(SOURCE_RTL_ALLOWED));
/// The Bidi classes a left-to-right label may contain.
static RE_LTR_ALLOWED: LazyLock<Regex> = LazyLock::new(|| compile(SOURCE_LTR_ALLOWED));
/// The Bidi classes that make a label right-to-left.
static RE_RTL_CLASSES: LazyLock<Regex> = LazyLock::new(|| compile(SOURCE_RTL_CLASSES));

/// The Bidi class of a code point, upstream's approximate mapping.
fn get_bidi_class(character: char) -> &'static str {
    // The patterns are matched against the single code point, without allocating.
    let mut buffer = [0u8; 4];
    let text = character.encode_utf8(&mut buffer);
    if RE_EUROPEAN_NUMBER.find(text).is_some() {
        "EN"
    } else if RE_ARABIC_INDIC_DIGIT.find(text).is_some() {
        "AN"
    } else if RE_MARK_NONSPACING.find(text).is_some() {
        "NSM"
    } else if RE_SCRIPT_HEBREW.find(text).is_some() {
        "R"
    } else if RE_SCRIPT_ARABIC_LETTER.find(text).is_some() {
        "AL"
    } else if RE_LETTER.find(text).is_some() {
        "L"
    } else {
        "ON"
    }
}

/// Whether the value contains at least one right-to-left character, per RFC 5893 §1.4.
pub fn has_right_to_left_characters(value: &str) -> bool {
    value
        .chars()
        .any(|character| RE_RTL_CLASSES.find(get_bidi_class(character)).is_some())
}

/// Whether the value contains Bidi characters, decoding an ACE label to its Unicode form first.
pub fn has_bidi_chars(value: &str) -> bool {
    if is_ace_prefixed(value) {
        match decode(&value[4..].to_lowercase()) {
            Ok(decoded) => has_right_to_left_characters(&decoded),
            Err(_) => false,
        }
    } else {
        has_right_to_left_characters(value)
    }
}

/// Whether the value satisfies the Bidi Rule (RFC 5893 §2).
pub fn satisfies_bidi_rule(value: &str) -> bool {
    let mut is_rtl = false;
    let mut allowed: &Regex = &RE_LTR_ALLOWED;
    let mut saw_european_number = false;
    let mut saw_arabic_number = false;
    let mut is_first = true;
    for character in value.chars() {
        let bidi_class = get_bidi_class(character);
        if is_first {
            // 1. The first character must be a character with Bidi property L, R, or AL.
            if bidi_class != "L" && bidi_class != "R" && bidi_class != "AL" {
                return false;
            }
            is_rtl = bidi_class == "R" || bidi_class == "AL";
            allowed = if is_rtl {
                &RE_RTL_ALLOWED
            } else {
                &RE_LTR_ALLOWED
            };
            is_first = false;
        }
        // 2 / 5. Only characters in the allowed set for this direction may appear.
        if allowed.find(bidi_class).is_none() {
            return false;
        }
        if bidi_class == "EN" {
            saw_european_number = true;
        } else if bidi_class == "AN" {
            saw_arabic_number = true;
        }
    }
    // 4. If an EN is present, no AN may be present, and vice versa.
    !(is_rtl && saw_european_number && saw_arabic_number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_right_to_left_characters() {
        assert!(has_right_to_left_characters("אבג"));
        assert!(has_right_to_left_characters("مثال"));
        assert!(has_right_to_left_characters("aمثال"));
        assert!(!has_right_to_left_characters("example"));
        assert!(!has_right_to_left_characters("bücher"));
    }

    #[test]
    fn detects_bidi_characters_in_an_ace_label() {
        assert!(has_bidi_chars("xn--4dbc0a"));
        assert!(has_bidi_chars("אבג"));
        assert!(!has_bidi_chars("xn--bcher-kva"));
        assert!(!has_bidi_chars("xn--not-valid-punycode!"));
    }

    #[test]
    fn satisfies_the_bidi_rule() {
        assert!(satisfies_bidi_rule("example"));
        assert!(satisfies_bidi_rule("אבג"));
        assert!(!satisfies_bidi_rule("1example"));
        assert!(!satisfies_bidi_rule("אa"));
        // An Arabic-Indic digit and an ASCII digit may not both appear in a right-to-left label.
        assert!(!satisfies_bidi_rule("א1٣"));
    }
}
