// The regular expressions the IDNA/hostname validators are built from, ported from
// `packages/schema/src/formats/typebox/idna/pattern/pattern.ts`.
//
// The upstream patterns are ECMAScript regular expressions. They are compiled here with `regress`, a port of
// V8's Irregexp, so their semantics (including the lookarounds and the `\p{...}` property escapes) match
// the upstream ones exactly.
//
// Two patterns upstream compiles without the `u` flag (`RE_RULE_HYPHEN_PLACEMENT` and `RE_RULE_NOT_RESERVED_ACE`)
// are compiled here without it as well; every other pattern keeps the flag.

use regress::Regex;

// -------------------------------------------------------------------
// Pattern sources
//
// The sources are kept as constants because the composite patterns upstream are built by joining the `.source`
// of several patterns with `|`.
// -------------------------------------------------------------------
const SOURCE_RULE_HYPHEN_PLACEMENT: &str = r"^(?!-).*(?<!-)$";
const SOURCE_RULE_NOT_RESERVED_ACE: &str = r"^(?!..--)";
const SOURCE_ASCII_LDH: &str = r"^[a-zA-Z0-9-]*$";
const SOURCE_ASCII: &str = r"^\p{ASCII}*$";
const SOURCE_NON_ASCII: &str = r"[^\p{ASCII}]";
const SOURCE_HIGH_SURROGATE: &str = r"[\u{d800}-\u{dbff}]";
const SOURCE_REGIONAL_INDICATOR: &str = r"[\u{1f1e6}-\u{1f1ff}]";
const SOURCE_VARIATION_SELECTOR: &str = r"[\u{fe00}-\u{fe0f}]";
const SOURCE_ZERO_WIDTH_JOINER: &str = r"\u{200d}";
const SOURCE_ASCII_DIGIT: &str = r"[0-9]";
const SOURCE_ARABIC_INDIC_DIGIT: &str = r"[\u{0660}-\u{0669}]";
const SOURCE_EXT_ARABIC_INDIC_DIGIT: &str = r"[\u{06f0}-\u{06f9}]";
const SOURCE_COMMON_SEPARATOR: &str = r"[\u{002e}\u{002c}\u{003a}\u{002f}]";
const SOURCE_EUROPEAN_SEPARATOR: &str = r"[\u{002d}\u{002b}]";
const SOURCE_MARK_NONSPACING: &str = r"\p{Mn}";
const SOURCE_MARK_SPACING_COMBINING: &str = r"\p{Mc}";
const SOURCE_MARK_ENCLOSING: &str = r"\p{Me}";
const SOURCE_COMBINING_MARK: &str = r"[\p{Mn}\p{Mc}\p{Me}]";
const SOURCE_LETTER: &str = r"\p{L}";
const SOURCE_NUMBER_DECIMAL: &str = r"\p{Nd}";
const SOURCE_SCRIPT_GREEK: &str = r"\p{Script=Greek}";
const SOURCE_SCRIPT_HEBREW: &str = r"\p{Script=Hebrew}";
const SOURCE_SCRIPT_JAPANESE: &str = r"[\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Han}]";
const SOURCE_SCRIPT_ARABIC_LETTER: &str =
    r"[\p{Script=Arabic}\p{Script=Syriac}\p{Script=Thaana}\p{Script=Mandaic}]";
const SOURCE_VIRAMA: &str = r"[\u{094d}\u{09cd}\u{0a4d}\u{0acd}\u{0b4d}\u{0bcd}\u{0c4d}\u{0ccd}\u{0d3b}\u{0d3c}\u{0d4d}\u{0dca}\u{1b44}\u{1baa}\u{1bab}\u{a9c0}\u{11046}\u{1107f}\u{110b9}\u{11133}\u{11134}\u{111c0}\u{11235}\u{1134d}\u{11442}\u{114c2}\u{115bf}\u{1163f}\u{116b6}\u{11c3f}\u{11d44}\u{11d45}]";
const SOURCE_RFC5892_DISALLOWED: &str =
    r"[\u{0640}\u{07fa}\u{302e}\u{302f}\u{3031}\u{3032}\u{3033}\u{3034}\u{3035}\u{303b}]";
const SOURCE_CONTEXTO_EXCEPTIONS: &str =
    r"[\u{00b7}\u{0375}\u{05f3}\u{05f4}\u{200c}\u{200d}\u{30fb}]";
const SOURCE_PVALID_EXCEPTIONS: &str = r"[\u{00df}\u{03c2}\u{06fd}\u{06fe}\u{0f0b}\u{3007}]";

// The composite sources, upstream: `new RegExp([...].map((regexp) => regexp.source).join("|"), "u")`.
const SOURCE_EUROPEAN_NUMBER: &str = concat!(r"[0-9]", "|", r"[\u{06f0}-\u{06f9}]");
const SOURCE_PERMITTED_CATEGORY: &str = concat!(
    r"\p{L}",
    "|",
    r"[\u{002d}\u{002b}]",
    "|",
    r"[\u{002e}\u{002c}\u{003a}\u{002f}]",
    "|",
    r"\p{Nd}",
    "|",
    r"\p{Mn}",
    "|",
    r"\p{Mc}",
    "|",
    r"[\u{00b7}\u{0375}\u{05f3}\u{05f4}\u{200c}\u{200d}\u{30fb}]",
    "|",
    r"[\u{00df}\u{03c2}\u{06fd}\u{06fe}\u{0f0b}\u{3007}]",
);

/// Compiles a pattern source with the given ECMAScript flags.
fn compile(source: &str, flags: &str) -> Regex {
    // INFALLIBLE: every source is a compile-time constant, and the unit tests assert that they all compile.
    Regex::with_flags(source, flags).expect("static regular expression must compile")
}

/// Declares a compiled pattern.
macro_rules! pattern {
    ($(#[$meta:meta])* $name:ident, $source:expr, $flags:expr) => {
        $(#[$meta])*
        pub static $name: std::sync::LazyLock<regress::Regex> =
            std::sync::LazyLock::new(|| compile($source, $flags));
    };
}

// -------------------------------------------------------------------
// Validation Rules
// -------------------------------------------------------------------
pattern!(
    /// RFC 5891 §4.2.3.2: the label does not start or end with a hyphen.
    RE_RULE_HYPHEN_PLACEMENT,
    SOURCE_RULE_HYPHEN_PLACEMENT,
    ""
);
pattern!(
    /// RFC 5891 §4.2.3.3: the label does not have hyphens in the third and fourth positions.
    RE_RULE_NOT_RESERVED_ACE,
    SOURCE_RULE_NOT_RESERVED_ACE,
    ""
);
pattern!(
    /// The label consists only of ASCII letters, digits and hyphens.
    RE_ASCII_LDH,
    SOURCE_ASCII_LDH,
    ""
);

// -------------------------------------------------------------------
// ASCII & Encoding
// -------------------------------------------------------------------
pattern!(
    /// The value consists only of ASCII characters.
    RE_ASCII,
    SOURCE_ASCII,
    "u"
);
pattern!(
    /// The value contains at least one non-ASCII character.
    RE_NON_ASCII,
    SOURCE_NON_ASCII,
    "u"
);

// -------------------------------------------------------------------
// Structural & Unicode Controls
// -------------------------------------------------------------------
pattern!(
    /// The value contains a high surrogate.
    RE_HIGH_SURROGATE,
    SOURCE_HIGH_SURROGATE,
    "u"
);
pattern!(
    /// The value contains a regional indicator symbol.
    RE_REGIONAL_INDICATOR,
    SOURCE_REGIONAL_INDICATOR,
    "u"
);
pattern!(
    /// The value contains a variation selector.
    RE_VARIATION_SELECTOR,
    SOURCE_VARIATION_SELECTOR,
    "u"
);
pattern!(
    /// The value contains a zero width joiner.
    RE_ZERO_WIDTH_JOINER,
    SOURCE_ZERO_WIDTH_JOINER,
    "u"
);

// -------------------------------------------------------------------
// Numbers & Digits
// -------------------------------------------------------------------
pattern!(
    /// The value contains an ASCII digit.
    RE_ASCII_DIGIT,
    SOURCE_ASCII_DIGIT,
    ""
);
pattern!(
    /// The value contains an Arabic-Indic digit.
    RE_ARABIC_INDIC_DIGIT,
    SOURCE_ARABIC_INDIC_DIGIT,
    "u"
);
pattern!(
    /// The value contains an extended Arabic-Indic digit.
    RE_EXT_ARABIC_INDIC_DIGIT,
    SOURCE_EXT_ARABIC_INDIC_DIGIT,
    "u"
);

// -------------------------------------------------------------------
// Separators
// -------------------------------------------------------------------
pattern!(
    /// The value contains a common separator.
    RE_COMMON_SEPARATOR,
    SOURCE_COMMON_SEPARATOR,
    "u"
);
pattern!(
    /// The value contains a European separator.
    RE_EUROPEAN_SEPARATOR,
    SOURCE_EUROPEAN_SEPARATOR,
    "u"
);

// -------------------------------------------------------------------
// General Categories
// -------------------------------------------------------------------
pattern!(
    /// The value contains a non-spacing mark.
    RE_MARK_NONSPACING,
    SOURCE_MARK_NONSPACING,
    "u"
);
pattern!(
    /// The value contains a spacing combining mark.
    RE_MARK_SPACING_COMBINING,
    SOURCE_MARK_SPACING_COMBINING,
    "u"
);
pattern!(
    /// The value contains an enclosing mark.
    RE_MARK_ENCLOSING,
    SOURCE_MARK_ENCLOSING,
    "u"
);
pattern!(
    /// The value contains a combining mark of any kind.
    RE_COMBINING_MARK,
    SOURCE_COMBINING_MARK,
    "u"
);
pattern!(
    /// The value contains a letter.
    RE_LETTER,
    SOURCE_LETTER,
    "u"
);
pattern!(
    /// The value contains a decimal number.
    RE_NUMBER_DECIMAL,
    SOURCE_NUMBER_DECIMAL,
    "u"
);

// -------------------------------------------------------------------
// Scripts
// -------------------------------------------------------------------
pattern!(
    /// The value contains a Greek character.
    RE_SCRIPT_GREEK,
    SOURCE_SCRIPT_GREEK,
    "u"
);
pattern!(
    /// The value contains a Hebrew character.
    RE_SCRIPT_HEBREW,
    SOURCE_SCRIPT_HEBREW,
    "u"
);
pattern!(
    /// The value contains a Japanese character.
    RE_SCRIPT_JAPANESE,
    SOURCE_SCRIPT_JAPANESE,
    "u"
);
pattern!(
    /// The value contains an Arabic, Syriac, Thaana or Mandaic character.
    RE_SCRIPT_ARABIC_LETTER,
    SOURCE_SCRIPT_ARABIC_LETTER,
    "u"
);

// -------------------------------------------------------------------
// RFC Specifications & Context Exceptions
// -------------------------------------------------------------------
pattern!(
    /// The value contains a virama.
    RE_VIRAMA,
    SOURCE_VIRAMA,
    "u"
);
pattern!(
    /// The value contains a code point RFC 5892 disallows.
    RE_RFC5892_DISALLOWED,
    SOURCE_RFC5892_DISALLOWED,
    "u"
);
pattern!(
    /// The value contains a code point with a context rule.
    RE_CONTEXTO_EXCEPTIONS,
    SOURCE_CONTEXTO_EXCEPTIONS,
    "u"
);
pattern!(
    /// The value contains a code point that is valid despite its general category.
    RE_PVALID_EXCEPTIONS,
    SOURCE_PVALID_EXCEPTIONS,
    "u"
);

// -------------------------------------------------------------------
// Composite Patterns
// -------------------------------------------------------------------
pattern!(
    /// The value contains an ASCII digit or an extended Arabic-Indic digit.
    RE_EUROPEAN_NUMBER,
    SOURCE_EUROPEAN_NUMBER,
    "u"
);
pattern!(
    /// The value contains a code point with a permitted general category.
    RE_PERMITTED_CATEGORY,
    SOURCE_PERMITTED_CATEGORY,
    "u"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_sources_are_the_joined_constituent_sources() {
        // Upstream builds the composite patterns by joining the `.source` of their constituents with `|`.
        assert_eq!(
            SOURCE_EUROPEAN_NUMBER,
            [SOURCE_ASCII_DIGIT, SOURCE_EXT_ARABIC_INDIC_DIGIT].join("|")
        );
        assert_eq!(
            SOURCE_PERMITTED_CATEGORY,
            [
                SOURCE_LETTER,
                SOURCE_EUROPEAN_SEPARATOR,
                SOURCE_COMMON_SEPARATOR,
                SOURCE_NUMBER_DECIMAL,
                SOURCE_MARK_NONSPACING,
                SOURCE_MARK_SPACING_COMBINING,
                SOURCE_CONTEXTO_EXCEPTIONS,
                SOURCE_PVALID_EXCEPTIONS,
            ]
            .join("|")
        );
    }

    #[test]
    fn every_pattern_compiles() {
        assert!(RE_RULE_HYPHEN_PLACEMENT.find("example").is_some());
        assert!(RE_RULE_NOT_RESERVED_ACE.find("example").is_some());
        assert!(RE_ASCII_LDH.find("example").is_some());
        assert!(RE_ASCII.find("example").is_some());
        assert!(RE_NON_ASCII.find("bücher").is_some());
        // A lone surrogate cannot be represented in a Rust string, so the pattern is only checked for the
        // values it does not match.
        assert!(RE_HIGH_SURROGATE.find("example").is_none());
        assert!(RE_REGIONAL_INDICATOR.find("\u{1f1e6}").is_some());
        assert!(RE_VARIATION_SELECTOR.find("\u{fe00}").is_some());
        assert!(RE_ZERO_WIDTH_JOINER.find("\u{200d}").is_some());
        assert!(RE_ASCII_DIGIT.find("7").is_some());
        assert!(RE_ARABIC_INDIC_DIGIT.find("\u{0660}").is_some());
        assert!(RE_EXT_ARABIC_INDIC_DIGIT.find("\u{06f0}").is_some());
        assert!(RE_COMMON_SEPARATOR.find("/").is_some());
        assert!(RE_EUROPEAN_SEPARATOR.find("-").is_some());
        assert!(RE_MARK_NONSPACING.find("\u{0301}").is_some());
        assert!(RE_MARK_SPACING_COMBINING.find("\u{0903}").is_some());
        assert!(RE_MARK_ENCLOSING.find("\u{20dd}").is_some());
        assert!(RE_COMBINING_MARK.find("\u{0301}").is_some());
        assert!(RE_LETTER.find("a").is_some());
        assert!(RE_NUMBER_DECIMAL.find("7").is_some());
        assert!(RE_SCRIPT_GREEK.find("α").is_some());
        assert!(RE_SCRIPT_HEBREW.find("א").is_some());
        assert!(RE_SCRIPT_JAPANESE.find("あ").is_some());
        assert!(RE_SCRIPT_ARABIC_LETTER.find("ع").is_some());
        assert!(RE_VIRAMA.find("\u{094d}").is_some());
        assert!(RE_RFC5892_DISALLOWED.find("\u{0640}").is_some());
        assert!(RE_CONTEXTO_EXCEPTIONS.find("\u{200c}").is_some());
        assert!(RE_PVALID_EXCEPTIONS.find("ß").is_some());
        assert!(RE_EUROPEAN_NUMBER.find("7").is_some());
        assert!(RE_PERMITTED_CATEGORY.find("a").is_some());
    }

    #[test]
    fn hyphen_placement_rejects_leading_and_trailing_hyphens() {
        assert!(RE_RULE_HYPHEN_PLACEMENT.find("example").is_some());
        assert!(RE_RULE_HYPHEN_PLACEMENT.find("-example").is_none());
        assert!(RE_RULE_HYPHEN_PLACEMENT.find("example-").is_none());
    }

    #[test]
    fn reserved_ace_rejects_the_xn_prefix() {
        assert!(RE_RULE_NOT_RESERVED_ACE.find("example").is_some());
        assert!(RE_RULE_NOT_RESERVED_ACE.find("xn--example").is_none());
    }
}
