use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{Contract, Member, schema::*},
    host::*,
    json::JsonValue,
    node::http::*,
    remote::{RemoteOptions, RemotePeer},
    state::{Batch, Source},
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const TEST_CA_PEM: &[u8] = include_bytes!("fixtures/tls/ca.pem");
const UNTRUSTED_CA_PEM: &[u8] = include_bytes!("fixtures/tls/untrusted_ca.pem");
const SERVER_CERT_PEM: &[u8] = include_bytes!("fixtures/tls/server.cert.pem");
const SERVER_KEY_PEM: &[u8] = include_bytes!("fixtures/tls/server.key.pem");

fn tls_acceptor() -> TlsAcceptor {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(SERVER_CERT_PEM)
        .map(|c| c.expect("valid test server cert"))
        .collect();
    let key = PrivateKeyDer::from_pem_slice(SERVER_KEY_PEM).expect("valid test server key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("valid TLS protocol versions")
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .expect("valid cert/key pair");
    TlsAcceptor::from(Arc::new(config))
}

async fn spawn_tls_proxy(backend: SocketAddr) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind TLS proxy");
    let addr = listener.local_addr().expect("TLS proxy addr");
    let acceptor = tls_acceptor();
    let task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls_stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls_stream, &mut upstream).await;
            });
        }
    });
    (addr, task)
}

#[tokio::test]
async fn wss_link_performs_chord_handshake_rpc_and_rejects_untrusted_certs() -> Result<()> {
    let contract = Arc::new(Contract::remote(
        "network.counter",
        1,
        vec![],
        IndexMap::from([(
            "main".into(),
            IndexMap::from([
                ("value".into(), Member::State(int())),
                ("add".into(), method(Some(int()), Some(int()))),
                ("who".into(), method(None, Some(str()))),
            ]),
        )]),
        Arc::new(|_| {
            pi_rs_chord::contracts::HostKind::new(
                "server",
                pi_rs_chord::contracts::read_params(&[], "server")?,
            )?
            .address(JsonValue::Object(IndexMap::new()))
        }),
    )?);
    let host = create_host(HostOptions {
        principal: Some(serde_json::json!("server")),
        address: Some(JsonValue::from_serde(
            serde_json::json!({"kind":"server","params":{}}),
        )),
        ..Default::default()
    })?;
    let value = Source::new(host.batch(), JsonValue::Number(1.0))?;
    let writable = value.clone();
    let ty = contract.clone();
    host.load(
        vec![PluginFacet::new(
            "counter",
            define_facet(move |setup| {
                let value = writable.clone();
                setup.serve(
                    ty.clone(),
                    move |_, _, _| {
                        let mut members = Members::variant("main");
                        members.state("value", value.clone())?;
                        let value = value.clone();
                        members.method("add", move |args, _| {
                            let value = value.clone();
                            Box::pin(async move {
                                let (JsonValue::Number(n), JsonValue::Number(amount)) =
                                    (value.value()?.to_json(), args)
                                else {
                                    return Err(pi_rs_chord::Error::Invalid(
                                        "numbers required".into(),
                                    ));
                                };
                                let next = JsonValue::Number(n + amount);
                                value.set(next.clone(), &Context::background())?;
                                Ok(next)
                            })
                        })?;
                        members.method("who", |_, ctx| {
                            Box::pin(async move {
                                Ok(JsonValue::from_serde(
                                    ctx.principal.clone().unwrap_or_default(),
                                ))
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let http = Http::new(host.clone(), HttpOptions::default());
    let mut auth = HttpContribution::new("auth");
    auth.authenticators.push(Arc::new(|r| {
        Box::pin(async move {
            Ok((r.header("authorization") == Some("Bearer tls-test"))
                .then(|| serde_json::json!("reader")))
        })
    }));
    http.contribute(auth)
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    let server = http
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    let (tls_addr, tls_task) = spawn_tls_proxy(server.address).await;
    let mut tls_hosts = HttpContribution::new("tls-hosts");
    tls_hosts.hosts.push(tls_addr.to_string());
    tls_hosts
        .hosts
        .push(format!("localhost:{}", tls_addr.port()));
    http.contribute(tls_hosts)
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    let wss_ip_url = format!("wss://{tls_addr}/_chord/link");
    let wss_host_url = format!("wss://localhost:{}/_chord/link", tls_addr.port());
    let headers = http::HeaderMap::from_iter([(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer tls-test"),
    )]);

    // 1. Default native roots must reject our self-signed test CA.
    assert!(
        connect_websocket(&wss_ip_url, headers.clone(), 1024 * 1024, 8 * 1024 * 1024)
            .await
            .is_err(),
        "default native roots must reject untrusted self-signed cert"
    );

    // 2. A different custom root CA must also reject the server's cert.
    let untrusted = WebSocketTlsConfig::with_custom_root_pem(UNTRUSTED_CA_PEM);
    assert!(
        connect_websocket_with_tls(
            &wss_ip_url,
            headers.clone(),
            1024 * 1024,
            8 * 1024 * 1024,
            Some(&untrusted),
        )
        .await
        .is_err(),
        "wrong custom root CA must be rejected"
    );

    // 3. Invalid PEM must fail cleanly without panicking.
    let bad_pem = WebSocketTlsConfig::with_custom_root_pem(b"not a certificate");
    assert!(
        connect_websocket_with_tls(
            &wss_ip_url,
            headers.clone(),
            1024 * 1024,
            8 * 1024 * 1024,
            Some(&bad_pem),
        )
        .await
        .is_err(),
        "malformed PEM must return Err"
    );

    // 4. Missing auth over valid TLS must fail (HTTP 401).
    let trusted = WebSocketTlsConfig::with_custom_root_pem(TEST_CA_PEM);
    assert!(
        connect_websocket_with_tls(
            &wss_ip_url,
            http::HeaderMap::new(),
            1024 * 1024,
            8 * 1024 * 1024,
            Some(&trusted),
        )
        .await
        .is_err(),
        "missing credentials over wss:// must be rejected"
    );

    // 5. Full Chord handshake + hold + method call + live state over wss:// (IP SAN).
    let link = connect_websocket_with_tls(
        &wss_ip_url,
        headers.clone(),
        1024 * 1024,
        8 * 1024 * 1024,
        Some(&trusted),
    )
    .await?;
    let client = RemotePeer::connect(
        link,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let held = client.hold(
        contract.clone(),
        JsonValue::Object(IndexMap::new()),
        &Context::background(),
    )?;
    tokio::time::timeout(Duration::from_secs(5), held.wait(&Context::background()))
        .await
        .unwrap()?;
    assert_eq!(
        held.member("value")?.value()?.to_json(),
        JsonValue::Number(1.0)
    );
    assert_eq!(
        held.call("who", JsonValue::Null, &Context::background())
            .await?,
        JsonValue::String("reader".into())
    );
    assert_eq!(
        held.call("add", JsonValue::Number(4.0), &Context::background())
            .await?,
        JsonValue::Number(5.0)
    );
    value.set(JsonValue::Number(42.0), &Context::background())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while held.member("value").unwrap().value().unwrap().to_json() != JsonValue::Number(42.0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    held.release();
    client.close();

    // 6. Verify DNS SAN (`wss://localhost:...`) also connects and completes a call.
    let dns_link = connect_websocket_with_tls(
        &wss_host_url,
        headers,
        1024 * 1024,
        8 * 1024 * 1024,
        Some(&trusted),
    )
    .await?;
    let dns_client = RemotePeer::connect(
        dns_link,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let dns_held = dns_client.hold(
        contract,
        JsonValue::Object(IndexMap::new()),
        &Context::background(),
    )?;
    tokio::time::timeout(
        Duration::from_secs(5),
        dns_held.wait(&Context::background()),
    )
    .await
    .unwrap()?;
    assert_eq!(
        dns_held
            .call("who", JsonValue::Null, &Context::background())
            .await?,
        JsonValue::String("reader".into())
    );
    dns_held.release();
    dns_client.close();

    tls_task.abort();
    server
        .close()
        .await
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    host.close(&Context::background()).await?;
    Ok(())
}
