// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Whether the value is an IPv4 address (RFC 2673).
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(
        r##"^(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)$"##,
        "",
    )
    .expect("the pattern is valid")
});

/// Whether the value has the format.
pub fn is_ipv4(value: &str) -> bool {
    IPV4.find(value).is_some()
}
