use std::{error::Error as StdError, path::Path, process::Stdio, sync::Arc, time::Duration};

use pi_rs_ai::{
    AssistantMessage, InputContent, StopReason,
    faux::{faux_assistant_message, faux_text, faux_tool_call},
};
use pi_rs_chord::{
    context::Context,
    node::workers::{Control, Frame, WorkerSession, read_frame, write_frame},
};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID, Session, SubmissionStatus,
    harness::{Extension, ToolRegistration, ensure_provider_session_id},
    session::SessionOptions,
    tools::ToolResult,
};
use pi_rs_p2::{
    api::v1::{PluginRegistry, session_address},
    hosts::worker::{Activity, start_session_worker, start_session_worker_configured},
    runtime::{session_store::open_session_storage, session_worker::Location},
};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncRead, DuplexStream};

const TIMEOUT: Duration = Duration::from_secs(10);
type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

async fn handshake(
    directory: &Path,
    cwd: &Path,
    model_id: &str,
    resume: bool,
) -> TestResult<(WorkerSession, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (child_read, child_write) = tokio::io::split(child);
    write_frame(&mut parent, &start_frame(directory, cwd, model_id, resume)).await?;
    let worker =
        tokio::time::timeout(TIMEOUT, WorkerSession::start(child_read, child_write)).await??;
    Ok((worker, parent))
}

fn start_frame(directory: &Path, cwd: &Path, model_id: &str, resume: bool) -> Frame {
    Frame::Control(Control::Start {
        address: session_address("sample"),
        start: json!({
            "app": directory,
            "dataDir": directory,
            "fake": true,
            "telemetry": false,
            "generation": 1,
            "values": { "session": {
                "id": "sample",
                "cwd": cwd,
                "model": { "provider": "faux", "modelId": model_id },
                "thinkingLevel": "off",
                "resume": resume,
            } },
        }),
        scripts: Vec::new(),
        lock: directory.join("worker.lock"),
    })
}

async fn next_control(parent: &mut (impl AsyncRead + Unpin)) -> TestResult<Control> {
    Ok(tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Frame::Control(control) = read_frame(parent).await?
                && !matches!(control, Control::Shutdown)
            {
                return Ok::<_, pi_rs_chord::Error>(control);
            }
        }
    })
    .await??)
}

#[tokio::test]
async fn production_worker_runs_fake_turn_and_reopens_the_same_history() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();
    let (session, mut parent) = handshake(fixture.path(), fixture.path(), "faux-1", true).await?;
    let worker = start_session_worker(session, &PluginRegistry::new(), None).await?;
    assert!(matches!(next_control(&mut parent).await?, Control::Started));
    assert!(matches!(
        next_control(&mut parent).await?,
        Control::Ready { idle: true }
    ));
    let harness = worker.process.harness()?;
    let root = harness.durable.root(&context).await?;
    assert_eq!(root.id, ROOT_CONVERSATION_ID);
    let input = serde_json::from_value(json!({"type":"input","content":"worker persistence"}))?;
    let submission = tokio::time::timeout(TIMEOUT, async {
        let submission = root.submit(input, &context).await?;
        let result = submission.wait(&context).await?;
        assert_eq!(result.status, SubmissionStatus::Done);
        harness.durable.wait_for_idle(&context).await?;
        Ok::<_, pi_rs_durable::session::Error>(submission.id)
    })
    .await??;
    let entries = root
        .entries(Default::default(), 32, None, &context)
        .await?
        .items;
    let messages: Vec<_> = entries
        .iter()
        .flat_map(|entry| entry.model.iter().flatten())
        .collect();
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant"
            && message["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["text"] == "Echo: worker persistence")
            })
    }));
    let provider_session =
        ensure_provider_session_id(&harness.durable.session, root.id, &context).await?;
    let run = tokio::spawn(worker.run());
    write_frame(&mut parent, &Frame::Control(Control::Shutdown)).await?;
    tokio::time::timeout(TIMEOUT, run).await???;

    // Start data is only a seed. Reopening keeps the original model and entry identities.
    let (session, _parent) = handshake(fixture.path(), fixture.path(), "unused", false).await?;
    let reopened = start_session_worker(session, &PluginRegistry::new(), None).await?;
    let harness = reopened.process.harness()?;
    assert_eq!(
        harness.durable.inspect(&context).await?.scheduling,
        "paused"
    );
    let root = harness.durable.root(&context).await?;
    assert_eq!(
        root.agent(&context).await?.state["model"]["modelId"],
        "faux-1"
    );
    assert_eq!(
        root.entries(Default::default(), 32, None, &context)
            .await?
            .items,
        entries
    );
    assert_eq!(
        ensure_provider_session_id(&harness.durable.session, root.id, &context).await?,
        provider_session,
    );
    assert_eq!(
        harness
            .durable
            .submission(submission, &context)
            .await?
            .ok_or("reopened worker lost its submission")?
            .status(&context)
            .await?
            .status,
        SubmissionStatus::Done,
    );
    reopened.close().await?;
    Ok(())
}

#[tokio::test]
async fn child_worker_reports_ready_only_after_durable_session_initialization() -> TestResult {
    let fixture = TempDir::new()?;
    let directory = fixture.path();
    for model_id in ["unknown", "faux-1"] {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_p2"))
            .arg("--worker")
            .env("P2_DATA_DIR", directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().ok_or("missing child stdin")?;
        let mut output = child.stdout.take().ok_or("missing child stdout")?;
        write_frame(
            &mut input,
            &start_frame(directory, directory, model_id, false),
        )
        .await?;
        assert!(matches!(next_control(&mut output).await?, Control::Started));
        let control = next_control(&mut output).await?;
        if model_id == "faux-1" {
            assert!(matches!(control, Control::Ready { .. }), "{control:?}");
            let context = Context::background();
            let storage = open_session_storage(directory, "sample").await?;
            let session = Session::new(storage.storage.clone(), SessionOptions::default());
            assert!(
                session
                    .conversation(ROOT_CONVERSATION_ID, &context)
                    .await?
                    .is_some()
            );
            session.close(&context).await?;
            storage.close().await?;
            write_frame(&mut input, &Frame::Control(Control::Shutdown)).await?;
            assert!(
                tokio::time::timeout(TIMEOUT, child.wait())
                    .await??
                    .success()
            );
        } else {
            assert!(
                matches!(&control, Control::Fatal { message } if message == "Unknown model"),
                "{control:?}"
            );
            assert!(
                !tokio::time::timeout(TIMEOUT, child.wait())
                    .await??
                    .success()
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn production_worker_fences_admission_and_moves_the_root_before_notifying() -> TestResult {
    let fixture = TempDir::new()?;
    let destination = fixture.path().join("other");
    std::fs::create_dir(&destination)?;
    let context = Context::background();
    let (session, _parent) = handshake(fixture.path(), fixture.path(), "faux-1", true).await?;
    let worker = start_session_worker(session, &PluginRegistry::new(), None).await?;
    let process = worker.process.clone();
    let harness = process.harness()?;
    let location = Location {
        cwd: destination.to_string_lossy().into_owned(),
        env: None,
    };
    let mut moves = process.location.subscribe();
    assert!(
        harness
            .boundary
            .prepare(Some(location.clone()), context.clone())
            .await?
            .accepted
    );
    assert_eq!(process.liveness().active_calls, 1);
    assert!(process.call(async { Ok(()) }).await.is_err());
    harness
        .boundary
        .relocate(location.clone(), context.clone())
        .await?;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, moves.recv()).await?,
        Some(location.clone())
    );
    assert_eq!(process.location.version(), 1);
    let root = harness.durable.root(&context).await?;
    assert_eq!(root.agent(&context).await?.state["cwd"], location.cwd);
    assert!(process.liveness().idle());
    let viewer = process.hold_activity(Activity::Viewer);
    assert_eq!(process.liveness().viewers, 1);
    drop(viewer);
    assert!(process.liveness().idle());
    worker.close().await?;
    assert!(process.stopping());
    Ok(())
}

#[tokio::test]
async fn native_extension_switches_control_deferred_tool_discovery_in_open_worker() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();
    let (session, _parent) = handshake(fixture.path(), fixture.path(), "faux-1", true).await?;
    let fake = pi_rs_ai::faux::faux_provider(Default::default());
    let models = pi_rs_ai::registry::Models::default();
    models.set_provider(fake.provider.clone());
    let worker =
        start_session_worker_configured(session, &PluginRegistry::new(), None, models, |process| {
            process.registries.extensions.install(Extension {
                name: "fixture".into(),
                tools: vec![ToolRegistration {
                    declaration: serde_json::from_value(json!({
                        "name":"lookup","description":"Lookup project tokens",
                        "parameters":{"type":"object","properties":{}}
                    }))?,
                    sequential: false,
                    prepare_arguments: None,
                    cancel: None,
                    structured_output_schema: None,
                    output_limits: None,
                    execute: Arc::new(|_, _, _| {
                        Box::pin(async {
                            Ok(ToolResult {
                                output: Some(vec![InputContent::Text("native lookup ran".into())]),
                                ..Default::default()
                            }
                            .into())
                        })
                    }),
                    replay_safe: true,
                    callers: Some(vec!["model".into()]),
                    deferred: true,
                }],
                ..Default::default()
            })?;
            Ok(Vec::new())
        })
        .await?;
    let process = worker.process.clone();
    let harness = process.harness()?;
    let root = harness.durable.root(&context).await?;
    assert!(
        root.agent(&context)
            .await?
            .tools
            .iter()
            .any(|tool| tool.declaration.name == "tool_search")
    );
    process.registries.extensions.install(Extension {
        name: "codemode".into(),
        ..Default::default()
    })?;
    assert!(
        harness
            .durable
            .registry
            .snapshot()
            .extension("tool-search")
            .is_none()
    );
    process
        .registries
        .extensions
        .switch("codemode", Some(false))?;
    assert!(
        harness
            .durable
            .registry
            .snapshot()
            .extension("tool-search")
            .is_some()
    );
    fake.set_responses([
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "tool_search",
                serde_json::from_value(json!({"query":"project tokens"}))?,
                Some("find-lookup".into()),
            )])
        }
        .into(),
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "lookup",
                Default::default(),
                Some("run-lookup".into()),
            )])
        }
        .into(),
        faux_assistant_message(vec![faux_text("all done")]).into(),
    ]);
    let input =
        serde_json::from_value(json!({"type":"input","content":"Find the project tokens"}))?;
    let submission = root.submit(input, &context).await?;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, submission.wait(&context))
            .await??
            .status,
        SubmissionStatus::Done
    );
    let entries = root
        .entries(Default::default(), 32, None, &context)
        .await?
        .items;
    assert!(
        entries
            .iter()
            .flat_map(|entry| entry.model.iter().flatten())
            .any(|message| {
                message["toolCallId"] == "run-lookup"
                    && message["isError"] == false
                    && message["content"][0]["text"] == "native lookup ran"
            })
    );
    process
        .registries
        .extensions
        .switch("fixture", Some(false))?;
    assert!(
        harness
            .durable
            .registry
            .snapshot()
            .extension("fixture")
            .is_none()
    );
    assert!(
        harness
            .durable
            .registry
            .snapshot()
            .extension("tool-search")
            .is_none()
    );
    assert_eq!(fake.state.call_count(), 3);
    worker.close().await?;
    Ok(())
}
