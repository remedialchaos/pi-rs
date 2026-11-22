use pi_rs_chord::{
    context::Context,
    host::{HostOptions, create_host},
    node::{surfaces::*, tui::*},
    tui::{LauncherMessage, SandboxMessage},
};
use serde_json::json;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn published_surface_digest_holdback_and_real_http_artifact() {
    let surface = Surface::new("web", b"base module".to_vec()).unwrap();
    let artifact = |bytes: &[u8]| {
        SurfaceInput::Built(SurfaceArtifact {
            name: "example".into(),
            bytes: bytes.to_vec(),
            css: Some(b"body{}".to_vec()),
            assets: BTreeMap::new(),
        })
    };
    surface
        .publish(vec![artifact(b"first")], &Context::background())
        .unwrap();
    let first = surface.manifest();
    assert_eq!(first.plugins.len(), 1);
    surface
        .publish(
            vec![SurfaceInput::Failed {
                name: "example".into(),
                error: "bad build".into(),
            }],
            &Context::background(),
        )
        .unwrap();
    assert_eq!(surface.manifest(), first);
    surface
        .publish(vec![artifact(b"second")], &Context::background())
        .unwrap();
    assert_ne!(surface.manifest().combined, first.combined);
    assert_eq!(surface.file(&first.plugins[0].url).unwrap().bytes, b"first");
    let host = create_host(HostOptions {
        principal: Some(json!({"kind":"server"})),
        address: Some(pi_rs_chord::json::JsonValue::from_serde(
            json!({"kind":"server","params":{}}),
        )),
        ..Default::default()
    })
    .unwrap();
    let report = host
        .load(
            vec![pi_rs_chord::host::PluginFacet::new(
                "surface",
                surface.facet().unwrap(),
            )],
            &Context::background(),
        )
        .await
        .unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    let held = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        host.get(
            std::sync::Arc::new(
                pi_rs_chord::surfaces::scripts_contract("chord.web.scripts").unwrap(),
            ),
            pi_rs_chord::json::JsonValue::from_serde(json!({})),
            &Context::background(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        held.read("set").unwrap().to_serde().unwrap(),
        serde_json::to_value(surface.manifest()).unwrap()
    );
    drop(held);
    let http = pi_rs_chord::node::http::Http::new(host.clone(), Default::default());
    http.contribute(surface.contribution(true).unwrap())
        .unwrap();
    let server = http.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let mut stream = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                surface.manifest().plugins[0].url,
                server.address
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("second"));
    assert!(response.contains("immutable"));
    server.close().await.unwrap();
    host.close(&Context::background()).await.unwrap();
}

#[test]
fn terminal_pipe_fragmentation_body_bounds_and_output_filter() {
    let encoded = frame(
        &json!({"kind":"write"}),
        b"hello\x1b[31m red\x1b[0m\x1b]52;c;secret\x07",
    )
    .unwrap();
    let mut reader = FrameReader::default();
    let mut messages = Vec::new();
    for chunk in encoded.chunks(3) {
        messages.extend(reader.push(chunk).unwrap());
    }
    reader.finish().unwrap();
    let SandboxMessage::Write { data } = &messages[0] else {
        panic!("wrong message")
    };
    assert_eq!(filter_output(data).unwrap(), "hello\x1b[31m red\x1b[0m");
    let mut launcher = TerminalLauncher::new("token".into());
    let mut output = Vec::new();
    launcher.apply(messages.remove(0), &mut output).unwrap();
    assert!(!String::from_utf8(output).unwrap().contains("secret"));
    assert!(
        FrameReader::default()
            .push(&u32::MAX.to_be_bytes())
            .is_err()
    );
    let mut truncated = FrameReader::default();
    truncated.push(&encoded[..9]).unwrap();
    assert!(truncated.finish().is_err());
    let binary = launcher_frame(&LauncherMessage::SocketMessage {
        socket: 7,
        bytes: vec![0, 255],
    })
    .unwrap();
    assert_eq!(&binary[binary.len() - 2..], &[0, 255]);
    assert!(filter_output(&"x".repeat(65537)).is_err());
}

#[test]
fn terminal_credentials_are_private_atomic_and_never_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("token");
    assert!(save_credentials(&path, "first").unwrap());
    assert!(!save_credentials(&path, "replacement").unwrap());
    assert_eq!(read_credentials(&path).unwrap(), "first");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
