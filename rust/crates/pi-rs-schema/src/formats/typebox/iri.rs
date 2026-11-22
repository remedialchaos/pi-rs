// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// The longest input the IPvFuture address is narrowed in.
const IPV_FUTURE_MAX_LENGTH: usize = 2048;

static IPV_FUTURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[vV][0-9a-fA-F]+\.[^\]]+\]").expect("the pattern is valid"));
static INVALID_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x20<>^`{|}\\]").expect("the pattern is valid"));
static INVALID_PERCENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"%(?![0-9a-fA-F]{2})").expect("the pattern is valid"));

/// Substitutes an IPvFuture address with a standard IPv6 loopback address: the URL parser rejects IPvFuture
/// literals, which are otherwise valid in RFC 3987.
fn narrow_ipv_future(value: &str) -> String {
    if value.len() >= IPV_FUTURE_MAX_LENGTH {
        return value.to_string();
    }
    IPV_FUTURE.replace_all(value, "[::1]")
}

/// Whether the value is a valid Internationalized Resource Identifier (RFC 3987).
pub fn is_iri(value: &str) -> bool {
    if INVALID_CHARS.find(value).is_some() {
        return false;
    }
    if INVALID_PERCENT.find(value).is_some() {
        return false;
    }
    url::Url::parse(&narrow_ipv_future(value)).is_ok()
}
