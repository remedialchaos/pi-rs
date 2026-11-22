// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

/// Whether the value is a regular expression string pattern (ECMA 262).
pub fn is_regex(value: &str) -> bool {
    crate::regex::is_valid(value)
}
