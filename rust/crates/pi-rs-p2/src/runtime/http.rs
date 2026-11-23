use http::{HeaderMap, Method, header};

pub const PROXY_MARKER_HEADER: &str = "x-p2-proxy-secret";

pub fn parse_allowed_hosts(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The historical proxy marker is provenance, never a credential.
pub fn forwarded(headers: &HeaderMap) -> bool {
    headers.contains_key(PROXY_MARKER_HEADER)
        || [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "forwarded",
        ]
        .iter()
        .any(|name| headers.get(*name).is_some_and(|value| !value.is_empty()))
}

pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    headers
        .get(header::ORIGIN)
        .is_none_or(|sent| sent == origin)
}

pub fn foreign_browser_request(headers: &HeaderMap) -> bool {
    let site = headers.get("sec-fetch-site");
    site.is_some_and(|site| site != "same-origin" && site != "none")
        || (site.is_none_or(|site| site != "same-origin")
            && headers.get("sec-fetch-dest").is_some_and(|dest| {
                matches!(
                    dest.to_str(),
                    Ok("iframe" | "frame" | "embed" | "object" | "fencedframe")
                )
            }))
}

pub fn cross_site_read(method: &Method, headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "cross-site")
        && !(matches!(*method, Method::GET | Method::HEAD)
            && headers
                .get("sec-fetch-mode")
                .is_some_and(|mode| mode == "navigate")
            && headers
                .get("sec-fetch-dest")
                .is_some_and(|dest| dest == "document"))
}

pub(crate) fn content_security_policy(nonce: &str, origin: &str) -> String {
    let socket = origin.replacen("http", "ws", 1);
    format!(
        concat!(
            "default-src 'none'; script-src 'self' 'nonce-{}' 'wasm-unsafe-eval'; ",
            "connect-src 'self' {}; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; ",
            "media-src 'self' data: blob:; font-src 'self' data:; worker-src 'self' blob:; ",
            "manifest-src 'self'; frame-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        ),
        nonce, socket
    )
}
