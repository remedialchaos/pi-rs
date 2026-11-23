//! Embedded public assets and the unauthenticated browser entrance.

use axum::body::Body;
use http::{HeaderValue, StatusCode, header};
use pi_rs_chord::node::{
    app::new_token,
    http::{
        HttpContribution, HttpRequest, HttpResponse, HttpResult, RouteOptions, html_response,
        text_response,
    },
};

use crate::runtime::http::foreign_browser_request;

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

fn sign_in(request: &HttpRequest) -> HttpResult<HttpResponse> {
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
    let mut response = html_response(
        SIGN_IN
            .replace("{{nonce}}", &nonce)
            .replace("{{reload}}", reload),
    )?;
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(
        &format!("default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
    )?);
    Ok(response)
}

pub(crate) fn add_routes(routes: &mut HttpContribution) -> HttpResult<()> {
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

    let fallback = RouteOptions::public(|request| {
        Box::pin(async move {
            if request.principal.is_some() {
                return text_response(404, "Not found");
            }
            sign_in(&request)
        })
    });
    let mut app_page = fallback.clone();
    app_page.page = true;
    for pattern in [
        "GET /",
        "GET /new",
        "GET /owner",
        "GET /s/*",
        "GET /list/*",
        "GET /join/*",
    ] {
        routes.route(pattern, app_page.clone())?;
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
