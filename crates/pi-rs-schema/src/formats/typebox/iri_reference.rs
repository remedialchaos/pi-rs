// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Rejects unencoded spaces, control characters, backslashes and malformed percent encodings.
static INVALID_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x20\x7F\\]").expect("the pattern is valid"));
static INVALID_PERCENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"%(?![0-9a-fA-F]{2})").expect("the pattern is valid"));
/// Detects a scheme without a colon, such as "httpx//example.com".
static MALFORMED_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9+\-.]*//").expect("the pattern is valid"));

/// Whether the value is an IRI reference (RFC 3987).
pub fn is_iri_reference(value: &str) -> bool {
    if INVALID_CHARS.find(value).is_some() {
        return false;
    }
    if INVALID_PERCENT.find(value).is_some() {
        return false;
    }
    if MALFORMED_SCHEME.find(value).is_some() {
        return false;
    }
    let base = url::Url::parse("http://example.com").ok();
    url::Url::options()
        .base_url(base.as_ref())
        .parse(value)
        .is_ok()
}
