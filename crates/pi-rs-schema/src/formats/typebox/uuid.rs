// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Whether the value is a UUID (RFC 4122, RFC 9562).
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(r##"^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$"##, "i")
        .expect("the pattern is valid")
});

/// Whether the value has the format.
pub fn is_uuid(value: &str) -> bool {
    UUID.find(value).is_some()
}
