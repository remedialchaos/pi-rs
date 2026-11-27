use futures_util::{SinkExt, StreamExt};
use pi_rs_chord::{
    context::Context,
    host::{HostOptions, PluginFacet, PluginFailure, create_host},
    node::{http::*, surfaces::*},
};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

async fn request(addr: SocketAddr, method: &str, path: &str) -> (String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (
        String::from_utf8(bytes[..end].to_vec()).unwrap(),
        bytes[end + 4..].to_vec(),
    )
}

fn plugin(version: &'static str) -> PluginFacet {
    let mut contribution = HttpContribution::new("not-the-owner");
    contribution
        .route(
            "GET /resource/:id",
            RouteOptions::public(move |req| {
                Box::pin(
                    async move { text_response(200, format!("{version}:{}", req.params["id"])) },
                )
            }),
        )
        .unwrap();
    let mut wrap = HttpWrap::new("version", WrapPhase::Headers, move |req, next| {
        Box::pin(async move {
            let mut response = next(req).await?;
            response
                .headers_mut()
                .insert("x-plugin-version", http::HeaderValue::from_static(version));
            Ok(response)
        })
    });
    wrap.route = Some("GET /resource/:id".into());
    contribution.wraps.push(wrap);
    contribution.sockets.insert(
        "/socket/:id".into(),
        SocketOptions {
            public: true,
            max_payload: 4096,
            handler: Arc::new(move |mut socket, req| {
                Box::pin(async move {
                    let greeting = format!("{version}:{}", req.params["id"]);
                    socket
                        .send(axum::extract::ws::Message::Text(greeting.into()))
                        .await
                        .map_err(|e| HttpError::new(500, e.to_string()))?;
                    while let Some(Ok(message)) = socket.next().await {
                        if matches!(message, axum::extract::ws::Message::Close(_)) {
                            break;
                        }
                        socket
                            .send(message)
                            .await
                            .map_err(|e| HttpError::new(500, e.to_string()))?;
                    }
                    Ok(())
                })
            }),
        },
    );
    PluginFacet::new("example.http", contribution.facet())
}

#[tokio::test]
async fn host_reload_replaces_routes_wraps_and_socket_owners_on_the_same_listener() {
    let ctx = Context::background();
    let host = create_host(HostOptions::default()).unwrap();
    let http = Http::new(host.clone(), Default::default());
    let server = http.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = server.address;
    assert!(
        request(addr, "GET", "/resource/one")
            .await
            .0
            .starts_with("HTTP/1.1 404")
    );
    host.load(vec![plugin("old")], &ctx).await.unwrap();
    let (headers, bytes) = request(addr, "GET", "/resource/one").await;
    assert_eq!(bytes, b"old:one");
    assert!(headers.contains("x-plugin-version: old"), "{headers}");
    let (headers, bytes) = request(addr, "HEAD", "/resource/one").await;
    assert!(headers.contains("x-plugin-version: old"), "{headers}");
    assert!(bytes.is_empty());

    let url = format!("ws://{addr}/socket/one");
    let (mut old_socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert_eq!(
        old_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
        "old:one"
    );
    host.reload_with_failures(
        vec![],
        vec![PluginFailure {
            plugin: "example.http".into(),
            error: "failed build".into(),
        }],
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(request(addr, "GET", "/resource/one").await.1, b"old:one");
    old_socket.send(Message::Text("echo".into())).await.unwrap();
    assert_eq!(
        old_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
        "echo"
    );

    host.reload(vec![plugin("new")], &ctx).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), old_socket.next())
        .await
        .unwrap();
    assert!(ended.is_none_or(|message| message.is_err() || message.is_ok_and(|m| m.is_close())));
    let (headers, bytes) = request(addr, "GET", "/resource/two").await;
    assert_eq!(bytes, b"new:two");
    assert!(headers.contains("x-plugin-version: new"), "{headers}");
    let (mut new_socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert_eq!(
        new_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
        "new:one"
    );
    host.reload(vec![], &ctx).await.unwrap();
    assert!(
        request(addr, "GET", "/resource/two")
            .await
            .0
            .starts_with("HTTP/1.1 404")
    );
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    server.close().await.unwrap();
    host.close(&ctx).await.unwrap();
}

#[tokio::test]
async fn surface_registry_serves_bootstrap_artifacts_and_live_publications()
-> Result<(), Box<dyn std::error::Error>> {
    let ctx = Context::background();
    let host = create_host(HostOptions {
        principal: Some(serde_json::json!({"kind":"server"})),
        address: Some(pi_rs_chord::json::JsonValue::from_serde(
            serde_json::json!({"kind":"server","params":{}}),
        )),
        ..Default::default()
    })
    .unwrap();
    let surface = Surface::new("web", vec![0, 97, 115, 109]).unwrap();
    let loaded = host
        .load(
            vec![PluginFacet::new(
                "example.surface",
                surface.http_facet(true).unwrap(),
            )],
            &ctx,
        )
        .await
        .unwrap();
    assert!(loaded.failed.is_empty(), "{loaded:?}");
    let server = Http::new(host.clone(), Default::default())
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = server.address;
    let (headers, bytes) = request(addr, "GET", "/_chord/web/start").await;
    assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
    let initial: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let base = initial["script"].as_str().unwrap();
    let (headers, bytes) = request(addr, "GET", base).await;
    assert_eq!(bytes, [0, 97, 115, 109]);
    assert!(
        headers.contains("content-type: application/wasm"),
        "{headers}"
    );
    assert!(headers.contains("immutable"), "{headers}");
    surface
        .publish(
            vec![SurfaceInput::Built(SurfaceArtifact {
                name: "example.widget".into(),
                bytes: b"widget".to_vec(),
                css: Some(b"body{}".to_vec()),
                assets: BTreeMap::new(),
            })],
            &ctx,
        )
        .unwrap();
    let next: serde_json::Value =
        serde_json::from_slice(&request(addr, "GET", "/_chord/web/start").await.1).unwrap();
    assert_ne!(initial["combined"], next["combined"]);
    let entry = &surface.manifest().plugins[0];
    assert_eq!(request(addr, "GET", &entry.url).await.1, b"widget");
    assert_eq!(
        request(addr, "GET", entry.css.as_deref().unwrap()).await.1,
        b"body{}"
    );
    assert_eq!(
        request(addr, "GET", initial["combined"].as_str().unwrap())
            .await
            .1,
        b"[]"
    );
    host.reload(
        vec![PluginFacet::new(
            "example.surface",
            surface.http_facet(false).unwrap(),
        )],
        &ctx,
    )
    .await
    .unwrap();
    assert!(
        request(addr, "GET", base)
            .await
            .0
            .starts_with("HTTP/1.1 401")
    );
    let (headers, body) = request(addr, "GET", "/_chord/web/base").await;
    assert!(
        headers.starts_with("HTTP/1.1 200") && headers.contains("cache-control: no-store"),
        "{headers}"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).ok(),
        Some(initial["base"].clone())
    );
    assert!(
        headers.contains("style-src 'self'")
            && headers.contains("img-src 'self' data:")
            && headers.contains("font-src 'self'")
            && headers.contains("/_chord/web/"),
        "{headers}"
    );
    let unbuilt = Surface::new("web", Vec::new())?;
    host.reload(
        vec![PluginFacet::new(
            "example.surface",
            unbuilt.http_facet(false)?,
        )],
        &ctx,
    )
    .await?;
    assert!(
        request(addr, "GET", "/_chord/web/base")
            .await
            .0
            .starts_with("HTTP/1.1 503")
    );
    host.reload(vec![], &ctx).await.unwrap();
    assert!(
        request(addr, "GET", base)
            .await
            .0
            .starts_with("HTTP/1.1 404")
    );
    server.close().await.unwrap();
    host.close(&ctx).await.unwrap();
    Ok(())
}
