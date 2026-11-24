use http::{HeaderMap, Method, header};
use pi_rs_chord::node::http::{AdmitLink, LinkAdmission};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::HashSet,
    sync::{Arc, Mutex},
};

use super::{
    logs::{LogLevel, process_logs},
    principals::{OwnerAccess, Principal},
};

#[cfg(test)]
mod tests;

pub const PROXY_MARKER_HEADER: &str = "x-p2-proxy-secret";

pub(crate) fn browser_links(base: Option<&'static str>) -> AdmitLink {
    let reported = Mutex::new(HashSet::new());
    Arc::new(move |request| {
        let Some(principal) = &request.principal else {
            return LinkAdmission::Reject(http::StatusCode::FORBIDDEN);
        };
        let Some(stale) = stale_page(&request.query, base) else {
            return LinkAdmission::Attach;
        };
        let who = match Principal::deserialize(principal) {
            Ok(Principal::Owner {
                access: OwnerAccess::Device { device },
            }) => format!("the owner's device {device}"),
            Ok(Principal::Owner {
                access: OwnerAccess::Local,
            }) => "the owner (local)".to_owned(),
            Ok(Principal::Owner {
                access: OwnerAccess::Relay,
            }) => "the owner (relay)".to_owned(),
            Ok(Principal::Guest { name, id, .. }) => format!("guest {name} ({id})"),
            Ok(Principal::Worker { session, .. }) => format!("session {session}'s worker"),
            _ => principal["kind"].as_str().unwrap_or("unknown").to_owned(),
        };
        // Node exposes incoming header bytes as Latin-1 code units.
        let agent = request.headers.get(header::USER_AGENT).map_or_else(
            || "unknown browser".to_owned(),
            |header| {
                header
                    .as_bytes()
                    .iter()
                    .take(200)
                    .map(|&b| char::from(b))
                    .collect()
            },
        );
        let key = format!(
            "{who}\n{agent}\n{}",
            stale.sent.as_deref().unwrap_or("null")
        );
        if reported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
        {
            let sent = stale
                .sent
                .as_deref()
                .unwrap_or("unnamed: from before 2026-11-12");
            let current = base.unwrap_or("not built");
            let remote = request.remote_address.map_or_else(
                || "unknown address".to_owned(),
                |addr| addr.ip().to_string(),
            );
            let text = format!(
                "A page running an older p2 (web bundle {sent}, current {current}) tried to connect and was told to reload: {who}, {agent}, from {remote} (WebSocket /chord)."
            );
            process_logs().append("stale-pages", LogLevel::Event, &text);
        }
        LinkAdmission::Close(axum::extract::ws::CloseFrame {
            code: 4001,
            reason: "p2: stale page, reload".into(),
        })
    })
}

struct StalePage<'a> {
    sent: Option<Cow<'a, str>>,
}

/// An old browser reloads on this close frame. Named non-browser clients may omit the base.
fn stale_page<'a>(query: &'a str, base: Option<&str>) -> Option<StalePage<'a>> {
    let params = form_urlencoded::parse(query.as_bytes());
    let first = |key: &str| {
        params
            .clone()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    };
    let sent = first("web");
    let is_current = match sent.as_deref() {
        Some(sent) => base.is_none_or(|base| sent == base),
        None => matches!(first("client").as_deref(), Some("tui" | "cloud" | "script")),
    };
    (!is_current).then_some(StalePage { sent })
}

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
