//! Embedded browser assets, sign-in and the authenticated application shell.

use std::sync::Arc;

use axum::body::Body;
use http::{HeaderValue, StatusCode, header};
use pi_rs_chord::node::{
    app::new_token,
    http::{
        HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions,
        html_response, json_response, text_response,
    },
};
use serde::Deserialize;

use crate::runtime::{
    http::{content_security_policy, foreign_browser_request},
    principals::Principal,
};

const MANIFEST: &[u8] = include_bytes!("../../assets/public/manifest.webmanifest");
const TOUCH_ICON: &[u8] = include_bytes!("../../assets/public/icons/apple-touch-icon.png");
const PUBLIC_FILES: &[(&str, &str, &[u8])] = &[
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        MANIFEST,
    ),
    (
        "/icons/favicon.svg",
        "image/svg+xml",
        include_bytes!("../../assets/public/icons/favicon.svg"),
    ),
    ("/icons/apple-touch-icon.png", "image/png", TOUCH_ICON),
    (
        "/icons/icon-192.png",
        "image/png",
        include_bytes!("../../assets/public/icons/icon-192.png"),
    ),
    (
        "/icons/icon-512.png",
        "image/png",
        include_bytes!("../../assets/public/icons/icon-512.png"),
    ),
    ("/apple-touch-icon.png", "image/png", TOUCH_ICON),
    ("/apple-touch-icon-precomposed.png", "image/png", TOUCH_ICON),
    (
        "/sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/public/sw.js"),
    ),
];

const SIGN_IN: &str = include_str!("../../assets/sign-in.html");
const APP: &str = include_str!("../../assets/app.html");
const WEB_PREFIX: &str = "/_chord/web/";

struct BrowserBundle {
    base: &'static str,
    files: &'static [(&'static str, &'static str, &'static [u8])],
}

include!(concat!(env!("OUT_DIR"), "/browser_bundle.rs"));

pub fn browser_base() -> Option<&'static str> {
    BROWSER_BUNDLE.map(|bundle| bundle.base)
}

fn viewer(request: &HttpRequest) -> HttpResult<Option<Principal>> {
    request
        .principal
        .as_ref()
        .map(|value| match Principal::deserialize(value) {
            Ok(principal @ (Principal::Owner { .. } | Principal::Guest { .. })) => Ok(principal),
            _ => Err(HttpError::new(403, "Forbidden")),
        })
        .transpose()
}

fn session_page(path: &str) -> Option<&str> {
    let path = path.strip_prefix("/s/")?;
    let session = if let Some((session, conversation)) = path.split_once("/c/") {
        if !matches!(conversation.as_bytes().first(), Some(b'1'..=b'9'))
            || !conversation.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        session
    } else {
        path
    };
    (!session.is_empty()
        && session
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-')))
    .then_some(session)
}

fn app_page(request: &HttpRequest, nonce: &str) -> HttpResult<HttpResponse> {
    let Some(viewer) = viewer(request)? else {
        return sign_in(request);
    };
    let path = request.path.as_str();
    let session = session_page(path);
    if viewer.is_owner() {
        let list = path
            .strip_prefix("/list/")
            .is_some_and(|key| !key.is_empty() && !key.contains('/'));
        if !matches!(path, "/" | "/new") && !list && session.is_none() {
            return text_response(404, "Not found");
        }
    } else if path != "/" && !session.is_some_and(|id| viewer.can_view(id)) {
        return text_response(403, "Forbidden");
    }
    let Some(bundle) = BROWSER_BUNDLE else {
        return text_response(503, "Could not load the UI");
    };
    let mut response = html_response(
        APP.replace("{{base}}", bundle.base)
            .replace("{{nonce}}", nonce),
    )?;
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&content_security_policy(nonce, &request.origin))?,
    );
    Ok(response)
}

fn browser_asset(request: &HttpRequest) -> HttpResult<HttpResponse> {
    if viewer(request)?.is_none() {
        return text_response(401, "Unauthorized");
    }
    if let Some(bundle) = BROWSER_BUNDLE
        && let Some((base, file)) = request
            .path
            .strip_prefix(WEB_PREFIX)
            .and_then(|p| p.split_once('/'))
        && base == bundle.base
        && let Some(&(_, mime, bytes)) = bundle.files.iter().find(|(name, _, _)| *name == file)
    {
        let mut response = HttpResponse::new(Body::from(bytes));
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        return Ok(response);
    }
    text_response(404, "Not found")
}

fn sign_in(request: &HttpRequest) -> HttpResult<HttpResponse> {
    sign_in_page(request, StatusCode::UNAUTHORIZED)
}

pub(crate) fn sign_in_page(request: &HttpRequest, status: StatusCode) -> HttpResult<HttpResponse> {
    let nonce = new_token(18);
    let page = request.path.as_str();
    let reload = if foreign_browser_request(&request.headers)
        && (matches!(page, "/" | "/new" | "/owner")
            || ["/s/", "/list/", "/join/"]
                .iter()
                .any(|prefix| page.starts_with(prefix)))
    {
        "if(!location.hash)location.replace(location.href);"
    } else {
        ""
    };
    let mut response = if request.method == http::Method::HEAD {
        let mut response = HttpResponse::new(Body::empty());
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        response
    } else {
        html_response(
            SIGN_IN
                .replace("{{nonce}}", &nonce)
                .replace("{{reload}}", reload),
        )?
    };
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(
        &format!("default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
    )?);
    Ok(response)
}

pub(crate) fn add_routes(routes: &mut HttpContribution, nonce: String) -> HttpResult<()> {
    for &(path, mime, bytes) in PUBLIC_FILES {
        routes.route(
            &format!("GET {path}"),
            RouteOptions::public(move |_| {
                Box::pin(async move {
                    let mut response = HttpResponse::new(Body::from(bytes));
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                    let cache = if path == "/sw.js" {
                        "no-cache"
                    } else {
                        "public, max-age=86400"
                    };
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
                    Ok(response)
                })
            }),
        )?;
    }

    routes.route(
        "GET /_chord/web/base",
        RouteOptions::public(|_| {
            Box::pin(async {
                match browser_base() {
                    Some(base) => json_response(&base.into()),
                    None => text_response(503, "Could not load the UI"),
                }
            })
        }),
    )?;
    routes.route(
        "GET /_chord/web/*",
        RouteOptions::new(|request| Box::pin(async move { browser_asset(&request) })),
    )?;

    let fallback = RouteOptions::public(|request| {
        Box::pin(async move {
            match viewer(&request)? {
                Some(Principal::Guest { .. }) => text_response(403, "Forbidden"),
                Some(_) => text_response(404, "Not found"),
                None => sign_in(&request),
            }
        })
    });
    let nonce = Arc::<str>::from(nonce);
    let mut page = RouteOptions::public(move |request| {
        let nonce = nonce.clone();
        Box::pin(async move { app_page(&request, &nonce) })
    });
    page.page = true;
    for pattern in [
        "GET /",
        "GET /new",
        "GET /s/*",
        "GET /list/*",
        "GET /join/*",
    ] {
        routes.route(pattern, page.clone())?;
    }
    routes.route("GET /*", fallback)?;
    for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        routes.route(
            &format!("{method} /__p2_dev"),
            RouteOptions::public(|_| {
                Box::pin(async {
                    let mut response = HttpResponse::new(Body::empty());
                    *response.status_mut() = StatusCode::FORBIDDEN;
                    Ok(response)
                })
            }),
        )?;
    }
    Ok(())
}
