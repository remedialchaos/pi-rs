use std::{error::Error as StdError, path::Path, sync::Arc, time::Duration};

use pi_rs_chord::{
    Error,
    context::Context,
    host::{Caller, Held, Members, ServeOptions, define_facet},
    json::JsonValue,
    node::workers::{Control, Frame, WorkerSession, write_frame},
};
use serde_json::{Value, json};
use tokio::io::DuplexStream;

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, session_address},
    hosts::worker::{SessionWorker, start_session_worker},
};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const SESSION: &str = "chat";
const TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test]
async fn chat_admits_one_owner_turn_across_restart_and_keeps_viewers_read_only() -> TestResult {
    let directory = tempfile::tempdir()?;
    let registry = registry()?;
    let context = Context::background();
    let (worker, _parent) = start(directory.path(), &registry).await?;
    let owner = chat(&worker, json!({"kind":"owner","access":"local"}), &context).await?;
    let viewer = chat(&worker, json!({"kind":"guest","id":"reader","name":"Reader","home":SESSION,"sessions":{"chat":"view"},"revision":1}), &context).await?;
    assert!(
        matches!(viewer.call("send", json!({"text":"unauthorized","requestId":"viewer"}).into(), &context).await, Err(Error::Chord(error)) if error.code == "invalid")
    );
    let other = chat(&worker, json!({"kind":"guest","id":"outsider","name":"Outsider","home":"other","sessions":{"other":"full"},"revision":1}), &context).await;
    assert!(
        matches!(other, Err(error) if error.downcast_ref::<Error>().is_some_and(|error| matches!(error, Error::Chord(cause) if cause.code == "forbidden")))
    );

    let sent = owner
        .call(
            "send",
            json!({"text":"durable chat turn","requestId":"same"}).into(),
            &context,
        )
        .await?;
    let result: Value = sent.to_serde()?;
    let id = result["submissionId"]
        .as_u64()
        .ok_or("Missing submission ID")?;
    let harness = worker.process.harness()?;
    let submission = harness
        .durable
        .submission(id, &context)
        .await?
        .ok_or("Missing submission")?;
    let outcome = tokio::time::timeout(TIMEOUT, submission.wait(&context)).await??;
    assert_eq!(outcome.status, pi_rs_durable::SubmissionStatus::Done);
    viewer.release();
    owner.release();
    drop(harness);
    worker.close().await?;

    let (worker, _parent) = start(directory.path(), &registry).await?;
    let owner = chat(&worker, json!({"kind":"owner","access":"local"}), &context).await?;
    assert_eq!(
        owner
            .call(
                "send",
                json!({"text":"do not duplicate","requestId":"same"}).into(),
                &context
            )
            .await?,
        sent
    );
    let view: Value = owner.member("view")?.value()?.to_json().to_serde()?;
    let entries = view["entries"].as_array().ok_or("Missing entries")?;
    let shown = entries
        .iter()
        .filter(|entry| entry["kind"] == "p2.input")
        .collect::<Vec<_>>();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["data"]["text"], "durable chat turn");
    assert!(entries.iter().any(|entry| entry["kind"] == "pi.assistant"));
    let model: Value = owner.member("model")?.value()?.to_json().to_serde()?;
    assert_eq!(model["modelId"], "faux-1");
    owner.release();
    worker.close().await?;
    Ok(())
}

fn registry() -> TestResult<PluginRegistry> {
    let mut registry = PluginRegistry::new();
    super::register(&mut registry)?;
    let status = Arc::new(lookup("p2.status-report")?);
    registry.register(
        "test-status",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                status.clone(),
                |_, _, _| {
                    let mut members = Members::new();
                    members.method("push", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;
    Ok(registry)
}

async fn chat(worker: &SessionWorker, principal: Value, context: &Context) -> TestResult<Held> {
    let held = worker.host.hold_as(
        Arc::new(lookup("p2.chat")?),
        json!({"session":SESSION,"conversation":1}).into(),
        Caller {
            principal: Some(principal),
            incarnation: "chat-test".into(),
        },
        context,
    )?;
    tokio::time::timeout(TIMEOUT, held.wait(context)).await??;
    Ok(held)
}

async fn start(
    directory: &Path,
    registry: &PluginRegistry,
) -> TestResult<(SessionWorker, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(child);
    write_frame(
        &mut parent,
        &Frame::Control(Control::Start {
            address: session_address(SESSION),
            start: json!({
                "app":directory,"dataDir":directory,"generation":1,"fake":true,"telemetry":false,
                "values":{"session":{
                    "id":SESSION,"cwd":directory,"model":{"provider":"faux","modelId":"faux-1"},
                    "thinkingLevel":"off","resume":true,
                }},
            }),
            scripts: Vec::new(),
            lock: directory.join("worker.lock"),
        }),
    )
    .await?;
    let worker = tokio::time::timeout(TIMEOUT, WorkerSession::start(read, write)).await??;
    let worker =
        tokio::time::timeout(TIMEOUT, start_session_worker(worker, registry, None)).await??;
    Ok((worker, parent))
}

#[tokio::test]
async fn shell_persists_display_only_output_and_denies_overlapping_commands() -> TestResult {
    let directory = tempfile::tempdir()?;
    let registry = registry()?;
    let context = Context::background();
    let (worker, _parent) = start(directory.path(), &registry).await?;
    let owner = chat(&worker, json!({"kind":"owner","access":"local"}), &context).await?;
    let args = json!({"command":"printf 'disposable output\\n'; while [ ! -f release-shell ]; do sleep 0.05; done","includeInContext":false,"requestId":"shell-1"});
    let id = owner.call("shell", args.clone().into(), &context).await?;
    assert_eq!(id, json!({"shellId":"shell-1"}).into());
    assert_eq!(owner.call("shell", args.into(), &context).await?, id);
    assert!(
        owner
            .call(
                "shell",
                json!({
                    "command":"printf unwanted","includeInContext":true,"requestId":"shell-2"
                })
                .into(),
                &context
            )
            .await
            .is_err()
    );
    tokio::fs::write(directory.path().join("release-shell"), b"").await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let value: Value = owner.member("view")?.value()?.to_json().to_serde()?;
            if value["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|entry| entry["kind"] == "p2.shell")
            {
                return pi_rs_chord::Result::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    owner.release();
    worker.close().await?;

    let (worker, _parent) = start(directory.path(), &registry).await?;
    let owner = chat(&worker, json!({"kind":"owner","access":"local"}), &context).await?;
    let view: Value = owner.member("view")?.value()?.to_json().to_serde()?;
    let outputs = view["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["kind"] == "p2.shell")
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["data"]["output"], "disposable output");
    assert_eq!(outputs[0]["data"]["exitCode"], 0);
    assert!(outputs[0].get("model").is_none());
    owner.release();
    worker.close().await?;
    Ok(())
}
