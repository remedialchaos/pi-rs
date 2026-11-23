use std::{error::Error, path::Path, sync::Arc, time::Duration};

use pi_rs_ai::{
    AssistantMessage, InputContent, StopReason,
    faux::{faux_assistant_message, faux_provider, faux_text, faux_tool_call},
    registry::Models,
};
use pi_rs_chord::{
    context::Context,
    node::workers::{Control, Frame, WorkerSession, write_frame},
};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID, SubmissionStatus,
    documents::resolve_address,
    harness::{Extension, ToolRegistration},
    tools::ToolResult,
};
use pi_rs_p2::{
    api::v1::{PluginRegistry, session_address},
    hosts::worker::start_session_worker_configured,
    plugins::{agent, subagents::contract_worker::subagents_doc},
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

async fn handshake(directory: &Path) -> TestResult<(WorkerSession, tokio::io::DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(child);
    write_frame(&mut parent, &Frame::Control(Control::Start {
        address: session_address("sample"),
        start: json!({
            "app": directory, "dataDir": directory, "fake": true,
            "telemetry": false, "generation": 1, "values": {"session":{
                "id": "sample", "cwd": directory, "model":{"provider":"faux","modelId":"faux-1"},
                "thinkingLevel": "off", "resume": true
            }}
        }),
        scripts: Vec::new(),
        lock: directory.join("worker.lock"),
    })).await?;
    Ok((WorkerSession::start(read, write).await?, parent))
}

#[tokio::test]
async fn agent_is_script_only_and_retries_invalid_answers_in_one_child() -> TestResult {
    let fixture = tempfile::tempdir()?;
    let context = Context::background();
    let (session, _parent) = handshake(fixture.path()).await?;
    let fake = faux_provider(Default::default());
    let models = Models::default();
    models.set_provider(fake.provider.clone());
    let mut registry = PluginRegistry::new();
    agent::register(&mut registry)?;
    let worker = start_session_worker_configured(
        session,
        &registry,
        Some(&["agent".into()]),
        models,
        |process| {
            process.registries.extensions.install(Extension {
                name: "script-fixture".into(),
                tools: vec![ToolRegistration {
                    declaration: serde_json::from_value(json!({
                        "name": "script", "description": "Run a fixture script",
                        "parameters": {"type":"object"}
                    }))?,
                    sequential: false,
                    prepare_arguments: None,
                    cancel: None,
                    structured_output_schema: None,
                    output_limits: None,
                    replay_safe: true,
                    callers: Some(vec!["model".into()]),
                    deferred: false,
                    execute: Arc::new(|_, invocation, ctx| {
                        Box::pin(async move {
                            let value = invocation
                                .execute_tool(
                                    "once",
                                    "agent",
                                    serde_json::from_value(json!({
                                        "prompt": "Find the answer", "label": "  Find  answer ",
                                        "schema": {"type": "integer"}, "tools": []
                                    }))?,
                                    false,
                                    false,
                                    ctx,
                                )
                                .await?;
                            Ok(ToolResult {
                                output: Some(vec![InputContent::Text(value.to_string().into())]),
                                ..Default::default()
                            }
                            .into())
                        })
                    }),
                }],
                ..Default::default()
            })?;
            Ok(Vec::new())
        },
    )
    .await?;

    let harness = &worker.process.harness()?.durable;
    let root = harness.root(&context).await?;
    let caller = root.agent(&context).await?;
    assert!(
        caller
            .callable
            .iter()
            .any(|tool| tool.declaration.name == "agent")
    );
    assert!(
        !caller
            .tools
            .iter()
            .any(|tool| tool.declaration.name == "agent")
    );
    fake.set_responses([
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "script",
                Default::default(),
                Some("run".into()),
            )])
        }
        .into(),
        faux_assistant_message(vec![faux_text("\"wrong type\"")]).into(),
        faux_assistant_message(vec![faux_text("```json\n42\n```")]).into(),
        faux_assistant_message(vec![faux_text("done")]).into(),
    ]);
    let submitted = root
        .submit(
            serde_json::from_value(json!({"type":"input","content":"Run the script"}))?,
            &context,
        )
        .await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), submitted.wait(&context))
            .await??
            .status,
        SubmissionStatus::Done
    );
    let entries = root
        .entries(Default::default(), 32, None, &context)
        .await?
        .items;
    let output = entries
        .iter()
        .flat_map(|entry| entry.model.iter().flatten())
        .find(|message| message["toolCallId"] == "run")
        .and_then(|message| message["content"][0]["text"].as_str())
        .ok_or_else(|| format!("missing script result in {entries:?}"))?;
    let result: serde_json::Value = serde_json::from_str(output)?;
    assert_eq!(result["structuredOutput"], 42, "{result}");
    assert_eq!(fake.state.call_count(), 4);
    let doc = subagents_doc();
    let address = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let state = harness
        .session
        .snapshot(&doc, &address, &context)
        .await?
        .ok_or("no subagents document")?;
    let one_shots = state["oneShots"]
        .as_object()
        .ok_or("no one-shot registry")?;
    assert_eq!(one_shots.len(), 1);
    let shot = one_shots.values().next().ok_or("no one-shot agent")?;
    assert_eq!(shot["label"], "Find answer");
    let id = shot["conversationId"].as_u64().ok_or("missing child ID")?;
    assert_eq!(
        worker
            .process
            .registries
            .read_only_reason(id, &context)
            .await?
            .as_deref(),
        Some("One-shot agents are read-only")
    );
    worker.close().await?;
    Ok(())
}
