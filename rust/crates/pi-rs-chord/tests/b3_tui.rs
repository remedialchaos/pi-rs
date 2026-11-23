use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{HostOptions, PluginFacet, create_host},
    json::JsonValue,
    node::{
        http::{Http, HttpContribution},
        surfaces::Surface,
        tui::*,
    },
    remote::{RemoteOptions, RemotePeer},
    state::Batch,
    surfaces::{ColorDepth, ThemeMode, scripts_contract},
    tui::{BridgeReply, BridgeRequest},
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Minimal fixture client, only for the loopback server's fixed-length responses.
struct LoopbackGet;
impl SurfaceGet for LoopbackGet {
    async fn get(
        &self,
        uri: http::Uri,
        authorization: http::HeaderValue,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let addr = uri.authority().unwrap().as_str();
        assert!(addr.starts_with("127.0.0.1:"));
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream.write_all(format!("GET {} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n", uri.path(), authorization.to_str().unwrap()).as_bytes()).await?;
        let mut response = Vec::new();
        (&mut stream)
            .take((limit + 4096) as u64)
            .read_to_end(&mut response)
            .await?;
        if !response.starts_with(b"HTTP/1.1 200 ") {
            return Err(Error::Invalid("loopback GET rejected".into()));
        }
        let end = response.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
        Ok(response.split_off(end + 4))
    }
}

fn settings() -> TerminalSettings {
    TerminalSettings {
        columns: 80,
        rows: 24,
        colors: ColorDepth::Indexed,
        mode: ThemeMode::Dark,
        locales: vec!["en-US".into()],
    }
}

#[tokio::test]
async fn launcher_bootstraps_private_surface_relays_rpc_and_closes_both_ends() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let ctx = Context::background();
        let host = create_host(HostOptions {
            principal: Some(serde_json::json!("server")),
            address: Some(JsonValue::from_serde(serde_json::json!({"kind":"server","params":{}}))),
            ..Default::default()
        }).unwrap();
        let surface = Surface::new("tui", b"native base".to_vec()).unwrap();
        let report = host.load(vec![PluginFacet::new("terminal", surface.http_facet(false).unwrap())], &ctx).await.unwrap();
        assert!(report.failed.is_empty());
        let http = Http::new(host.clone(), Default::default());
        let mut auth = HttpContribution::new("auth");
        auth.authenticators.push(Arc::new(|req| Box::pin(async move {
            Ok((req.header("authorization") == Some("Bearer terminal-test")).then(|| serde_json::json!("terminal")))
        })));
        http.contribute(auth).unwrap();
        let server = http.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let origin: http::Uri = format!("http://{}/", server.address).parse().unwrap();
        assert!(TerminalSession::start(origin.clone(), "wrong".into(), settings(), LoopbackGet).await.is_err());
        let (mut session, base) = TerminalSession::start(origin, "terminal-test".into(), settings(), LoopbackGet).await.unwrap();
        let channel = session.channel().clone();
        assert_eq!(base, b"native base");
        let advertised = surface.manifest();
        assert_eq!(advertised.base, channel.start().base);
        assert_eq!(channel.start().combined, "[]");

        let link = channel.socket().unwrap();
        let peer = RemotePeer::connect(link.clone(), RemoteOptions::new(serde_json::json!("server")), Batch::new(|_| {})).unwrap();
        let held = peer.hold(Arc::new(scripts_contract("chord.tui.scripts").unwrap()), JsonValue::from_serde(serde_json::json!({})), &ctx).unwrap();
        let mut output = Vec::new();
        {
            let ready = held.wait(&ctx);
            tokio::pin!(ready);
            loop {
                tokio::select! {
                    result = &mut ready => { result.unwrap(); break; }
                    result = session.step(&mut output) => assert_eq!(result.unwrap(), None),
                }
            }
        }
        assert_eq!(held.member("set").unwrap().value().unwrap().to_json(), JsonValue::from_serde(serde_json::to_value(&advertised).unwrap()));
        for (path, allowed) in [(advertised.combined.as_str(), true), ("/_chord/tui/%2e%2e/private", false), ("http://other.invalid/_chord/tui/base", false)] {
            let request = channel.request(BridgeRequest::Fetch { path: path.into() }, &ctx);
            tokio::pin!(request);
            loop {
                tokio::select! {
                    result = &mut request => {
                        if allowed { assert_eq!(result.unwrap(), BridgeReply::Value(serde_json::json!("[]"))); }
                        else { assert!(result.is_err()); }
                        break;
                    }
                    result = session.step(&mut output) => assert_eq!(result.unwrap(), None),
                }
            }
        }
        server.close().await.unwrap();
        while !link.closed() { tokio::task::yield_now().await; }
        channel.exit(7).unwrap();
        assert_eq!(session.step(&mut output).await.unwrap(), Some(7));
        assert!(channel.socket().is_err());
        host.close(&ctx).await.unwrap();
    }).await.expect("authenticated launcher round trip and close must finish");
}
