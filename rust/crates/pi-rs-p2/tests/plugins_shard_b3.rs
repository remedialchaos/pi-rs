use std::{error::Error, path::Path, process::Command, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use pi_rs_chord::{
    context::Context,
    host::{Caller, Held},
    node::workers::{Control, Frame, WorkerSession, write_frame},
};
use pi_rs_p2::{
    api::v1::{PluginRegistry, p2_contracts::lookup, session_address},
    hosts::worker::start_session_worker,
    plugins::shard_b3,
};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
const SESSION: &str = "workspace-test";

async fn call(target: &Held, method: &str, args: Value) -> TestResult<Value> {
    Ok(target
        .call(method, args.into(), &Context::background())
        .await?
        .to_serde()?)
}

fn git(cwd: &Path, args: &[&str]) -> TestResult<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn workspace_queries_live_git_and_files() -> TestResult {
    workspace_case(WorkspaceCase::Query).await
}

#[tokio::test]
async fn workspace_transfer_enforces_ranges_and_confinement() -> TestResult {
    workspace_case(WorkspaceCase::Transfer).await
}

#[tokio::test]
async fn workspace_upload_commits_or_aborts_atomically() -> TestResult {
    workspace_case(WorkspaceCase::Upload).await
}

enum WorkspaceCase {
    Query,
    Transfer,
    Upload,
}

async fn workspace_case(case: WorkspaceCase) -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), exercise_workspace(case)).await?
}

async fn exercise_workspace(case: WorkspaceCase) -> TestResult {
    let fixture = TempDir::new()?;
    let cwd = fixture.path().join("project");
    std::fs::create_dir(&cwd)?;
    git(&cwd, &["init", "-q"])?;
    std::fs::write(cwd.join("note.txt"), "original\n")?;
    git(&cwd, &["add", "note.txt"])?;
    git(
        &cwd,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Initial note",
        ],
    )?;
    let commit = git(&cwd, &["rev-parse", "HEAD"])?;
    let commit = commit.trim();
    std::fs::write(cwd.join("note.txt"), "updated text\n")?;
    std::fs::write(cwd.join("sample.bin"), [0, 1, 2, 3, 4])?;
    std::fs::write(fixture.path().join("outside.txt"), "outside")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(fixture.path().join("outside.txt"), cwd.join("escape.txt"))?;

    let (mut parent, child) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(child);
    write_frame(&mut parent,&Frame::Control(Control::Start {
        address: session_address(SESSION),
        start: json!({
            "app":fixture.path(), "dataDir":fixture.path(), "fake":true, "telemetry":false,"generation":1,
            "values":{"session":{"id":SESSION,"cwd":cwd,"model":{"provider":"faux","modelId":"faux-1"},"thinkingLevel":"off","resume":true}},
        }),
        scripts:vec![],lock:fixture.path().join("worker.lock"),
    })).await?;
    let session = WorkerSession::start(reader, writer).await?;
    let mut registry = PluginRegistry::new();
    shard_b3::register(&mut registry)?;
    let worker = start_session_worker(session, &registry, Some(&["workspace".into()])).await?;
    let ctx = Context::background();
    let hold = |name, principal| -> TestResult<Held> {
        Ok(worker.host.hold_as(
            Arc::new(lookup(name)?),
            json!({"session":SESSION,"conversation":1}).into(),
            Caller {
                principal: Some(principal),
                incarnation: worker.host.incarnation().into(),
            },
            &ctx,
        )?)
    };
    let owner = json!({"kind":"owner","access":"local"});
    let view = hold("p2.workspace", owner.clone())?;
    view.wait(&ctx).await?;
    let rejected = hold("p2.workspace-transfer", owner)?;
    assert!(
        matches!(rejected.wait(&ctx).await,Err(pi_rs_chord::Error::Chord(error)) if error.code == "forbidden")
    );
    let transfer = hold("p2.workspace-transfer", json!({"kind":"server"}))?;
    transfer.wait(&ctx).await?;
    match case {
        WorkspaceCase::Query => workspace_queries(&view, commit).await?,
        WorkspaceCase::Transfer => workspace_transfer(&transfer).await?,
        WorkspaceCase::Upload => workspace_upload(&transfer, &cwd).await?,
    }
    drop(transfer);
    drop(rejected);
    drop(view);
    worker.close().await?;
    Ok(())
}

async fn workspace_queries(view: &Held, commit: &str) -> TestResult {
    let query = |args| call(view, "query", args);
    let listing = query(json!({"kind":"list","path":""})).await?;
    assert!(
        listing["entries"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "note.txt")
                && rows.iter().all(|row| row["name"] != ".git"))
    );
    assert_eq!(
        query(json!({"kind":"file","path":"sample.bin"})).await?["binary"],
        true
    );
    assert_eq!(
        query(json!({"kind":"file","path":"note.txt","commit":commit})).await?["text"],
        "original\n"
    );
    assert_eq!(
        query(json!({"kind":"status"})).await?["changes"][0],
        json!({"path":"note.txt","status":".M"})
    );
    let patch = query(json!({"kind":"diff","path":"note.txt"})).await?;
    assert!(
        patch["patch"]
            .as_str()
            .is_some_and(|s| s.contains("-original\n+updated text\n"))
    );
    let search =
        query(json!({"kind":"search","path":"","text":"updated","include":"*.{md,txt}"})).await?;
    assert_eq!(
        search["matches"],
        json!([{"path":"note.txt","line":1,"text":"updated text"}])
    );
    assert_eq!(
        query(json!({"kind":"commit","commit":commit})).await?["changes"],
        json!([{"status":"A.","path":"note.txt"}])
    );
    assert_eq!(
        query(json!({"kind":"log","skip":0,"limit":1})).await?["commits"][0]["hash"],
        commit
    );
    Ok(())
}

async fn workspace_transfer(transfer: &Held) -> TestResult {
    let bytes = call(
        transfer,
        "read",
        json!({"path":"sample.bin","start":1,"end":2}),
    )
    .await?;
    assert_eq!(
        bytes,
        json!({"size":5,"start":1,"data":STANDARD.encode([1,2])})
    );
    assert!(
        call(transfer, "read", json!({"path":"../outside.txt"}))
            .await
            .is_err()
    );
    #[cfg(unix)]
    assert!(
        call(transfer, "read", json!({"path":"escape.txt"}))
            .await
            .is_err()
    );
    Ok(())
}

async fn workspace_upload(transfer: &Held, cwd: &Path) -> TestResult {
    let file = cwd.join("upload.txt");
    let upload = |offset, data, final_chunk, abort| {
        call(
            transfer,
            "write",
            json!({
                "path":"","name":"upload.txt","token":"12345678-1234-4234-8234-123456789abc",
                "offset":offset,"data":data,"final":final_chunk,"abort":abort
            }),
        )
    };
    upload(0, "YQ==", false, false).await?;
    assert!(!file.exists());
    upload(1, "Yg==", true, false).await?;
    assert_eq!(std::fs::read(&file)?, b"ab");
    assert!(upload(0, "Yw==", true, false).await.is_err());
    std::fs::remove_file(&file)?;
    upload(0, "YQ==", false, false).await?;
    upload(1, "", false, true).await?;
    assert!(upload(1, "Yg==", true, false).await.is_err());
    assert!(!file.exists());
    Ok(())
}
