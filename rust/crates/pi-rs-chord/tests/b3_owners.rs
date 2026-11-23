#![cfg(unix)]
#[path = "fixtures/b3_worker.rs"]
mod worker;

use pi_rs_chord::{
    context::Context,
    contracts::Address,
    json::JsonValue,
    node::{
        app::{AppOptions, AppServer, AppWorkerKind, ArchiveLimits, start_server},
        http::connect_websocket,
        workers::WorkerOptions,
    },
    remote::{RemoteOptions, RemotePeer, RemoteReference},
    state::Batch,
    store::HostDataExisting,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

// Ask Cargo for the executable so this also works with a filtered test run.
fn worker_executable() -> std::io::Result<PathBuf> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "build",
            "--offline",
            "-j",
            "1",
            "--example",
            "b3-worker",
            "--message-format=json",
        ])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: serde_json::Value = serde_json::from_slice(line)?;
        if message["target"]["name"] == "b3-worker"
            && let Some(path) = message["executable"].as_str()
        {
            return Ok(PathBuf::from(path));
        }
    }
    Err(std::io::Error::other(
        "Cargo did not report the worker fixture executable",
    ))
}
fn params(id: &str) -> JsonValue {
    JsonValue::from_serde(json!({"id":id}))
}
fn address(id: &str) -> Address {
    worker::kind().address(params(id)).unwrap()
}
async fn app(dir: &Path) -> pi_rs_chord::Result<(AppServer, Arc<AtomicUsize>)> {
    let child = worker_executable()?;
    let count = Arc::new(AtomicUsize::new(0));
    let starts = count.clone();
    let mut process = WorkerOptions::new(child, dir.join("unused"));
    process.start_timeout = Duration::from_secs(10);
    let mut kind = AppWorkerKind::new(
        worker::kind(),
        process,
        |address| json!({"kind":"worker","key":address.key}),
        |principal, address| principal["kind"] == "owner" && address.params != params("private"),
    );
    kind.contracts.push(worker::contract());
    kind.start = Arc::new(move |address, _| {
        starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(address.params.to_serde()?["id"].clone()) })
    });
    let mut options = AppOptions::new(
        "test",
        dir.join("app"),
        json!({"kind":"owner"}),
        json!({"kind":"server"}),
    );
    options.workers.push(kind);
    Ok((start_server(options).await?, count))
}
async fn who(reference: &RemoteReference) -> serde_json::Value {
    let context = Context::background();
    tokio::time::timeout(Duration::from_secs(8), async {
        reference.wait(&context).await.unwrap();
        reference
            .call("who", JsonValue::Null, &context)
            .await
            .unwrap()
            .to_serde()
            .unwrap()
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn worker_kind_routes_two_addresses_and_stamps_the_authenticated_caller()
-> pi_rs_chord::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let (app, starts) = app(dir.path()).await?;
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    let mut headers = http::HeaderMap::new();
    let issued = app
        .sign_ins
        .issue(json!({"kind":"owner"}), "worker test", "device", false)
        .await
        .unwrap();
    headers.insert(
        "authorization",
        http::HeaderValue::from_str(&format!("Bearer {}", issued.token)).unwrap(),
    );
    let link = connect_websocket(
        &format!("ws://{}/_chord/link", app.address),
        headers,
        1024 * 1024,
        8 * 1024 * 1024,
    )
    .await
    .unwrap();
    let peer = RemotePeer::connect(
        link.clone(),
        RemoteOptions::new(json!({"kind":"server"})),
        Batch::new(|_| {}),
    )
    .unwrap();
    let context = Context::background();
    let one = peer
        .hold(worker::contract(), params("one"), &context)
        .unwrap();
    let also_one = peer
        .hold(worker::contract(), params("one"), &context)
        .unwrap();
    let two = peer
        .hold(worker::contract(), params("two"), &context)
        .unwrap();
    assert_eq!(
        who(&one).await,
        json!({"id":"one","principal":{"kind":"owner"}})
    );
    assert_eq!(who(&also_one).await["id"], "one");
    assert_eq!(who(&two).await["id"], "two");
    let denied = peer
        .hold(worker::contract(), params("private"), &context)
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), denied.wait(&context))
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    tokio::time::timeout(Duration::from_secs(12), app.close())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !link.closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    Ok(())
}
#[tokio::test]
async fn live_export_replace_and_delete_transfer_only_the_selected_address()
-> pi_rs_chord::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let (app, starts) = app(dir.path()).await?;
    let context = Context::background();
    let one = app
        .hold_worker(worker::contract(), params("one"), &context)
        .await
        .unwrap();
    let two = app
        .hold_worker(worker::contract(), params("two"), &context)
        .await
        .unwrap();
    assert_eq!(who(&one).await["principal"], json!({"kind":"server"}));
    assert_eq!(who(&two).await["id"], "two");
    let first = dir.path().join("first.tar");
    app.export_data(&address("one"), &first, &context)
        .await
        .unwrap();
    assert_eq!(who(&one).await["id"], "one");
    assert!(
        app.import_data(
            &first,
            &address("two"),
            HostDataExisting::Refuse,
            ArchiveLimits::default()
        )
        .await
        .is_err()
    );
    assert_eq!(who(&two).await["id"], "two");
    app.import_data(
        &first,
        &address("two"),
        HostDataExisting::Replace,
        ArchiveLimits::default(),
    )
    .await
    .unwrap();
    assert!(two.call("who", JsonValue::Null, &context).await.is_err());
    assert_eq!(who(&one).await["id"], "one");
    let replacement = app
        .hold_worker(worker::contract(), params("two"), &context)
        .await
        .unwrap();
    assert_eq!(who(&replacement).await["id"], "two");
    assert_eq!(starts.load(Ordering::SeqCst), 3);
    let second = dir.path().join("second.tar");
    app.export_data(&address("two"), &second, &context)
        .await
        .unwrap();
    let entries =
        pi_rs_chord::node::app::read_tar(&std::fs::read(second).unwrap(), ArchiveLimits::default())
            .unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.starts_with("files/") && entry.bytes == b"one")
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.path.starts_with("files/") && entry.bytes == b"two")
    );
    app.delete_data(&address("two")).await.unwrap();
    assert!(
        replacement
            .call("who", JsonValue::Null, &context)
            .await
            .is_err()
    );
    assert!(
        app.export_data(&address("two"), &dir.path().join("deleted.tar"), &context)
            .await
            .is_err()
    );
    assert_eq!(who(&one).await["id"], "one");
    app.close().await
}
