use std::borrow::Cow;

use url::Url;

use crate::error::{Error, Result};
use crate::request::ProviderEnv;

pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

fn proxy_env<'a>(name: &str, env: &'a ProviderEnv) -> Option<Cow<'a, str>> {
    let upper = name.to_ascii_uppercase();
    env.get(name)
        .filter(|s| !s.is_empty())
        .or_else(|| env.get(&upper).filter(|s| !s.is_empty()))
        .map(|s| Cow::Borrowed(s.as_str()))
        .or_else(|| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var(upper).ok().filter(|s| !s.is_empty()))
                .map(Cow::Owned)
        })
}
fn brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}
fn parse_decimal_prefix(value: &str) -> i32 {
    let value = value.trim_start();
    let skip = usize::from(value.starts_with(['+', '-']));
    let digits = value[skip..].bytes().take_while(u8::is_ascii_digit).count();
    value[..skip + digits].parse().unwrap_or_default()
}
fn no_proxy_entry(entry: &str) -> (&str, i32) {
    if let Some((host, rest)) = entry.strip_prefix('[').and_then(|e| e.split_once(']')) {
        return (host, rest.strip_prefix(':').map_or(0, parse_decimal_prefix));
    }
    if let Some((host, digits)) = entry.split_once(':')
        && !digits.contains(':')
    {
        let port = parse_decimal_prefix(digits);
        let started = digits
            .trim_start()
            .trim_start_matches(['+', '-'])
            .starts_with(|c: char| c.is_ascii_digit());
        if port != 0 || started {
            return (host, port);
        }
    }
    (entry, 0)
}
fn is_excluded(url: &Url, no_proxy: &str) -> bool {
    if no_proxy == "*" {
        return true;
    }
    let host = brackets(url.host_str().unwrap_or_default());
    let port = url.port_or_known_default().map_or(
        match url.scheme() {
            "gopher" => 70,
            _ => 0,
        },
        i32::from,
    );
    let normalized = no_proxy.to_ascii_lowercase();
    normalized
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|s| !s.is_empty())
        .any(|entry| {
            let (domain, constraint) = no_proxy_entry(entry);
            if constraint != 0 && constraint != port {
                return false;
            }
            let domain = brackets(domain);
            let domain = domain
                .strip_prefix("*.")
                .or_else(|| domain.strip_prefix(['*', '.']))
                .unwrap_or(domain);
            !domain.is_empty()
                && (host == domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.')))
        })
}

pub fn resolve_http_proxy_url_for_target(target: &str, env: &ProviderEnv) -> Result<Option<Url>> {
    let Ok(url) = Url::parse(target) else {
        return Ok(None);
    };
    if !url.has_host() || proxy_env("no_proxy", env).is_some_and(|list| is_excluded(&url, &list)) {
        return Ok(None);
    }
    let key = format!("{}_proxy", url.scheme());
    let Some(raw) = proxy_env(&key, env).or_else(|| proxy_env("all_proxy", env)) else {
        return Ok(None);
    };
    let raw = if raw.contains("://") {
        raw
    } else {
        Cow::Owned(format!("{}://{raw}", url.scheme()))
    };
    let proxy = Url::parse(&raw)
        .map_err(|_| Error::Protocol(format!("Invalid proxy URL {raw:?}: Invalid URL")))?;
    if !matches!(proxy.scheme(), "http" | "https") {
        return Err(Error::Protocol(format!(
            "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
            proxy.scheme()
        )));
    }
    Ok(Some(proxy))
}
