// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Whether the value is a JSON pointer (RFC 6901).
static JSON_POINTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(r##"^(?:\/(?:[^~/]|~0|~1)*)*$"##, "").expect("the pattern is valid")
});

/// Whether the value has the format.
pub fn is_json_pointer(value: &str) -> bool {
    JSON_POINTER.find(value).is_some()
}
