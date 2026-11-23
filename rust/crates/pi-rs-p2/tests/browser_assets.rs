use std::{error::Error, net::SocketAddr, time::Duration};

use pi_rs_chord::node::http::HttpContribution;
use pi_rs_p2::{
    api::v1::FacetKind,
    hosts::server::{P2Server, ServerOptions, start_server},
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const OWNER: &str = "Cookie: fixture=owner\r\n";
const GUEST: &str = "Cookie: fixture=guest\r\n";
const BASE: &str = "/_chord/web/base";

async fn server(directory: &tempfile::TempDir) -> TestResult<P2Server> {
    let mut auth = HttpContribution::new("browser-fixture");
    auth.authenticators.push(std::sync::Arc::new(|request| {
        Box::pin(async move {
            Ok(match request.header("cookie") {
                Some("fixture=owner") => Some(json!({"kind":"owner","access":"local"})),
                Some("fixture=guest" | "fixture=both") => Some(json!({
                    "kind":"guest","id":"guest","name":"Guest","home":"aa-11",
                    "sessions":{"aa-11":"view"},"revision":1
                })),
                Some("fixture=shared") => Some(json!({
                    "kind":"guest","id":"share","name":"Invited","home":"second","email":"user@example.test",
                    "sessions":{"second":"full","first":"view"},"revision":2
                })),
                Some("fixture=worker") => {
                    Some(json!({"kind":"worker","session":"aa-11","generation":1}))
                }
                Some("fixture=incomplete-owner") => Some(json!({"kind":"owner"})),
                _ => None,
            })
        })
    }));
    auth.authenticators.push(std::sync::Arc::new(|request| {
        Box::pin(async move {
            Ok((request.header("cookie") == Some("fixture=both"))
                .then(|| json!({"kind":"owner","access":"local"})))
        })
    }));
    let mut options = ServerOptions::new(directory.path().to_owned());
    options.port = 0;
    options
        .registry
        .register("browser-fixture", FacetKind::Server, auth.facet())?;
    Ok(start_server(options).await?)
}

#[tokio::test]
async fn access_over_chord_exposes_only_the_authenticated_browsers_own_grant() -> TestResult {
    use pi_rs_chord::{
        context::Context,
        json::JsonValue,
        node::http::connect_websocket,
        remote::{RemoteOptions, RemotePeer},
        state::Batch,
    };
    use pi_rs_p2::api::v1::p2_contracts::lookup;
    use std::sync::Arc;

    let directory = tempfile::tempdir()?;
    let server = server(&directory).await?;
    let contract = Arc::new(lookup("p2.access")?);
    let ctx = Context::background();
    for (cookie, variant, grant) in [
        ("fixture=owner", "owner", None),
        ("fixture=both", "owner", None),
        (
            "fixture=guest",
            "guest",
            Some(
                json!({"id":"guest","name":"Guest","session":"aa-11","mode":"view","sessions":{"aa-11":"view"}}),
            ),
        ),
        (
            "fixture=shared",
            "guest",
            Some(
                json!({"id":"share","name":"Invited","session":"second","mode":"full","sessions":{"second":"full","first":"view"},"email":"user@example.test"}),
            ),
        ),
        ("fixture=worker", "forbidden", None),
    ] {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::COOKIE, cookie.parse()?);
        let link = connect_websocket(
            &format!("ws://{}/chord?client=script", server.address),
            headers,
            1024 * 1024,
            4 * 1024 * 1024,
        )
        .await?;
        let client = RemotePeer::connect(
            link,
            RemoteOptions::new(json!({"kind":"server"})),
            Batch::new(|_| {}),
        )?;
        let held = client.hold(contract.clone(), JsonValue::from_serde(json!({})), &ctx)?;
        let ready = tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await?;
        if variant == "forbidden" {
            assert!(matches!(ready, Err(pi_rs_chord::Error::Chord(e)) if e.code == "forbidden"));
        } else {
            ready?;
            assert_eq!(held.variant().as_deref(), Some(variant));
            if let Some(grant) = grant {
                assert_eq!(held.member("grant")?.value()?.to_json().to_serde()?, grant);
            } else {
                assert!(held.member("grant").is_err());
            }
        }
        held.release();
        client.close();
    }
    server.close().await?;
    Ok(())
}

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> TestResult<(String, Vec<u8>)> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await??;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("missing response headers")?;
    Ok((
        String::from_utf8(response[..split].to_vec())?,
        response.split_off(split + 4),
    ))
}

#[tokio::test]
async fn browser_shell_requires_an_owner_or_a_guest_with_access_to_the_page() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = server(&directory).await?;
    let address = server.address;
    let (headers, body) = request(address, "GET", "/", "").await?;
    assert!(headers.starts_with("HTTP/1.1 401"));
    let sign_in = String::from_utf8(body)?;
    assert!(sign_in.contains("<h1>Sign in to Pi</h1>"));
    assert!(sign_in.contains("fetch(\"/owner/claim\""));
    assert!(!sign_in.contains("chordPage"));

    let available_status = if option_env!("P2_BROWSER_ASSETS").is_some() {
        "HTTP/1.1 200"
    } else {
        "HTTP/1.1 503"
    };
    for (path, cookie, status) in [
        ("/", OWNER, available_status),
        ("/new", OWNER, available_status),
        ("/list/inbox", OWNER, available_status),
        ("/s/aa-11/c/2", OWNER, available_status),
        ("/s/aa-11/c/0", OWNER, "HTTP/1.1 404"),
        ("/unmatched", OWNER, "HTTP/1.1 404"),
        ("/", GUEST, available_status),
        ("/s/aa-11", GUEST, available_status),
        ("/s/aa-11/c/2", GUEST, available_status),
        ("/s/aa-12", GUEST, "HTTP/1.1 403"),
        ("/s/%61a-11", GUEST, "HTTP/1.1 403"),
        ("/new", GUEST, "HTTP/1.1 403"),
        ("/list/inbox", GUEST, "HTTP/1.1 403"),
        ("/", "Cookie: fixture=worker\r\n", "HTTP/1.1 403"),
        ("/", "Cookie: fixture=incomplete-owner\r\n", "HTTP/1.1 403"),
    ] {
        let (headers, _) = request(address, "GET", path, cookie).await?;
        assert!(headers.starts_with(status), "{path}: {headers}");
    }
    let foreign = "Cookie: fixture=owner\r\nSec-Fetch-Site: same-site\r\n";
    let (headers, _) = request(address, "GET", "/", foreign).await?;
    assert!(headers.starts_with("HTTP/1.1 403"));
    let navigation = format!("{foreign}Sec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n");
    let (headers, _) = request(address, "GET", "/", &navigation).await?;
    assert!(headers.starts_with(available_status));
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn browser_bundle_paths_share_the_csp_bootstrap_version_and_serve_exact_assets() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let server = server(&directory).await?;
    let address = server.address;
    let (headers, base) = request(address, "GET", BASE, "").await?;
    let Some(input) = option_env!("P2_BROWSER_ASSETS") else {
        assert!(headers.starts_with("HTTP/1.1 503"));
        assert_eq!(base, b"Could not load the UI");
        server.close().await?;
        return Ok(());
    };
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(input);
    assert!(headers.starts_with("HTTP/1.1 200"));
    assert!(headers.contains("cache-control: no-store"));
    let base: String = serde_json::from_slice(&base)?;
    assert_eq!(base.len(), 64);
    assert!(base.bytes().all(|b| b.is_ascii_hexdigit()));

    let prefix = format!("/_chord/web/{base}");
    let (headers, body) = request(address, "GET", "/s/aa-11/c/2", GUEST).await?;
    let body = String::from_utf8(body)?;
    let csp = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-security-policy: "))
        .ok_or("missing CSP")?;
    let nonce = csp
        .split_once("'nonce-")
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(nonce, _)| nonce)
        .ok_or("missing CSP nonce")?;
    assert!(headers.starts_with("HTTP/1.1 200"));
    assert!(headers.contains("cache-control: no-store"));
    assert!(csp.contains("'wasm-unsafe-eval'"));
    assert!(csp.contains(&format!("connect-src 'self' ws://{address}")));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("'unsafe-eval'"));
    assert!(body.contains(r#"<div id="root"></div>"#));
    assert!(body.contains(&format!(r#"nonce="{nonce}""#)));
    assert!(body.contains(&format!(r#"data-web-version="{base}""#)));
    assert!(!body.contains("globalThis.chordPage"));
    assert!(!body.contains("aa-11"));
    assert!(body.contains(&format!(r#""{prefix}/app.js""#)));
    assert!(body.contains(&format!(r#""{prefix}/p2.css""#)));
    assert_eq!(body.matches("await init()").count(), 1);

    let (owner_headers, owner_body) = request(address, "GET", "/", OWNER).await?;
    assert!(owner_headers.starts_with("HTTP/1.1 200"));
    assert!(
        owner_headers
            .lines()
            .any(|line| { line.strip_prefix("content-security-policy: ") == Some(csp) }),
        "authorized pages on the same server must use the same CSP nonce"
    );
    assert!(String::from_utf8(owner_body)?.contains(&format!(r#"nonce="{nonce}""#)));

    let (head, body) = request(address, "HEAD", "/", OWNER).await?;
    assert!(head.starts_with("HTTP/1.1 200"));
    assert!(body.is_empty());

    let css = std::fs::read_to_string(input.join("p2.css"))?;
    let fonts: std::collections::BTreeSet<_> = css
        .split("url(\"./")
        .skip(1)
        .filter_map(|tail| tail.split_once("\")").map(|(filename, _)| filename))
        .filter(|file| file.ends_with(".woff2"))
        .collect();
    assert_eq!(fonts.len(), 5);
    let assets = [
        ("app.js", "text/javascript; charset=utf-8"),
        ("app_bg.wasm", "application/wasm"),
        ("p2.css", "text/css; charset=utf-8"),
    ];
    for (file, mime) in assets
        .into_iter()
        .chain(fonts.into_iter().map(|f| (f, "font/woff2")))
    {
        let (headers, body) = request(address, "GET", &format!("{prefix}/{file}"), GUEST).await?;
        assert!(headers.starts_with("HTTP/1.1 200"), "{file}: {headers}");
        let expected_type = format!("content-type: {mime}");
        assert!(
            headers.lines().any(|line| line == expected_type),
            "{file}: {headers}"
        );
        assert!(
            headers.contains("cache-control: no-store"),
            "{file}: {headers}"
        );
        assert_eq!(body, std::fs::read(input.join(file))?, "{file}");
    }
    let wasm = format!("{prefix}/app_bg.wasm");
    let (headers, body) = request(address, "HEAD", &wasm, OWNER).await?;
    assert!(headers.starts_with("HTTP/1.1 200"));
    assert!(headers.contains("content-type: application/wasm"));
    assert!(body.is_empty());

    for (path, credential, status) in [
        (wasm, "", "HTTP/1.1 401"),
        (format!("{prefix}/missing"), OWNER, "HTTP/1.1 404"),
        (format!("{prefix}/../{base}/app.js"), OWNER, "HTTP/1.1 404"),
        ("/_chord/web/obsolete/app.js".into(), OWNER, "HTTP/1.1 404"),
        (
            format!("{prefix}/app.js"),
            "Cookie: fixture=worker\r\n",
            "HTTP/1.1 403",
        ),
        (
            format!("{prefix}/app.js"),
            "Cookie: fixture=owner\r\nSec-Fetch-Site: cross-site\r\n",
            "HTTP/1.1 403",
        ),
    ] {
        let (headers, _) = request(address, "GET", &path, credential).await?;
        assert!(headers.starts_with(status), "{path}: {headers}");
    }
    server.close().await?;
    Ok(())
}
