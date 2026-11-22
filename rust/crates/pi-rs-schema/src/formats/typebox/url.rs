// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

/// Whether the value is a valid URL (RFC 3986, RFC 3987).
pub fn is_url(value: &str) -> bool {
    url::Url::parse(value).is_ok()
}
