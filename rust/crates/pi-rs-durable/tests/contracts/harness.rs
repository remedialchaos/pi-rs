use pi_rs_chord::context::Context;
use pi_rs_durable::{
    AGENT_DOC, ConversationOwnership, EntryDraft, Harness,
    documents::resolve_address,
    session::{Error, SessionOptions},
    storage::sqlite::SqliteStorage,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn fork_after_reopen_inherits_agent_at_requested_entry() -> Result<(), Error> {
    let dir = tempfile::tempdir().map_err(pi_rs_durable::storage::StorageError::from)?;
    let path = dir.path().join("harness.sqlite");
    let ctx = Context::background();
    let harness = Harness::open(
        Arc::new(SqliteStorage::open(&path)?),
        SessionOptions::default(),
        &ctx,
    )
    .await?;
    let root = harness.root(&ctx).await?;
    let first = root
        .commit(
            async |tx| {
                pi_rs_durable::configure(
                    tx,
                    root.id,
                    &[("cwd".into(), json!("/first"))].into_iter().collect(),
                )
                .await?;
                Ok(tx
                    .append_entry(
                        root.id,
                        EntryDraft {
                            kind: "pi.user".into(),
                            ..Default::default()
                        },
                    )
                    .await?
                    .id)
            },
            &ctx,
            Default::default(),
        )
        .await?;
    root.configure(
        &[("cwd".into(), json!("/second"))].into_iter().collect(),
        &ctx,
    )
    .await?;
    harness.close(&ctx).await?;
    let reopened = Harness::open(
        Arc::new(SqliteStorage::open(&path)?),
        SessionOptions::default(),
        &ctx,
    )
    .await?;
    let root = reopened.root(&ctx).await?;
    let fork = root
        .fork(first, ConversationOwnership::Ownerless, &ctx)
        .await?;
    for (id, expected) in [(fork.id, "/first"), (root.id, "/second")] {
        let address = resolve_address(&AGENT_DOC, Some(id), None)?;
        let state = reopened.snapshot(&AGENT_DOC, &address, &ctx).await?;
        assert_eq!(
            state.as_ref().and_then(|s| s.get("cwd")),
            Some(&json!(expected))
        );
    }
    reopened.close(&ctx).await
}

#[tokio::test]
async fn model_tool_round_matches_upstream_entries_and_tool_output()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_ai::faux::{faux_assistant_message, faux_provider, faux_text, faux_tool_call};
    use pi_rs_durable::{
        Extension, HarnessOptions, HarnessRegistry, ToolRegistration, create_registry,
        tools::ToolResult,
    };
    use pi_rs_durable::{SubmissionStatus, storage::memory::MemoryStorage};
    for available in [false, true] {
        let registry = create_registry();
        if available {
            registry.install(Extension {
                name: "tool:echo".into(),
                tools: vec![ToolRegistration {
                    sequential: false,
                    prepare_arguments: None,
                    cancel: None,
                    structured_output_schema: None,
                    output_limits: None,
                    declaration: pi_rs_ai::types::Tool {
                        name: "echo".into(),
                        description: "Returns the input text".into(),
                        parameters: json!({
                            "type": "object",
                            "properties": {"text":{"type":"string"}},
                            "required": ["text"]
                        }),
                        constrained_sampling: None,
                    },
                    execute: Arc::new(|args, _, _| {
                        Box::pin(async move {
                            let result: ToolResult = serde_json::from_value(
                                json!({"output":[{"type":"text","text":args["text"]}]}),
                            )?;
                            Ok(result.into())
                        })
                    }),
                    deferred: false,
                    callers: None,
                    replay_safe: false,
                }],
                ..Default::default()
            })?;
        }
        let ctx = Context::background();
        let harness = Harness::open_with(
            Arc::new(MemoryStorage::new()),
            HarnessOptions {
                registry: Arc::new(HarnessRegistry::Local(registry)),
                ..Default::default()
            },
            &ctx,
        )
        .await?;
        let fake = faux_provider(Default::default());
        let model = &fake.models[0];
        let mut first = faux_assistant_message(vec![faux_tool_call(
            if available { "echo" } else { "unavailable" },
            if available {
                serde_json::from_value(json!({"text":"hello"}))?
            } else {
                Default::default()
            },
            Some("call-1".into()),
        )]);
        first.stop_reason = pi_rs_ai::StopReason::ToolUse;
        fake.set_responses([
            first.into(),
            faux_assistant_message(vec![faux_text("answer")]).into(),
        ]);
        harness.models.set_provider(fake.provider.clone());
        let root = harness.root(&ctx).await?;
        root.configure(
            &[(
                "model".into(),
                json!({"provider":model.provider,"modelId":model.id}),
            )]
            .into_iter()
            .collect(),
            &ctx,
        )
        .await?;
        let submission = root
            .submit(
                serde_json::from_value(json!({"type":"input","content":"question"}))?,
                &ctx,
            )
            .await?;
        let record =
            match tokio::time::timeout(std::time::Duration::from_secs(10), submission.wait(&ctx))
                .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(format!(
                        "submission {:?} tasks {:?}",
                        submission.status(&ctx).await?,
                        harness
                            .scan_tasks(&Default::default(), 64, None, &ctx)
                            .await?
                            .items
                    )
                    .into());
                }
            };
        assert_eq!(record.status, SubmissionStatus::Done);
        let entries = root
            .entries(Default::default(), 32, None, &ctx)
            .await?
            .items;
        let kinds: Vec<_> = entries.iter().rev().map(|e| e.kind.as_str()).collect();
        let mut expected = vec!["pi.user"];
        if available {
            expected.push("pi.system");
        }
        expected.extend(["pi.assistant", "pi.tool-result", "pi.assistant"]);
        assert_eq!(kinds, expected);
        let messages: Vec<_> = entries
            .iter()
            .rev()
            .flat_map(|e| e.model.iter().flatten())
            .collect();
        let roles: Vec<_> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        let mut expected_roles = vec!["user"];
        if available {
            expected_roles.push("system");
        }
        expected_roles.extend(["assistant", "toolResult", "assistant"]);
        assert_eq!(roles, expected_roles);
        let tool = messages
            .iter()
            .find(|m| m["role"] == "toolResult")
            .ok_or("missing tool result")?;
        assert_eq!(tool["toolCallId"], "call-1");
        assert_eq!(tool["isError"], !available);
        assert_eq!(
            tool["content"],
            json!([{"type":"text","text":if available { "hello" } else { "<harness>\n[error] Tool unavailable is not available\n</harness>" }}])
        );
        assert_eq!(fake.state.call_count(), 2);
        harness.close(&ctx).await?;
    }
    Ok(())
}

#[tokio::test]
async fn disconnected_transcript_windows_keep_the_gap_and_resume_after_the_newest_entry()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_durable::{ConversationTranscript, ConversationView, MemoryStorage, TranscriptItem};
    let ctx = Context::background();
    let harness = Harness::open(Arc::new(MemoryStorage::new()), Default::default(), &ctx).await?;
    let root = harness.root(&ctx).await?;
    let mut transcript = ConversationTranscript::new(Arc::new(root), None)?;
    let mut windows = Vec::new();
    for (ids, before) in [([2, 3], 1), ([7, 8], 6)] {
        let entries = ids
            .into_iter()
            .map(|id| {
                serde_json::from_value(json!({"id":id,"conversationId":1,"kind":"test"}))
                    .map(Arc::new)
            })
            .collect::<Result<_, _>>()?;
        let view = transcript.merge(ConversationView {
            conversation: Arc::new(serde_json::from_value(json!({"id":1}))?),
            docs: Arc::default(),
            entries,
            before: Some(before),
        });
        windows.push(serde_json::Value::Array(
            view.items
                .iter()
                .map(|item| match item {
                    TranscriptItem::Earlier => json!({"kind":"earlier"}),
                    TranscriptItem::Gap(gap) => {
                        json!({"kind":"gap","after":gap.after,"upTo":gap.up_to})
                    }
                    TranscriptItem::Entry(e) => json!(e.id),
                })
                .collect(),
        ));
    }
    windows.push(serde_json::to_value(transcript.resume())?);
    assert_eq!(
        json!(windows),
        json!([[{"kind":"earlier"},2,3],[{"kind":"earlier"},2,3,{"kind":"gap","after":3,"upTo":7},7,8],{"from":9}])
    );
    harness.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_places_one_summary_before_answering_second_input()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_ai::faux::{faux_assistant_message, faux_provider, faux_text};
    use pi_rs_durable::{
        Extension, HarnessOptions, HarnessRegistry, HookHandler, HookRegistration, MemoryStorage,
        SubmissionStatus, create_registry,
    };
    for trigger in ["threshold", "overflow"] {
        let registry = create_registry();
        let summarize: HookHandler = Arc::new(move |input, _, _| {
            Box::pin(async move {
                assert_eq!(input["reason"], trigger);
                assert!(!input["entries"].as_array().is_none_or(Vec::is_empty));
                Ok(Some(
                    json!({"kind":"summary","summary":"remember the first answer"}),
                ))
            })
        });
        registry.install(Extension {
            name: "compact-hook".into(),
            hooks: vec![HookRegistration {
                task: "pi.compaction".into(),
                handlers: [("beforeCompact".into(), summarize)].into_iter().collect(),
            }],
            ..Default::default()
        })?;
        let ctx = Context::background();
        let harness = Harness::open_with(
            Arc::new(MemoryStorage::new()),
            HarnessOptions {
                registry: Arc::new(HarnessRegistry::Local(registry)),
                ..Default::default()
            },
            &ctx,
        )
        .await?;
        let fake = faux_provider(Default::default());
        let model = &fake.models[0];
        let mut responses = vec![faux_assistant_message(vec![faux_text("first answer")]).into()];
        if trigger == "overflow" {
            let mut error = faux_assistant_message(vec![]);
            error.stop_reason = pi_rs_ai::StopReason::Error;
            error.error_message = Some("maximum context length is 100 tokens".into());
            responses.push(error.into());
        }
        responses.push(faux_assistant_message(vec![faux_text("second answer")]).into());
        fake.set_responses(responses);
        harness.models.set_provider(fake.provider.clone());
        let root = harness.root(&ctx).await?;
        root.configure(
            &[
                (
                    "model".into(),
                    json!({"provider":model.provider,"modelId":model.id}),
                ),
                (
                    "compaction".into(),
                    json!({
                        "keepRecentTokens": 1,
                        "backgroundTokens": 0,
                        "reserveTokens": if trigger=="threshold"{model.context_window}else{0}
                    }),
                ),
            ]
            .into_iter()
            .collect(),
            &ctx,
        )
        .await?;
        for input in ["first", "second"] {
            let submission = root
                .submit(
                    serde_json::from_value(json!({"type":"input","content":input}))?,
                    &ctx,
                )
                .await?;
            let settled =
                tokio::time::timeout(std::time::Duration::from_secs(10), submission.wait(&ctx))
                    .await??;
            assert_eq!(
                settled.status,
                SubmissionStatus::Done,
                "{trigger}: {settled:?}"
            );
        }
        let entries = root
            .entries(Default::default(), 32, None, &ctx)
            .await?
            .items;
        let summaries = entries
            .iter()
            .filter(|e| e.kind == "pi.compaction")
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1, "{trigger}");
        let answer = |text: &str| {
            entries
                .iter()
                .find(|entry| {
                    entry.kind == "pi.assistant"
                        && entry
                            .model
                            .iter()
                            .flatten()
                            .any(|message| message["content"][0]["text"].as_str() == Some(text))
                })
                .map(|entry| entry.id)
        };
        let first = answer("first answer").ok_or("missing first answer")?;
        let second = answer("second answer").ok_or("missing second answer")?;
        assert!(
            first < summaries[0].id && summaries[0].id < second,
            "{trigger}"
        );
        assert_eq!(
            summaries[0]
                .data
                .as_ref()
                .and_then(|d| d["reason"].as_str()),
            Some(trigger)
        );
        assert_eq!(
            fake.state.call_count(),
            if trigger == "overflow" { 3 } else { 2 }
        );
        harness.close(&ctx).await?;
    }
    Ok(())
}
#[tokio::test]
async fn failed_run_policy_settles_or_preserves_the_queued_follow_up()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_ai::faux::{FauxStep, faux_assistant_message, faux_provider, faux_text};
    use pi_rs_durable::{HarnessOptions, MemoryStorage, SubmissionStatus};
    use tokio::sync::Notify;
    for policy in ["keep", "withdraw", "continue"] {
        let ctx = Context::background();
        let harness = Harness::open_with(
            Arc::new(MemoryStorage::new()),
            HarnessOptions {
                settings: Arc::new(move || json!({"afterFailedRun": policy})),
                ..Default::default()
            },
            &ctx,
        )
        .await?;
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let gate = (started.clone(), release.clone());
        let fake = faux_provider(Default::default());
        let model = &fake.models[0];
        fake.set_responses([
            FauxStep::Factory(Arc::new(move |_, _, _, _| {
                let (started, release) = gate.clone();
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    let mut response = faux_assistant_message(vec![]);
                    response.stop_reason = pi_rs_ai::StopReason::Error;
                    response.error_message = Some("permanent failure".into());
                    Ok(response)
                })
            })),
            faux_assistant_message(vec![faux_text("next answer")]).into(),
        ]);
        harness.models.set_provider(fake.provider.clone());
        let root = harness.root(&ctx).await?;
        root.configure(
            &[(
                "model".into(),
                json!({"provider":model.provider,"modelId":model.id}),
            )]
            .into_iter()
            .collect(),
            &ctx,
        )
        .await?;
        let draft = |text| {
            serde_json::from_value(json!({"type":"input","content":text,"whenBusy":"followUp"}))
        };
        let first = root.submit(draft("first")?, &ctx).await?;
        let timeout = std::time::Duration::from_secs(10);
        tokio::time::timeout(timeout, started.notified()).await?;
        let second = root.submit(draft("second")?, &ctx).await?;
        release.notify_one();
        assert_eq!(
            tokio::time::timeout(timeout, first.wait(&ctx))
                .await??
                .status,
            SubmissionStatus::Unanswered
        );
        if policy == "keep" {
            assert_eq!(second.status(&ctx).await?.status, SubmissionStatus::Queued);
        } else {
            let outcome = tokio::time::timeout(timeout, second.wait(&ctx)).await??;
            if policy == "withdraw" {
                assert_eq!(outcome.reason.as_deref(), Some("run_failed"));
                assert_eq!(outcome.status, SubmissionStatus::Unanswered);
            } else {
                assert_eq!(outcome.status, SubmissionStatus::Done);
            }
        }
        harness.close(&ctx).await?;
    }
    Ok(())
}
