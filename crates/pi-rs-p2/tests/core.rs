use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use pi_rs_chord::node::http::{HttpContribution, RouteOptions, text_response};
use pi_rs_p2::{
    api::v1::{FacetKind, PluginRegistry},
    hosts::server::{ServerOptions, start_server},
    upstream,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult = Result<(), Box<dyn Error>>;

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> Result<String, Box<dyn Error>> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await??;
    Ok(response)
}

#[tokio::test]
async fn server_publishes_health_and_reloads_registered_routes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut routes = HttpContribution::new("counter");
    routes.route(
        "GET /counter",
        RouteOptions::public(|_| Box::pin(async { text_response(200, "loaded") })),
    )?;
    let mut registry = PluginRegistry::new();
    registry.register("counter", FacetKind::Server, routes.clone().facet())?;
    assert!(
        registry
            .register("counter", FacetKind::Server, routes.facet())
            .is_err()
    );
    let mut options = ServerOptions::new(directory.path().to_owned());
    options.port = 0;
    options.registry = registry;
    let mut server = start_server(options).await?;
    assert!(server.address.ip().is_loopback());
    assert!(directory.path().join("server/store.sqlite").is_file());

    let health = request(server.address, "GET", "/health", "").await?;
    assert!(health.starts_with("HTTP/1.1 200"));
    assert!(health.contains("content-type: application/json\r\n"));
    assert!(health.ends_with(r#"{"ok":true,"name":"p2"}"#));
    let head = request(server.address, "HEAD", "/health", "").await?;
    assert!(head.starts_with("HTTP/1.1 200"));
    assert!(head.ends_with("\r\n\r\n"));
    let refused = request(
        server.address,
        "GET",
        "/health",
        "Origin: https://foreign.example\r\n",
    )
    .await?;
    assert!(refused.starts_with("HTTP/1.1 403"));
    assert!(refused.ends_with("Invalid origin"));

    assert!(health.contains("wasm-unsafe-eval"));
    let manifest = request(server.address, "GET", "/manifest.webmanifest", "").await?;
    assert!(manifest.contains("cache-control: public, max-age=86400"));
    assert!(manifest.ends_with(include_str!("../assets/public/manifest.webmanifest")));
    let sign_in = request(server.address, "GET", "/", "").await?;
    assert!(sign_in.starts_with("HTTP/1.1 401"));
    assert!(sign_in.contains("<h1>Sign in to Pi</h1>"));

    let loaded = request(server.address, "GET", "/counter", "").await?;
    assert!(loaded.ends_with("loaded"));
    let report = server.reload(&PluginRegistry::new(), None).await?;
    assert_eq!(report.removed, ["counter"]);
    let removed = request(server.address, "GET", "/counter", "").await?;
    assert!(removed.starts_with("HTTP/1.1 401"));
    server.close().await?;
    Ok(())
}

#[test]
fn discovers_upstream_aliases_and_respects_plugin_root_precedence() -> TestResult {
    let fixture_repo = tempfile::tempdir()?;
    let app_dir = fixture_repo.path().join("apps/p2");
    std::fs::create_dir_all(app_dir.join("src/api/v1"))?;
    std::fs::create_dir_all(app_dir.join("src/hosts"))?;
    std::fs::create_dir_all(app_dir.join("plugins"))?;
    std::fs::write(app_dir.join("src/api/v1/index.ts"), "")?;
    std::fs::write(app_dir.join("src/hosts/server.ts"), "")?;
    std::fs::write(
        fixture_repo.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"paths":{}}}"#,
    )?;
    std::fs::write(
        app_dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"paths":{"@p2/app/v1":["./src/api/v1/index.ts"]}}}"#,
    )?;

    let app = upstream::find_app(fixture_repo.path())?.ok_or("missing fixture app")?;
    let resolver = upstream::SourceResolver::open(&app)?;
    assert_eq!(
        resolver.resolve("@p2/app/v1")?,
        Some(app.join("src/api/v1/index.ts"))
    );

    let directory = tempfile::tempdir()?;
    let builtin = directory.path().join("builtin");
    let user = directory.path().join("user");
    for root in [&builtin, &user] {
        std::fs::create_dir_all(root.join("counter"))?;
    }
    let server = builtin.join("counter/server.ts");
    std::fs::write(&server, "")?;
    std::fs::write(builtin.join("counter/server.tsx"), "")?;
    std::fs::write(user.join("counter/worker.ts"), "")?;
    let listing = upstream::list_roots(&[builtin, user])?;
    assert_eq!(
        listing["counter"][&FacetKind::Server],
        server.canonicalize()?
    );
    assert!(!listing["counter"].contains_key(&FacetKind::Worker));
    Ok(())
}

#[tokio::test]
async fn worker_opens_matching_session_and_refuses_mismatched_address() -> TestResult {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use pi_rs_chord::{
        context::Context,
        host::{Caller, HostOptions, create_host},
        json::JsonValue,
        node::{
            app::host_data_contract,
            workers::{WorkerOptions, Workers},
        },
    };
    use pi_rs_p2::api::v1::{server::SessionWorkers, session_address};
    use serde_json::json;

    let directory = tempfile::tempdir()?;
    let mut options = WorkerOptions::new(
        Path::new(env!("CARGO_BIN_EXE_p2")).to_owned(),
        directory.path().join("locks"),
    );
    options.args.push("--worker".into());
    let workers = Workers::new(options.clone())?;
    let start = json!({
        "app": "",
        "dataDir": directory.path(),
        "fake": true,
        "telemetry": false,
        "generation": 1,
        "values": {"session":{"id":"active","cwd":"/","model":{"provider":"faux","modelId":"faux-1"},"thinkingLevel":"off"}}
    });
    let host = create_host(HostOptions {
        principal: Some(json!({"kind":"server"})),
        ..Default::default()
    })?;
    let starts = Arc::new(AtomicUsize::new(0));
    let observed = starts.clone();
    let valid = start.clone();
    let sessions = SessionWorkers::new(&host, options, move |_, _, _| {
        observed.fetch_add(1, Ordering::Relaxed);
        let valid = valid.clone();
        Box::pin(async move { Ok(serde_json::from_value(valid)?) })
    })?;
    let context = Context::background();
    let contract = host_data_contract()?;
    let params = JsonValue::from_serde(json!({"address":session_address("active").to_string()}));
    let caller = |kind: &str| Caller {
        principal: Some(json!({"kind":kind,"access":"local"})),
        incarnation: host.incarnation().into(),
    };
    assert!(
        sessions
            .hold(contract.clone(), params.clone(), caller("owner"), &context)
            .await
            .is_err()
    );
    assert_eq!(starts.load(Ordering::Relaxed), 0);
    let server = Caller {
        principal: Some(json!({"kind":"server"})),
        incarnation: host.incarnation().into(),
    };
    let (first, second) = tokio::try_join!(
        sessions.hold(contract.clone(), params.clone(), server.clone(), &context),
        sessions.hold(contract, params, server, &context),
    )?;
    tokio::time::timeout(Duration::from_secs(3), first.wait(&context)).await??;
    tokio::time::timeout(Duration::from_secs(3), second.wait(&context)).await??;
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    drop((first, second));
    sessions.close().await;
    host.close(&context).await?;
    let wrong = workers
        .demand(session_address("other"), start, vec![])
        .await?;
    let refused = wrong.wait_ready().await;
    workers.close().await;
    assert!(refused.is_err());
    assert!(
        directory
            .path()
            .join("sessions/active/store.sqlite")
            .is_file()
    );
    assert!(!directory.path().join("sessions/other").exists());
    Ok(())
}
