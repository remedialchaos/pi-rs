use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use pi_rs_chord::{
    context::Context,
    data::ChordError,
    host::{Members, RefState, ServeOptions, define_facet},
    json::JsonValue,
    node::{
        app::{ArchiveLimits, host_data_contract, read_tar},
        http::{HttpContribution, connect_websocket},
        workers::{ProcessLock, WorkerOptions},
    },
    remote::{RemoteOptions, RemotePeer, RemoteReference},
    state::Batch,
};
use pi_rs_p2::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup, session_address},
    hosts::server::{P2Server, ServerOptions, start_server},
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ID: &str = "92dc44af-a301-47a6-b591-4419d9ebd133";
const IDLE: Duration = Duration::from_millis(350);
const TIMEOUT: Duration = Duration::from_secs(15);

async fn fixture(
    root: &Path,
    rows: Vec<Value>,
    reads: Arc<AtomicUsize>,
    configure: impl FnOnce(&mut ServerOptions, &mut WorkerOptions) -> TestResult,
) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(root.to_owned());
    options.port = 0;
    options.fake = true;
    let mut process = WorkerOptions::new(
        PathBuf::from(env!("CARGO_BIN_EXE_p2")),
        root.join("worker-locks"),
    );
    process.args.push("--worker".into());
    process.idle_timeout = IDLE;

    let catalog = Arc::new(lookup("p2.catalog")?);
    options.registry.register(
        "sessions",
        FacetKind::Server,
        define_facet(move |host| {
            let rows = rows.clone();
            let reads = reads.clone();
            host.serve(
                catalog.clone(),
                move |_, _, this| {
                    let mut members = Members::new();
                    let sessions = this.writable(JsonValue::from_serde(json!(rows)))?;
                    members.state("sessions", sessions.clone())?;
                    let reader = sessions.clone();
                    let reads = reads.clone();
                    members.method("get", move |args, _| {
                        let reader = reader.clone();
                        let reads = reads.clone();
                        Box::pin(async move {
                            reads.fetch_add(1, Ordering::SeqCst);
                            let id = args.to_serde()?["id"].clone();
                            let rows: Vec<Value> =
                                serde_json::from_value(reader.value()?.to_json().to_serde()?)?;
                            let row = rows
                                .into_iter()
                                .find(|row| row["id"] == id)
                                .ok_or_else(|| ChordError::new("invalid", "Unknown session"))?;
                            Ok(JsonValue::from_serde(row))
                        })
                    })?;
                    let creator = sessions.clone();
                    members.method("create", move |args, ctx| {
                        let creator = creator.clone();
                        Box::pin(async move {
                            let args = args.to_serde()?;
                            let row = row(&args["cwd"], Value::Null);
                            let mut rows: Vec<Value> =
                                serde_json::from_value(creator.value()?.to_json().to_serde()?)?;
                            rows.push(row.clone());
                            creator.set(JsonValue::from_serde(json!(rows)), &ctx.context)?;
                            Ok(JsonValue::from_serde(row))
                        })
                    })?;
                    members.method("activity", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    members.method("update", move |args, ctx| {
                        let sessions = sessions.clone();
                        Box::pin(async move {
                            let args = args.to_serde()?;
                            let mut rows: Vec<Value> =
                                serde_json::from_value(sessions.value()?.to_json().to_serde()?)?;
                            let row = rows
                                .iter_mut()
                                .find(|row| row["id"] == args["id"])
                                .ok_or_else(|| ChordError::new("invalid", "Unknown session"))?;
                            row["archivedAt"] = Value::Null;
                            let updated = row.clone();
                            sessions.set(JsonValue::from_serde(json!(rows)), &ctx.context)?;
                            Ok(JsonValue::from_serde(updated))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;

    let mut auth = HttpContribution::new("test-auth");
    let revision_file = root.join("grant-revision");
    auth.authenticators.push(Arc::new(move |request| {
        let revision_file = revision_file.clone();
        Box::pin(async move {
            Ok(match request.headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
                // A disposable credential permits a real server-only contract over the test link.
                Some("Bearer internal-test") => Some(json!({"kind":"server"})),
                Some("Bearer guest-test") => Some(json!({
                    "kind":"guest", "id":"guest", "name":"Guest", "home":"", "sessions":{}, "revision":1
                })),
                Some("Bearer grant-test") => tokio::fs::read_to_string(revision_file).await?
                    .trim().parse::<u64>().ok().map(|revision| json!({
                        "kind":"guest", "id":"guest", "name":"Guest", "home":"", "sessions":{(ID): "view"}, "revision":revision
                    })),
                Some("Bearer host-test") => Some(json!({"kind":"host"})),
                _ => None,
            })
        })
    }));
    options
        .registry
        .register("test-auth", FacetKind::Server, auth.facet())?;
    configure(&mut options, &mut process)?;
    options.worker_process = Some(process);
    Ok(start_server(options).await?)
}

fn row(cwd: &Value, archived: Value) -> Value {
    json!({
        "id": ID, "env": null, "cwd": cwd, "title": "Routing", "createdAt": 1,
        "updatedAt": 1, "archivedAt": archived, "pinned": false,
        "model": {"provider": "fake", "modelId": "fake"},
        "thinkingLevel": "off", "main": 1, "branches": [],
    })
}

async fn client(server: &P2Server, token: Option<&str>) -> TestResult<RemotePeer> {
    let mut headers = http::HeaderMap::new();
    if let Some(token) = token {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }
    let link = connect_websocket(
        &format!("ws://{}/chord?client=script", server.address),
        headers,
        1024 * 1024,
        4 * 1024 * 1024,
    )
    .await?;
    Ok(RemotePeer::connect(
        link,
        RemoteOptions::new(json!({"kind":"server"})),
        Batch::new(|_| {}),
    )?)
}

fn hold(client: &RemotePeer, context: &Context) -> TestResult<RemoteReference> {
    Ok(client.hold(
        host_data_contract()?,
        JsonValue::from_serde(json!({"address":session_address(ID).to_string()})),
        context,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_holds_share_a_native_worker_until_the_last_release() -> TestResult {
    let dir = tempfile::tempdir()?;
    let reads = Arc::new(AtomicUsize::new(0));
    let server = fixture(dir.path(), vec![], reads.clone(), |_, process| {
        // Expire the idle timer before the worker's first 50 ms activity poll.
        process.idle_timeout = Duration::from_millis(10);
        Ok(())
    })
    .await?;
    let context = Context::background();
    let anonymous = client(&server, None).await.err();
    assert!(
        matches!(
            anonymous.as_deref().and_then(|error| error.downcast_ref::<pi_rs_chord::Error>()),
            Some(pi_rs_chord::Error::Invalid(message))
                if matches!(message.as_str(), "HTTP error: 401 Unauthorized" | "HTTP error: 403 Forbidden")
        ),
        "anonymous WebSocket upgrade was not rejected by admission: {anonymous:?}"
    );
    let guest = client(&server, Some("guest-test")).await?;
    let denied = hold(&guest, &context)?;
    let result = tokio::time::timeout(TIMEOUT, denied.wait(&context))
        .await
        .map_err(|_| "guest hold did not receive a denial")?;
    assert!(matches!(result, Err(pi_rs_chord::Error::Chord(error)) if error.code == "forbidden"));
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let client = client(&server, Some("internal-test")).await?;
    let catalog = server
        .host
        .get(
            Arc::new(lookup("p2.catalog")?),
            JsonValue::from_serde(json!({})),
            &context,
        )
        .await?;
    catalog.call("create", JsonValue::from_serde(json!({
        "id": ID, "cwd": dir.path(), "title": "Routing", "model": {"provider":"fake","modelId":"fake"}, "thinkingLevel": "off"
    })), &context).await?;
    let first = hold(&client, &context)?;
    let second = hold(&client, &context)?;
    let (first_ready, second_ready) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(first.wait(&context), second.wait(&context))
    })
    .await
    .map_err(|_| {
        format!(
            "shared holds did not bind: first={:?}, second={:?}, worker={:?}, catalog reads={}",
            first.state().value(),
            second.state().value(),
            server.workers.observe().borrow().get(ID),
            reads.load(Ordering::SeqCst),
        )
    })?;
    first_ready?;
    second_ready?;
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    assert_worker_export(dir.path(), &second, &context).await?;

    let lock = fs::read_dir(dir.path().join("server/worker-locks"))?
        .next()
        .ok_or("child process lock is missing")??
        .path();
    first.release();
    tokio::time::sleep(IDLE * 2).await;
    assert!(
        ProcessLock::try_acquire(&lock)?.is_none(),
        "second hold keeps the child alive"
    );
    second.release();
    drop(ProcessLock::acquire(&lock, TIMEOUT).await?);
    let third = hold(&client, &context)?;
    tokio::time::timeout(TIMEOUT, third.wait(&context))
        .await
        .map_err(|_| {
            format!(
                "hold after idle shutdown did not bind: state={:?}, worker={:?}, catalog reads={}",
                third.state().value(),
                server.workers.observe().borrow().get(ID),
                reads.load(Ordering::SeqCst),
            )
        })??;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    third.release();
    server.close().await?;
    assert!(ProcessLock::try_acquire(&lock)?.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_rebinds_the_same_remote_hold_after_catalog_refusal() -> TestResult {
    let dir = tempfile::tempdir()?;
    let reads = Arc::new(AtomicUsize::new(0));
    let server = fixture(
        dir.path(),
        vec![row(&json!(dir.path()), json!(2))],
        reads.clone(),
        |_, _| Ok(()),
    )
    .await?;
    let context = Context::background();
    let client = client(&server, Some("internal-test")).await?;
    let first = hold(&client, &context)?;
    let failed = tokio::time::timeout(TIMEOUT, first.wait(&context)).await?;
    assert!(matches!(failed, Err(pi_rs_chord::Error::Chord(error))
        if error.code == "unavailable" && error.message == "worker failed to start"));
    assert!(!dir.path().join("sessions").join(ID).exists());

    let catalog = server
        .host
        .get(
            Arc::new(lookup("p2.catalog")?),
            JsonValue::from_serde(json!({})),
            &context,
        )
        .await?;
    catalog
        .call(
            "update",
            JsonValue::from_serde(json!({"id":ID,"archived":false})),
            &context,
        )
        .await?;
    let second = hold(&client, &context)?;
    assert!(
        tokio::time::timeout(TIMEOUT, second.wait(&context))
            .await?
            .is_err(),
        "failure is sticky until retry"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    first.retry()?;
    tokio::time::timeout(TIMEOUT, async {
        while !matches!(first.state().value(), RefState::Live) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        second.wait(&context).await
    })
    .await??;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert!(
        dir.path()
            .join("sessions")
            .join(ID)
            .join("store.sqlite")
            .is_file()
    );
    server.close().await?;
    Ok(())
}

async fn assert_worker_export(
    root: &Path,
    held: &RemoteReference,
    context: &Context,
) -> TestResult {
    fs::write(
        root.join("sessions").join(ID).join("files/probe"),
        b"worker-owned file",
    )?;
    let source = root.join("server.archive");
    let destination = root.join("result.tar");
    fs::write(&source, b"disposable server archive")?;
    let manifest = json!({"format":2,"app":"p2","api":"v1","address":{"kind":"session","key":session_address(ID)["key"]}});
    tokio::time::timeout(
        TIMEOUT,
        held.call(
            "exportTo",
            JsonValue::from_serde(json!({
                "path":destination, "manifest":manifest.to_string(), "server":source
            })),
            context,
        ),
    )
    .await
    .map_err(|_| "worker export exceeded its deadline")??;
    let entries = read_tar(&fs::read(destination)?, ArchiveLimits::default())?;
    assert!(
        entries
            .iter()
            .any(|entry| { entry.path == "files/probe" && entry.bytes == b"worker-owned file" })
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_remote_hold_rebinds_after_worker_is_killed() -> TestResult {
    use pi_rs_chord::watch::NativeWatchCall;

    let dir = tempfile::tempdir()?;
    let pids = dir.path().join("worker-pids");
    let reads = Arc::new(AtomicUsize::new(0));
    let server = fixture(
        dir.path(),
        vec![row(&json!(dir.path()), Value::Null)],
        reads.clone(),
        |_, process| {
            let worker = std::mem::replace(&mut process.executable, PathBuf::from("/bin/sh"));
            process.args.splice(
                ..0,
                [
                    "-c".into(),
                    r#"printf '%s\n' "$$" >> "$P2_TEST_PIDS"; exec "$@""#.into(),
                    "p2-test-worker".into(),
                    worker.to_string_lossy().into_owned(),
                ],
            );
            process
                .env
                .insert("P2_TEST_PIDS".into(), pids.to_string_lossy().into_owned());
            Ok(())
        },
    )
    .await?;
    let context = Context::background();
    let client = client(&server, Some("internal-test")).await?;
    let held = hold(&client, &context)?;
    tokio::time::timeout(TIMEOUT, held.wait(&context))
        .await
        .map_err(|_| "original native worker never bound")??;
    // Wait for dispatch to finish publishing the initial binding before observing it.
    tokio::time::timeout(TIMEOUT, client.fence())
        .await
        .map_err(|_| "initial binding fence exceeded its deadline")??;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let first: u32 = fs::read_to_string(&pids)?.trim().parse()?;
    let worker = fs::canonicalize(env!("CARGO_BIN_EXE_p2"))?;
    assert_eq!(fs::read_link(format!("/proc/{first}/exe"))?, worker);
    let (send, mut states) = tokio::sync::mpsc::unbounded_channel();
    let _observation = held.state().observe(
        &context,
        move |call, _| {
            if let NativeWatchCall::Value { value } = call {
                let _ = send.send(value);
            }
            Ok(())
        },
        |_| {},
    )?;

    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", "--", &first.to_string()])
            .status()?
            .success()
    );
    let disconnected = tokio::time::timeout(TIMEOUT, states.recv())
        .await
        .map_err(|_| "client did not observe the worker disconnect")?;
    assert!(
        matches!(disconnected, Some(RefState::Loading)),
        "after crash: {disconnected:?}"
    );
    let rebound = tokio::time::timeout(TIMEOUT, states.recv())
        .await
        .map_err(|_| "the same hold did not bind to the replacement worker")?;
    assert!(
        matches!(rebound, Some(RefState::Live)),
        "after restart: {rebound:?}"
    );
    let workers = fs::read_to_string(&pids)?;
    let second: u32 = workers
        .lines()
        .nth(1)
        .ok_or("replacement PID missing")?
        .parse()?;
    assert_ne!(first, second);
    assert_eq!(workers.lines().count(), 2);
    assert_eq!(fs::read_link(format!("/proc/{second}/exe"))?, worker);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_worker_export(dir.path(), &held, &context).await?;
    server.close().await?;
    Ok(())
}

async fn routed_state(held: &RemoteReference) -> TestResult<RefState> {
    let outcome = tokio::time::timeout(TIMEOUT, async {
        loop {
            let state = held.state().value();
            if !matches!(state, RefState::Loading) {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "session hold had no routing outcome before its deadline")?;
    Ok(outcome)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_guest_hold_requires_a_current_grant_and_an_active_authorizer() -> TestResult {
    let dir = tempfile::tempdir()?;
    let revision_file = dir.path().join("grant-revision");
    fs::write(&revision_file, "1")?;
    let reads = Arc::new(AtomicUsize::new(0));
    let chat = Arc::new(lookup("p2.chat")?);
    let mut registry = PluginRegistry::new();
    let mut server = fixture(
        dir.path(),
        vec![row(&json!(dir.path()), Value::Null)],
        reads.clone(),
        |options, _| {
            let revision_file = revision_file.clone();
            options.registry.register_session_authorizer(
                "sharing-test",
                move |_, principal, session, _| {
                    let file = revision_file.clone();
                    Box::pin(async move {
                        let Principal::Guest {
                            revision, sessions, ..
                        } = principal
                        else {
                            return Ok(false);
                        };
                        let current = tokio::fs::read_to_string(file).await?;
                        Ok(sessions.contains_key(&session)
                            && current.trim().parse::<u64>().ok() == Some(revision))
                    })
                },
            )?;
            registry = options.registry.clone();
            Ok(())
        },
    )
    .await?;
    let context = Context::background();
    let hold_chat = |peer: &RemotePeer| {
        peer.hold(
            chat.clone(),
            JsonValue::from_serde(json!({"session": ID, "conversation":1})),
            &context,
        )
    };
    let guest = client(&server, Some("grant-test")).await?;
    let privileged = hold(&guest, &context)?;
    let rejected = routed_state(&privileged).await?;
    assert!(
        matches!(&rejected, RefState::Failed { cause, .. } if cause.error.code == "forbidden"),
        "guest privileged hold: {rejected:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let current = hold_chat(&guest)?;
    let reached = routed_state(&current).await?;
    assert!(
        matches!(&reached, RefState::Live),
        "current guest hold: {reached:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(
        dir.path()
            .join("sessions")
            .join(ID)
            .join("store.sqlite")
            .is_file()
    );
    let lock = fs::read_dir(dir.path().join("server/worker-locks"))?
        .next()
        .ok_or("child process lock is missing")??
        .path();
    current.release();
    // `p2.chat` lingers for 30s per upstream spec; stop explicitly so we can assert
    // the next unauthorized holds do not spawn a new worker.
    server.workers.stop(ID).await?;
    drop(ProcessLock::acquire(&lock, TIMEOUT).await?);

    fs::write(&revision_file, "2")?;
    let stale = hold_chat(&guest)?;
    let rejected = routed_state(&stale).await?;
    assert!(
        matches!(&rejected, RefState::Failed { cause, .. } if cause.error.code == "forbidden"),
        "superseded guest hold: {rejected:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(ProcessLock::try_acquire(&lock)?.is_some());

    let selected: Vec<_> = server
        .plugins
        .iter()
        .filter(|name| *name != "sharing-test")
        .cloned()
        .collect();
    let report = server.reload(&registry, Some(&selected)).await?;
    assert!(report.failed.is_empty());
    let fresh = client(&server, Some("grant-test")).await?;
    let missing_policy = hold_chat(&fresh)?;
    let rejected = routed_state(&missing_policy).await?;
    assert!(
        matches!(&rejected, RefState::Failed { cause, .. } if cause.error.code == "forbidden"),
        "guest hold without an active authorizer: {rejected:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(ProcessLock::try_acquire(&lock)?.is_some());

    let internal = client(&server, Some("host-test")).await?;
    let privileged = hold(&internal, &context)?;
    let rejected = routed_state(&privileged).await?;
    assert!(
        matches!(&rejected, RefState::Failed { cause, .. } if cause.error.code == "forbidden"),
        "host privileged hold: {rejected:?}"
    );
    let allowed = hold_chat(&internal)?;
    let reached = routed_state(&allowed).await?;
    // Routing admits `Principal::Host` to non-privileged worker contracts (spawning the worker),
    // where `p2.chat`'s own viewer check then rejects non-viewer principals with "viewers only".
    assert!(
        matches!(
            &reached,
            RefState::Failed { cause, .. }
                if cause.error.code == "forbidden" && cause.error.message == "viewers only"
        ),
        "internal host hold: {reached:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    server.close().await?;
    Ok(())
}
