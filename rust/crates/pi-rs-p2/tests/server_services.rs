use std::{error::Error, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use http::HeaderValue;
use pi_rs_chord::{
    context::Context,
    data::ChordError,
    host::{Caller, Members, ServeOptions, define_facet},
    json::JsonValue,
    node::{
        app::host_data_contract,
        http::{HttpError, RouteOptions, text_response},
        workers::{ProcessLock, WorkerOptions},
    },
    watch::WatchCall,
};
use pi_rs_p2::{
    api::v1::{FacetKind, OwnerAccess, PluginRegistry, Principal, p2_contracts::lookup},
    hosts::server::{ServerOptions, start_server},
    runtime::server::{
        RelayUser, ResolvedRequest, ServerServices, hosts_contract, server_contract, server_of,
    },
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(10);
const SESSION: &str = "34c4c4c4-a301-47a6-b591-4419d9ebd133";

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> TestResult<String> {
    let mut connection = tokio::net::TcpStream::connect(address).await?;
    connection
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    tokio::time::timeout(TIMEOUT, connection.read_to_string(&mut response)).await??;
    Ok(response)
}

fn owner() -> Principal {
    Principal::Owner {
        access: OwnerAccess::Local,
    }
}

fn guest() -> Principal {
    Principal::Guest {
        id: "guest".into(),
        name: "Guest".into(),
        email: None,
        home: SESSION.into(),
        sessions: Default::default(),
        revision: 1,
    }
}

fn route(label: &'static str) -> RouteOptions {
    RouteOptions::public(move |_| Box::pin(async move { text_response(200, label) }))
}

#[tokio::test]
async fn plugin_routes_choose_the_exact_or_longest_match_and_follow_reload() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut services = ServerServices::default();
    services.route("GET /plugin/*", route("outer"))?;
    services.route("GET /plugin/a/*", route("inner"))?;
    services.route("GET /plugin/a/exact", route("exact"))?;
    let mut options = ServerOptions::new(directory.path().into());
    options.port = 0;
    options
        .registry
        .register("probe", FacetKind::Server, services.facet())?;
    let mut server = start_server(options).await?;
    let natives = server_of(&server.host)?;
    assert_eq!(natives.info.port, server.address.port());
    assert_eq!(natives.plugin_root, directory.path().join("plugins"));
    let root = natives.root();
    let info = root.hold(
        Arc::new(server_contract()?),
        JsonValue::from_serde(json!({})),
    )?;
    let context = Context::background();
    info.wait(&context).await?;
    assert_eq!(
        info.member("info")?.value()?.to_json().to_serde()?["port"],
        server.address.port()
    );
    drop(root);
    for (path, expected) in [
        ("/plugin/x", "outer"),
        ("/plugin/a/x", "inner"),
        ("/plugin/a/exact", "exact"),
    ] {
        let response = request(server.address, "GET", path, "").await?;
        assert!(response.ends_with(expected), "{response}");
    }

    let mut rejected = ServerServices::default();
    rejected.route("GET /plugin/a/exact", route("uncommitted"))?;
    let mut replacement = PluginRegistry::new();
    replacement.register(
        "probe",
        FacetKind::Server,
        rejected.with_facet(|_| {
            Err(pi_rs_chord::Error::Invalid(
                "fixture rejects replacement".into(),
            ))
        }),
    )?;
    assert_eq!(server.reload(&replacement, None).await?.failed.len(), 1);
    assert!(
        request(server.address, "GET", "/plugin/a/exact", "")
            .await?
            .ends_with("exact")
    );
    let mut next = ServerServices::default();
    next.route("GET /plugin/a/exact", route("replacement"))?;
    let mut replacement = PluginRegistry::new();
    replacement.register("probe", FacetKind::Server, next.facet())?;
    assert!(server.reload(&replacement, None).await?.failed.is_empty());
    assert!(
        request(server.address, "GET", "/plugin/a/exact", "")
            .await?
            .ends_with("replacement")
    );
    assert_eq!(
        server.reload(&PluginRegistry::new(), None).await?.removed,
        ["probe"]
    );
    let removed = request(server.address, "GET", "/plugin/a/exact", "").await?;
    assert!(removed.starts_with("HTTP/1.1 401"), "{removed}");
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn owner_authentication_wins_and_only_owner_app_pages_renew_cookies() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut guests = ServerServices::default();
    guests.authenticators.insert(
        "guest".into(),
        Arc::new(|request, _| {
            Box::pin(async move { Ok((request.header("x-guest") == Some("yes")).then(guest)) })
        }),
    );
    let mut owners = ServerServices::default();
    owners.authenticators.insert(
        "owner".into(),
        Arc::new(|request, origin| {
            Box::pin(async move {
                Ok((request.header("x-owner") == Some("yes") && !origin.remote).then(owner))
            })
        }),
    );
    for (name, value) in [
        ("first", HeaderValue::from_static("first=renewed; Path=/")),
        ("second", HeaderValue::from_static("second=renewed; Path=/")),
    ] {
        owners.page_cookies.insert(
            name.into(),
            Arc::new(move |_, access| {
                access
                    .principal
                    .as_ref()
                    .filter(|principal| principal.is_owner())
                    .map(|_| value.clone())
            }),
        );
    }
    owners.route(
        "GET /principal",
        RouteOptions::new(|request| {
            Box::pin(async move {
                let principal = request
                    .principal
                    .as_ref()
                    .ok_or_else(|| HttpError::new(401, "missing principal"))?;
                let kind = principal["kind"]
                    .as_str()
                    .ok_or_else(|| HttpError::new(500, "missing kind"))?;
                text_response(200, kind)
            })
        }),
    )?;
    let mut options = ServerOptions::new(directory.path().into());
    options.port = 0;
    options
        .registry
        .register("guest-probe", FacetKind::Server, guests.facet())?;
    options
        .registry
        .register("owner-probe", FacetKind::Server, owners.facet())?;
    let mut server = start_server(options).await?;
    let both = "x-guest: yes\r\nx-owner: yes\r\n";
    assert!(
        request(server.address, "GET", "/principal", both)
            .await?
            .ends_with("owner")
    );
    assert!(
        request(server.address, "GET", "/principal", "x-guest: yes\r\n")
            .await?
            .ends_with("guest")
    );
    for method in ["GET", "HEAD"] {
        let page = request(server.address, method, "/", both).await?;
        assert!(
            page.contains("set-cookie: first=renewed; Path=/\r\n"),
            "{page}"
        );
        assert!(
            page.contains("set-cookie: second=renewed; Path=/\r\n"),
            "{page}"
        );
    }
    for (path, headers) in [
        ("/", "x-guest: yes\r\n"),
        ("/", "x-owner: yes\r\nsec-fetch-site: cross-site\r\n"),
        ("/principal", both),
        ("/s/not-a-session", both),
        ("/s/deadbeef/c/01", both),
    ] {
        let response = request(server.address, "GET", path, headers).await?;
        assert!(!response.contains("set-cookie:"), "{response}");
    }
    let missing = request(server.address, "GET", "/principal", "").await?;
    assert!(missing.starts_with("HTTP/1.1 401"));

    let mut without_owner = PluginRegistry::new();
    let mut guest_only = ServerServices::default();
    guest_only.authenticators.insert(
        "guest".into(),
        Arc::new(|_, _| Box::pin(async { Ok(Some(guest())) })),
    );
    without_owner.register("guest-probe", FacetKind::Server, guest_only.facet())?;
    assert!(server.reload(&without_owner, None).await?.failed.is_empty());
    assert!(
        !request(server.address, "GET", "/", both)
            .await?
            .contains("set-cookie:")
    );
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_relay_resolver_can_admit_a_verified_remote_user() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut services = ServerServices::default();
    let proof = uuid::Uuid::new_v4().to_string();
    let expected_proof = proof.clone();
    services.request_resolvers.insert(
        "verified-probe".into(),
        Arc::new(move |headers| {
            Ok((headers
                .get("x-probe-proof")
                .and_then(|header| header.to_str().ok())
                == Some(&expected_proof))
            .then(|| ResolvedRequest {
                origin: "https://relay.example".into(),
                shell_origin: Some("https://shell.example".into()),
                user: Some(RelayUser {
                    id: "relay-user".into(),
                    email: "test@example.com".into(),
                }),
                principal: None,
            }))
        }),
    );
    services.authenticators.insert(
        "relayed-guest".into(),
        Arc::new(|_, origin| {
            Box::pin(async move {
                Ok((origin.remote
                    && origin.origin == "https://relay.example"
                    && origin.relay_shell_origin.as_deref() == Some("https://shell.example")
                    && origin
                        .relay_user
                        .is_some_and(|user| user.email == "test@example.com"))
                .then(guest))
            })
        }),
    );
    services.route(
        "GET /relayed",
        RouteOptions::new(|request| Box::pin(async move { text_response(200, request.origin) })),
    )?;
    let mut options = ServerOptions::new(directory.path().into());
    options.port = 0;
    options
        .registry
        .register("relay-probe", FacetKind::Server, services.facet())?;
    let server = start_server(options).await?;
    let forwarded = "x-forwarded-host: relay.example\r\n";
    let forged = request(server.address, "GET", "/relayed", forwarded).await?;
    assert!(forged.starts_with("HTTP/1.1 403"), "{forged}");
    let health = request(
        server.address,
        "GET",
        "/health",
        "x-p2-proxy-secret: forged\r\n",
    )
    .await?;
    assert!(health.starts_with("HTTP/1.1 403"), "{health}");
    assert!(health.contains("content-type: text/plain\r\n"), "{health}");
    assert!(health.ends_with("Localhost only"), "{health}");
    let headers = format!("{forwarded}x-probe-proof: {proof}\r\nOrigin: https://relay.example\r\n");
    let verified = request(server.address, "GET", "/relayed", &headers).await?;
    assert!(
        verified.starts_with("HTTP/1.1 200") && verified.ends_with("https://relay.example"),
        "{verified}"
    );
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn hosts_follows_the_supervised_child_and_stop_replaces_it_for_holders() -> TestResult {
    let directory = tempfile::tempdir()?;
    let row = json!({
        "id": SESSION, "env": null, "cwd": directory.path(), "title": "Workers",
        "createdAt": 1, "updatedAt": 1, "archivedAt": null, "pinned": false,
        "model": {"provider": "fake", "modelId": "fake"}, "thinkingLevel": "off", "main": 1, "branches": [],
    });
    let catalog = Arc::new(lookup("p2.catalog")?);
    let mut options = ServerOptions::new(directory.path().into());
    options.port = 0;
    options.fake = true;
    let mut process = WorkerOptions::new(
        PathBuf::from(env!("CARGO_BIN_EXE_p2")),
        directory.path().join("worker-locks"),
    );
    process.args.push("--worker".into());
    process.idle_timeout = Duration::from_millis(350);
    options.worker_process = Some(process);
    options.registry.register(
        "fixture-catalog",
        FacetKind::Server,
        define_facet(move |host| {
            let row = row.clone();
            host.serve(
                catalog.clone(),
                move |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "sessions",
                        object.writable(JsonValue::from_serde(json!([row])))?,
                    )?;
                    let row = row.clone();
                    members.method("get", move |args, _| {
                        let row = row.clone();
                        Box::pin(async move {
                            if args.to_serde()?["id"] != row["id"] {
                                return Err(ChordError::new(
                                    "invalid",
                                    "Fixture session not found",
                                )
                                .into());
                            }
                            Ok(JsonValue::from_serde(row))
                        })
                    })?;
                    members.method("create", |_, _| {
                        Box::pin(async { Err(pi_rs_chord::Error::Closed) })
                    })?;
                    members.method("update", |_, _| {
                        Box::pin(async { Err(pi_rs_chord::Error::Closed) })
                    })?;
                    members.method("activity", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;
    let server = start_server(options).await?;
    let context = Context::background();
    let natives = server_of(&server.host)?;
    let hosts = server
        .host
        .get(
            Arc::new(hosts_contract()?),
            JsonValue::from_serde(json!({})),
            &context,
        )
        .await?;
    let workers = hosts.member("workers")?;
    assert_eq!(workers.value()?.to_json().to_serde()?, json!({}));
    assert_eq!(natives.workers.generation(SESSION), 0);
    let (send, mut states) = tokio::sync::mpsc::unbounded_channel();
    let _subscription = workers.subscribe(
        move |call, _| {
            if let WatchCall::Value { value, .. } = call {
                let _ = send.send(value.to_serde()?);
            }
            Ok(())
        },
        |_| {},
    );
    let held = natives
        .workers
        .hold(
            host_data_contract()?,
            JsonValue::from_serde(
                json!({"address":pi_rs_p2::api::v1::session_address(SESSION).to_string()}),
            ),
            Caller {
                principal: Some(json!({"kind":"server"})),
                incarnation: server.host.incarnation().into(),
            },
            &context,
        )
        .await?;
    tokio::time::timeout(TIMEOUT, held.wait(&context))
        .await
        .map_err(|_| "first child did not bind its hold")??;
    let first = next_state(&mut states, "first running state", |state| {
        state["running"] == true
    })
    .await?;
    let first_pid = first["pid"].as_u64().ok_or("first child pid missing")?;
    assert!(first_pid > 0);
    let first_generation = first["generation"]
        .as_u64()
        .ok_or("first child generation missing")?;
    assert!(first_generation > 0);
    assert_eq!(natives.workers.running(), [SESSION]);
    assert_eq!(natives.workers.generation(SESSION), first_generation);
    assert!(
        directory
            .path()
            .join("sessions")
            .join(SESSION)
            .join("store.sqlite")
            .is_file()
    );

    let lock = std::fs::read_dir(directory.path().join("server/worker-locks"))?
        .next()
        .ok_or("missing worker process lock")??
        .path();
    assert!(ProcessLock::try_acquire(&lock)?.is_none());
    assert_eq!(
        tokio::time::timeout(
            TIMEOUT,
            hosts.call(
                "stop",
                JsonValue::from_serde(json!({"session": SESSION})),
                &context
            )
        )
        .await
        .map_err(|_| "stop did not return after the child exited")??,
        JsonValue::Null,
    );
    let second = next_state(&mut states, "replacement running state", |state| {
        state["running"] == true
            && state["generation"]
                .as_u64()
                .is_some_and(|generation| generation > first_generation)
    })
    .await?;
    assert_ne!(second["pid"], first_pid);
    tokio::time::timeout(TIMEOUT, held.wait(&context))
        .await
        .map_err(|_| "replacement child did not bind the existing hold")??;
    held.release();
    let stopped = next_state(&mut states, "replacement idle exit", |state| {
        state["generation"] == second["generation"]
            && state["running"] == false
            && state.get("pid").is_none()
    })
    .await?;
    assert!(stopped.get("error").is_none());
    assert!(ProcessLock::try_acquire(&lock)?.is_some());
    server.close().await?;
    Ok(())
}

async fn next_state(
    states: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    event: &str,
    matches: impl Fn(&Value) -> bool,
) -> TestResult<Value> {
    let mut last = Value::Null;
    tokio::time::timeout(TIMEOUT, async {
        while let Some(update) = states.recv().await {
            if matches(&update[SESSION]) {
                return Ok(update[SESSION].clone());
            }
            last = update[SESSION].clone();
        }
        Err("worker state subscription ended".into())
    })
    .await
    .map_err(|_| format!("timed out waiting for {event} (last observed: {last})"))?
}
