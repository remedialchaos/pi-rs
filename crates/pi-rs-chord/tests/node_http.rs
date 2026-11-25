use pi_rs_chord::{
    context::Context,
    host::{HostOptions, create_host},
    node::http::*,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn signed_cookie_wire_expiry_tampering_and_name_binding() {
    let options = CookieOptions {
        secret: "secret".into(),
        secure: true,
        max_age: Some(10),
        same_site: SameSite::Strict,
    };
    let made = cookie_at("session", "hello./ 雪", &options, 100).unwrap();
    assert!(made.ends_with("HttpOnly; Path=/; SameSite=Strict; Secure; Max-Age=10"));
    assert_eq!(
        read_cookie_at(&made, "session", "secret", 109).as_deref(),
        Some("hello./ 雪")
    );
    assert_eq!(read_cookie_at(&made, "session", "secret", 110), None);
    assert_eq!(
        read_cookie_at(&made.replace("hello", "other"), "session", "secret", 100),
        None
    );
    assert_eq!(read_cookie_at(&made, "another", "secret", 100), None);
    assert_eq!(read_cookie_at(&made, "session", "wrong", 100), None);
    assert!(cookie_at("bad\r\n", "x", &options, 100).is_err());
}

#[tokio::test]
async fn loopback_routes_wrap_phases_auth_and_security_headers() {
    let host = create_host(HostOptions::default()).unwrap();
    host.load(vec![], &Context::background()).await.unwrap();
    let http = Http::new(host.clone(), HttpOptions::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut contribution = HttpContribution::new("example");
    contribution
        .route(
            "GET /hello/:name",
            RouteOptions::public(|r| {
                Box::pin(
                    async move { json_response(&serde_json::json!({"hello":r.params["name"]})) },
                )
            }),
        )
        .unwrap();
    contribution
        .route(
            "GET /private",
            RouteOptions::new(|_| Box::pin(async { text_response(200, "private") })),
        )
        .unwrap();
    for (name, phase) in [
        ("transform", WrapPhase::Transform),
        ("observe", WrapPhase::Observe),
        ("headers", WrapPhase::Headers),
    ] {
        let calls = calls.clone();
        contribution
            .wraps
            .push(HttpWrap::new(name, phase, move |request, next| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.lock().unwrap().push(format!("{name}:before"));
                    let response = next(request).await;
                    calls.lock().unwrap().push(format!("{name}:after"));
                    response
                })
            }));
    }
    http.contribute(contribution).unwrap();
    let server = http.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let response = request(server.address, "GET", "/hello/snow%20man", "", "").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    for header in [
        "cache-control: no-store",
        "x-content-type-options: nosniff",
        "referrer-policy: no-referrer",
        "x-frame-options: DENY",
        "content-security-policy:",
    ] {
        assert!(response.contains(header), "{header}: {response}");
    }
    assert!(response.contains("{\"hello\":\"snow man\"}"));
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "observe:before",
            "headers:before",
            "transform:before",
            "transform:after",
            "headers:after",
            "observe:after"
        ]
    );
    assert!(
        request(server.address, "GET", "/private", "", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        request(
            server.address,
            "GET",
            "/hello/world",
            "Origin: https://evil.example\r\n",
            ""
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    assert!(
        request(
            server.address,
            "GET",
            "/hello/world",
            "X-Forwarded-For: 127.0.0.1\r\n",
            ""
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    server.close().await.unwrap();
    host.close(&Context::background()).await.unwrap();
}

#[tokio::test]
async fn static_files_reject_traversal_and_symlinks_and_serve_ranges_etags_head() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("public");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("data.txt"), b"0123456789").unwrap();
    std::fs::write(temp.path().join("secret"), b"secret").unwrap();
    let host = create_host(HostOptions::default()).unwrap();
    host.load(vec![], &Context::background()).await.unwrap();
    let http = Http::new(host.clone(), HttpOptions::default());
    let mut contribution = HttpContribution::new("files");
    for method in ["GET", "HEAD"] {
        let root = root.clone();
        contribution
            .route(
                &format!("{method} /files/*"),
                RouteOptions::public(move |r| {
                    let root = root.clone();
                    Box::pin(async move {
                        file_response(&root, &r.params["*"], &r, FileOptions::default()).await
                    })
                }),
            )
            .unwrap();
    }
    http.contribute(contribution).unwrap();
    let server = http.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let response = request(
        server.address,
        "GET",
        "/files/data.txt",
        "Range: bytes=2-5\r\n",
        "",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 206"), "{response}");
    assert!(response.contains("content-range: bytes 2-5/10"));
    assert!(response.ends_with("2345"));
    assert!(
        request(
            server.address,
            "GET",
            "/files/data.txt",
            "Range: bytes=10-\r\n",
            ""
        )
        .await
        .starts_with("HTTP/1.1 416")
    );
    let full = request(server.address, "GET", "/files/data.txt", "", "").await;
    let etag = full
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap()
        .trim();
    assert!(
        request(
            server.address,
            "GET",
            "/files/data.txt",
            &format!("If-None-Match: {etag}\r\n"),
            ""
        )
        .await
        .starts_with("HTTP/1.1 304")
    );
    let head = request(server.address, "HEAD", "/files/data.txt", "", "").await;
    assert!(head.contains("content-length: 10"));
    assert!(head.ends_with("\r\n\r\n"));
    assert!(
        request(server.address, "GET", "/files/%2e%2e/secret", "", "")
            .await
            .starts_with("HTTP/1.1 403")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path().join("secret"), root.join("escape")).unwrap();
        assert!(
            request(server.address, "GET", "/files/escape", "", "")
                .await
                .starts_with("HTTP/1.1 403")
        );
    }
    server.close().await.unwrap();
    host.close(&Context::background()).await.unwrap();
}
