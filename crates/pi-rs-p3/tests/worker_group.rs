#![cfg(unix)]

use std::time::Duration;

use pi_rs_p3::{
    install::WorkerSpec,
    runner::{ProcessLauncher, WorkerLaunch, WorkerLauncher},
};

#[tokio::test]
async fn killing_a_worker_closes_the_pipe_held_by_its_descendant()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let ready = dir.path().join("ready");
    let worker = ProcessLauncher.launch(WorkerLaunch {
        session: "group-test".into(),
        spec: WorkerSpec {
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 10 & touch \"$READY\"; wait".into()],
        },
        env: vec![("READY".into(), ready.display().to_string())],
    })?;
    let link = worker.link().ok_or("worker has no pipe")?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    worker.kill().await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !link.closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
