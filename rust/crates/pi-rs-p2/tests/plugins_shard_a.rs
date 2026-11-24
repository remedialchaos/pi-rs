use std::{error::Error, path::Path, sync::Arc, time::Duration};

use pi_rs_chord::{
    context::Context,
    host::{Caller, Held},
    node::workers::WorkerOptions,
    remote::RemoteReference,
};
use pi_rs_p2::{
    api::v1::{node::session_directory, p2_contracts::lookup},
    hosts::server::{P2Server, ServerOptions, start_server},
    plugins,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_archives_clones_and_forks_real_session_workers_with_durable_history() -> TestResult {
    tokio::time::timeout(Duration::from_secs(45), check_lifecycle()).await?
}

async fn check_lifecycle() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start(directory.path()).await?;
    let context = Context::background();
    let owner = sessions(&server, owner_caller(), &context).await?;
    let created: Value = owner
        .call(
            "create",
            json!({
                "cwd":directory.path(), "title":"Trace lifecycle", "thinkingLevel":"off",
                "model":{"provider":"faux","modelId":"faux-1"},
            })
            .into(),
            &context,
        )
        .await?
        .to_serde()?;
    let id = created["id"].as_str().ok_or("Missing session ID")?;
    let guest = sessions(&server, Caller {
        principal: Some(json!({"kind":"guest","id":"reader","name":"Reader","home":id,"sessions":{(id):"view"},"revision":1})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }, &context).await?;
    assert!(
        guest
            .call("archive", json!({"id":id}).into(), &context)
            .await
            .is_err()
    );

    let transfer = hold_worker(
        &server,
        "p2.session-transfer",
        json!({"session":id}),
        server_caller(),
        &context,
    )
    .await?;
    transfer
        .call(
            "seed",
            json!({"entries":[
                {"kind":"p2.input","data":{"text":"kept before split"}},
                {"kind":"p2.input","data":{"text":"omitted by fork"}}
            ]})
            .into(),
            &context,
        )
        .await?;
    let chat = hold_worker(
        &server,
        "p2.chat",
        json!({"session":id,"conversation":1}),
        owner_caller(),
        &context,
    )
    .await?;
    let before: Value = chat
        .call("entries", json!({"limit":10}).into(), &context)
        .await?
        .to_serde()?;
    let split = before["items"]
        .as_array()
        .ok_or("Missing history")?
        .iter()
        .find(|item| item["data"]["text"] == "omitted by fork")
        .ok_or("Missing split point")?["id"]
        .as_u64()
        .ok_or("Missing entry ID")?;

    let files = session_directory(directory.path(), id)?.join("files");
    std::fs::create_dir_all(&files)?;
    std::fs::write(files.join("artifact.txt"), "unchanged artifact")?;
    std::fs::create_dir_all(files.join(".uploads"))?;
    std::fs::write(files.join(".uploads/incomplete"), "unfinished")?;
    assert!(
        owner
            .call(
                "fork",
                json!({"id":id,"conversation":1,"before":split+20_000}).into(),
                &context
            )
            .await
            .is_err()
    );

    let clone: Value = owner
        .call("clone", json!({"id":id}).into(), &context)
        .await?
        .to_serde()?;
    let clone_id = clone["id"].as_str().ok_or("Missing clone ID")?;
    let cloned = hold_worker(
        &server,
        "p2.chat",
        json!({"session":clone_id,"conversation":1}),
        owner_caller(),
        &context,
    )
    .await?;
    let history: Value = cloned
        .call("entries", json!({"limit":10}).into(), &context)
        .await?
        .to_serde()?;
    assert!(history.to_string().contains("omitted by fork"));
    assert_eq!(
        read_artifact(directory.path(), clone_id)?,
        "unchanged artifact"
    );

    let fork: Value = owner
        .call(
            "fork",
            json!({"id":id,"conversation":1,"before":split}).into(),
            &context,
        )
        .await?
        .to_serde()?;
    let fork_id = fork["id"].as_str().ok_or("Missing fork ID")?;
    wait_operations(&owner).await?;
    assert_eq!(
        read_artifact(directory.path(), fork_id)?,
        "unchanged artifact"
    );

    let old_generation = server.workers.generation(id);
    owner
        .call("archive", json!({"id":id}).into(), &context)
        .await?;
    wait_operations(&owner).await?;
    assert!(!server.workers.running().contains(&id.to_owned()));
    let visible: Vec<Value> = serde_json::from_value(guest.read("sessions")?.to_serde()?)?;
    assert!(visible.is_empty());
    owner
        .call("restore", json!({"id":id}).into(), &context)
        .await?;
    chat.call("entries", json!({"limit":10}).into(), &context)
        .await?;
    assert!(server.workers.generation(id) > old_generation);

    for worker in [transfer, chat, cloned] {
        worker.release();
    }
    guest.release();
    owner.release();
    server.close().await?;

    let reopened = start(directory.path()).await?;
    let forked = hold_worker(
        &reopened,
        "p2.chat",
        json!({"session":fork_id,"conversation":1}),
        owner_caller(),
        &context,
    )
    .await?;
    let history: Value = forked
        .call("entries", json!({"limit":10}).into(), &context)
        .await?
        .to_serde()?;
    assert!(history.to_string().contains("kept before split"));
    assert!(!history.to_string().contains("omitted by fork"));
    forked.release();
    reopened.close().await?;
    Ok(())
}

fn read_artifact(data: &Path, id: &str) -> TestResult<String> {
    let files = session_directory(data, id)?.join("files");
    assert!(!files.join(".uploads/incomplete").exists());
    Ok(std::fs::read_to_string(files.join("artifact.txt"))?)
}

async fn wait_operations(owner: &Held) -> TestResult {
    loop {
        let operations: Vec<Value> = serde_json::from_value(owner.read("operations")?.to_serde()?)?;
        if operations.iter().any(|row| row["phase"] == "failed") {
            return Err(format!("Session operation failed: {operations:?}").into());
        }
        if operations.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn owner_caller() -> Caller {
    Caller {
        principal: Some(json!({"kind":"owner","access":"local"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

fn server_caller() -> Caller {
    Caller {
        principal: Some(json!({"kind":"server"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

async fn sessions(server: &P2Server, principal: Caller, context: &Context) -> TestResult<Held> {
    let held = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        principal,
        context,
    )?;
    held.wait(context).await?;
    Ok(held)
}

async fn hold_worker(
    server: &P2Server,
    contract: &str,
    params: Value,
    caller: Caller,
    context: &Context,
) -> TestResult<RemoteReference> {
    Ok(server
        .workers
        .hold(Arc::new(lookup(contract)?), params.into(), caller, context)
        .await?)
}

async fn start(directory: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(directory.to_owned());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    plugins::register_builtins(&mut options.registry)?;
    let mut process = WorkerOptions::new(
        env!("CARGO_BIN_EXE_p2").into(),
        directory.join("worker-locks"),
    );
    process.args.push("--worker".into());
    options.worker_process = Some(process);
    Ok(start_server(options).await?)
}
