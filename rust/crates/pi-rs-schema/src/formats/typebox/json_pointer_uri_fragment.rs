// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Whether the value is a JSON pointer URI fragment (RFC 6901).
static JSON_POINTER_URI_FRAGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(
        r##"^#(?:\/(?:[a-z0-9_\-.!$&'()*+,;:=@]|%[0-9a-f]{2}|~0|~1)*)*$"##,
        "i",
    )
    .expect("the pattern is valid")
});

/// Whether the value has the format.
pub fn is_json_pointer_uri_fragment(value: &str) -> bool {
    JSON_POINTER_URI_FRAGMENT.find(value).is_some()
}
