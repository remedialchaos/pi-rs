use std::{error::Error, net::SocketAddr, sync::Arc, time::Duration};

use pi_rs_chord::{
    context::Context,
    json::JsonValue,
    link::Link,
    node::http::{HttpContribution, connect_websocket},
    remote::{RemoteOptions, RemotePeer},
    state::Batch,
};
use pi_rs_p2::{
    api::v1::{FacetKind, p2_contracts::lookup},
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
                Some("fixture=device") => Some(json!({"kind":"owner","access":"device","device":"phone"})),
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
        let (client, _) = browser_link(server.address, cookie).await?;
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

async fn browser_link(
    address: SocketAddr,
    cookie: &str,
) -> TestResult<(RemotePeer, Arc<dyn Link>)> {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::COOKIE, cookie.parse()?);
    let link = connect_websocket(
        &format!("ws://{address}/chord?client=script"),
        headers,
        1024 * 1024,
        4 * 1024 * 1024,
    )
    .await?;
    let peer = RemotePeer::connect(
        link.clone(),
        RemoteOptions::new(json!({"kind":"server"})),
        Batch::new(|_| {}),
    )?;
    Ok((peer, link))
}

#[tokio::test]
async fn signing_out_closes_only_selected_live_clients_and_permits_fresh_authentication()
-> TestResult {
    use pi_rs_chord::host::Caller;
    let directory = tempfile::tempdir()?;
    let server = server(&directory).await?;
    let ctx = Context::background();
    let empty = JsonValue::from_serde(json!({}));
    let core = server
        .host
        .get(Arc::new(lookup("p2.server")?), empty.clone(), &ctx)
        .await?;
    let access = Arc::new(lookup("p2.access")?);
    let device_caller = Caller {
        principal: Some(json!({"kind":"owner","access":"device","device":"phone"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    };
    let device_access = server
        .host
        .hold_as(access.clone(), empty.clone(), device_caller, &ctx)?;
    device_access.wait(&ctx).await?;
    let mut clients = Vec::new();
    for cookie in [
        "fixture=device",
        "fixture=owner",
        "fixture=guest",
        "fixture=shared",
    ] {
        let (client, link) = browser_link(server.address, cookie).await?;
        let held = client.hold(access.clone(), empty.clone(), &ctx)?;
        tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await??;
        clients.push((client, link, held));
    }
    for (method, selector, target) in [
        ("revokeOwners", json!({"device":"phone"}), 0),
        ("revokeGuests", json!({"id":"guest"}), 2),
        ("revokeOwners", json!({}), 1),
    ] {
        core.call(method, JsonValue::from_serde(selector), &ctx)
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !clients[target].1.closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        clients[3].0.fence().await?;
        assert_eq!(clients[3].2.variant().as_deref(), Some("guest"));
        if target == 0 {
            clients[1].0.fence().await?;
            clients[2].0.fence().await?;
            assert!(matches!(device_access.wait(&ctx).await,
                Err(pi_rs_chord::Error::Chord(e)) if e.code == "forbidden" && e.reason.as_deref() == Some("revoked")));
        }
    }
    let (fresh, _) = browser_link(server.address, "fixture=device").await?;
    let held = fresh.hold(access, empty, &ctx)?;
    tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await??;
    assert_eq!(held.variant().as_deref(), Some("owner"));
    fresh.close();
    for (client, _, _) in clients {
        client.close();
    }
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn owner_renames_the_server_across_clients_and_restarts() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("identity.json");
    std::fs::write(&path, b"{ broken")?;
    let first = server(&directory).await?;
    let ctx = Context::background();
    let empty = JsonValue::from_serde(json!({}));
    let local = first
        .host
        .get(Arc::new(lookup("p2.server-name")?), empty.clone(), &ctx)
        .await?;
    let settings = Arc::new(lookup("p2.server-name-settings")?);
    let device = local.read("device")?.to_serde()?;
    assert_eq!(local.read("name")?.to_serde()?, device);
    assert!(
        pi_rs_p2::runtime::logs::process_logs()
            .notices()
            .iter()
            .any(|notice| notice.source == "server"
                && notice.message.contains("identity.json is not usable"))
    );
    for cookie in ["fixture=guest", "fixture=worker"] {
        let (client, _) = browser_link(first.address, cookie).await?;
        let held = client.hold(settings.clone(), empty.clone(), &ctx)?;
        let ready = tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await?;
        assert!(matches!(ready, Err(pi_rs_chord::Error::Chord(e)) if e.code == "forbidden"));
        client.close();
    }
    let (client, _) = browser_link(first.address, "fixture=owner").await?;
    let held = client.hold(settings.clone(), empty.clone(), &ctx)?;
    tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await??;
    let expected = format!("Lab {}", "\u{10400}".repeat(59));
    let raw = format!(" Lab\u{0000}\u{200b}\n {}   ", "\u{10400}".repeat(65));
    held.call("rename", JsonValue::from_serde(json!({"name":raw})), &ctx)
        .await?;
    client.fence().await?;
    assert_eq!(
        held.member("name")?.value()?.to_json().to_serde()?,
        json!(expected)
    );
    assert_eq!(local.read("chosen")?.to_serde()?, json!(expected));
    assert_eq!(
        std::fs::read_to_string(&path)?,
        format!("{{\n\t\"name\": \"{expected}\"\n}}\n")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    client.close();
    first.close().await?;

    let restarted = server(&directory).await?;
    let (client, _) = browser_link(restarted.address, "fixture=device").await?;
    let held = client.hold(settings, empty, &ctx)?;
    tokio::time::timeout(Duration::from_secs(5), held.wait(&ctx)).await??;
    assert_eq!(
        held.member("chosen")?.value()?.to_json().to_serde()?,
        json!(expected)
    );
    held.call(
        "rename",
        JsonValue::from_serde(json!({"name":" \t "})),
        &ctx,
    )
    .await?;
    client.fence().await?;
    assert!(!path.try_exists()?);
    assert_eq!(held.member("name")?.value()?.to_json().to_serde()?, device);
    assert_eq!(held.member("chosen")?.value()?.to_json(), JsonValue::Null);
    client.close();
    restarted.close().await?;
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
