use pi_rs_chord::node::{plugins::digest, workers::*};
use serde_json::json;
use std::{sync::Arc, time::Duration};
#[tokio::test]
async fn worker_ipc_bounds_control_and_binary_frames() {
    let (mut a, mut b) = tokio::io::duplex(1024);
    write_frame(&mut a, &Frame::Control(Control::Ready { idle: true }))
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut b).await.unwrap(),
        Frame::Control(Control::Ready { idle: true })
    ));
    write_frame(&mut a, &Frame::Binary(vec![0, 1, 255]))
        .await
        .unwrap();
    assert!(
        matches!(read_frame(&mut b).await.unwrap(),Frame::Binary(bytes) if bytes==vec![0,1,255])
    );
    use tokio::io::AsyncWriteExt;
    a.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
    assert!(read_frame(&mut b).await.is_err());
    assert!(
        DeliveredPlugin {
            name: "bad".into(),
            digest: "wrong".into(),
            bytes: vec![]
        }
        .validate()
        .is_err()
    );
}
#[tokio::test]
async fn locks_exclude_other_open_files_and_release_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.lock");
    let lock = ProcessLock::try_acquire(&path).unwrap().unwrap();
    assert!(ProcessLock::try_acquire(&path).unwrap().is_none());
    assert!(
        ProcessLock::acquire(&path, Duration::from_millis(30))
            .await
            .is_err()
    );
    drop(lock);
    assert!(ProcessLock::try_acquire(&path).unwrap().is_some());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workers_demand_start_deliver_plugins_idle_stop_and_never_restart_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("worker-fixture");
    let result = std::process::Command::new("rustc")
        .args([
            "--edition=2024",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/worker.rs"),
            "-o",
        ])
        .arg(&binary)
        .status()
        .unwrap();
    assert!(result.success());
    let marker = dir.path().join("starts");
    let mut options = WorkerOptions::new(binary, dir.path().join("locks"));
    options
        .env
        .insert("STARTS".into(), marker.to_string_lossy().into());
    options.idle_timeout = Duration::from_millis(150);
    options.shutdown_timeout = Duration::from_millis(100);
    let workers = Workers::new(options).unwrap();
    assert!(!marker.exists());
    let script = DeliveredPlugin {
        name: "plug".into(),
        digest: digest(b"plugin"),
        bytes: b"plugin".to_vec(),
    };
    let handle = workers
        .demand(json!({"key":"a"}), json!(null), vec![script])
        .await
        .unwrap();
    handle.wait_ready().await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "1");
    assert!(Arc::ptr_eq(
        &handle,
        &workers
            .demand(json!({"key":"a"}), json!(null), vec![])
            .await
            .unwrap()
    ));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));
    handle.link.on_message(Arc::new(move |bytes| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(bytes);
        }
    }));
    handle.link.send(b"hello".to_vec()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap(),
        b"hello"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), handle.wait_exit())
            .await
            .unwrap(),
        WorkerState::Stopped
    );
    let crash = workers
        .demand(json!({"key":"crash"}), json!(null), vec![])
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), crash.wait_exit())
            .await
            .unwrap(),
        WorkerState::Failed(_)
    ));
    assert!(Arc::ptr_eq(
        &crash,
        &workers
            .demand(json!({"key":"crash"}), json!(null), vec![])
            .await
            .unwrap()
    ));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "11");
    workers.retry(&json!({"key":"crash"})).await.unwrap();
    let retry = workers
        .demand(json!({"key":"crash"}), json!(null), vec![])
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&retry, &crash));
    retry.wait_exit().await;
    workers.close().await;
}
