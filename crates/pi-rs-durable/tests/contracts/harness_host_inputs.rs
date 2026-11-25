//! Host-input contracts of the durable Harness, mirroring upstream
//! `harness-structured-output.test.ts`, `harness-storage-failure.test.ts`, and
//! `harness-host-inputs.test.ts`: reports that name the owning extension, the safe host clock,
//! and model/settings selectors read at each use.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_rs_ai::faux::{
    FauxFactory, FauxOptions, FauxStep, faux_assistant_message, faux_provider, faux_text,
    faux_tool_call,
};
use pi_rs_ai::registry::Models;
use pi_rs_chord::context::Context;
use pi_rs_durable::session::{ReportKind, SessionOptions, TaskOptions};
use pi_rs_durable::tasks::{PhaseHandler, TaskDefinition};
use pi_rs_durable::tools::ToolResult;
use pi_rs_durable::{
    ClockReport, Conversation, Extension, Harness, HarnessOptions, HarnessRegistry, HookError,
    Invocation, MemoryStorage, ModelsInput, SubmissionStatus, TaskOutcome, TaskOwnership,
    TaskState, ToolRegistration, create_registry, define_task, safe_now, wrap_tool,
};
use serde_json::{Value, json};

/// The schema tools in this file declare: an object with one required number `count`.
fn count_schema() -> Value {
    json!({"type":"object","properties":{"count":{"type":"number"}},"required":["count"]})
}

/// A tool named `name` whose execution returns `result`.
fn returning(name: &str, result: ToolResult, schema: Option<Value>) -> ToolRegistration {
    ToolRegistration {
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: schema,
        output_limits: None,
        declaration: pi_rs_ai::types::Tool {
            name: name.into(),
            description: name.into(),
            parameters: json!({"type":"object"}),
            constrained_sampling: None,
        },
        execute: Arc::new(move |_, _, _| {
            let result = result.clone();
            Box::pin(async move { Ok(result.into()) })
        }),
        deferred: false,
        callers: None,
        replay_safe: false,
    }
}

/// A `Models` whose faux provider answers every request with `text`.
fn answering(text: &str) -> Models {
    let faux = faux_provider(FauxOptions::default());
    faux.set_responses((0..16).map(|_| faux_assistant_message(vec![faux_text(text)]).into()));
    let models = Models::default();
    models.set_provider(faux.provider);
    models
}

/// The first text content of a message.
fn first_text(message: &Value) -> Option<String> {
    message["content"].as_array()?.iter().find_map(|content| {
        (content["type"] == "text")
            .then(|| content["text"].as_str().map(str::to_owned))
            .flatten()
    })
}

async fn open_chat(
    options: HarnessOptions,
) -> Result<(Harness, Conversation), Box<dyn std::error::Error>> {
    let ctx = Context::background();
    let harness = Harness::open_with(Arc::new(MemoryStorage::new()), options, &ctx).await?;
    let root = harness.root(&ctx).await?;
    root.configure(
        &[(
            "model".into(),
            json!({"provider":"faux","modelId":"faux-1"}),
        )]
        .into_iter()
        .collect(),
        &ctx,
    )
    .await?;
    Ok((harness, root))
}

/// One turn: submit `content`, wait for the answer, and return its first text.
async fn turn(
    root: &Conversation,
    content: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let ctx = Context::background();
    let submission = root
        .submit(
            serde_json::from_value(json!({"type":"input","content":content}))?,
            &ctx,
        )
        .await?;
    let settled = tokio::time::timeout(Duration::from_secs(20), submission.wait(&ctx)).await??;
    assert_eq!(settled.status, SubmissionStatus::Done);
    let entries = root
        .entries(Default::default(), 32, None, &ctx)
        .await?
        .items;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.model.as_ref())
        .flatten()
        .find_map(first_text))
}

/// Every report the session reporter saw, as `(kind, message)` pairs.
type SeenReports = Arc<Mutex<Vec<(ReportKind, String)>>>;

fn session_reporter(seen: SeenReports) -> Option<pi_rs_durable::session::Reporter> {
    Some(Arc::new(move |kind, message| {
        seen.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((kind, message.to_owned()));
    }))
}

#[tokio::test]
async fn invalid_structured_output_names_the_owning_extension_for_model_calls()
-> Result<(), Box<dyn std::error::Error>> {
    // (tool, returned result, declared schema, report fragment, "" when the value is dropped)
    let cases: &[(&str, ToolResult, Option<Value>, &str, bool)] = &[
        (
            "wrong",
            ToolResult {
                output: Some(Vec::new()),
                structured_output: Some(json!({"count":"three"})),
                ..ToolResult::default()
            },
            Some(count_schema()),
            "does not match its schema: count",
            true,
        ),
        (
            "missing",
            ToolResult {
                output: Some(Vec::new()),
                ..ToolResult::default()
            },
            Some(count_schema()),
            "returned no structuredOutput",
            true,
        ),
        (
            "undeclared",
            ToolResult {
                structured_output: Some(json!({"count":3})),
                ..ToolResult::default()
            },
            None,
            "declares no structuredOutputSchema",
            false,
        ),
    ];
    for (name, result, schema, fragment, dropped) in cases {
        // A nested caller gets an error result carrying the diagnostic, and no host report.
        let received: Arc<Mutex<Vec<Value>>> = Arc::default();
        let probe = ToolRegistration {
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            structured_output_schema: None,
            output_limits: None,
            declaration: pi_rs_ai::types::Tool {
                name: "probe".into(),
                description: "probe".into(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            execute: {
                let (received, name) = (received.clone(), (*name).to_owned());
                Arc::new(move |_, inv: &Invocation, ctx| {
                    let (received, name) = (received.clone(), name.clone());
                    Box::pin(async move {
                        let result = inv
                            .execute_tool("inner", &name, serde_json::Map::new(), false, false, ctx)
                            .await?;
                        received
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(result);
                        Ok(ToolResult::default().into())
                    })
                })
            },
            deferred: false,
            callers: None,
            replay_safe: false,
        };
        let registry = create_registry();
        registry.install(Extension {
            name: "broken-outputs".into(),
            tools: vec![returning(name, result.clone(), schema.clone())],
            ..Extension::default()
        })?;
        registry.install(Extension {
            name: "probes".into(),
            tools: vec![probe],
            ..Extension::default()
        })?;
        let faux = faux_provider(FauxOptions::default());
        let mut call = faux_assistant_message(vec![faux_tool_call(
            "probe",
            Default::default(),
            Some("c1".into()),
        )]);
        call.stop_reason = pi_rs_ai::StopReason::ToolUse;
        faux.set_responses([
            call.into(),
            faux_assistant_message(vec![faux_text("done")]).into(),
        ]);
        let models = Models::default();
        models.set_provider(faux.provider);
        let session_reports: SeenReports = Arc::default();
        let (extension_tx, mut extension_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        let (harness, root) = open_chat(HarnessOptions {
            models: models.into(),
            registry: Arc::new(HarnessRegistry::Local(registry)),
            report_hook: Some(Arc::new(move |extension: &str, error: &HookError| {
                let _ = extension_tx.send((extension.to_owned(), error.to_string()));
            })),
            session: SessionOptions {
                report: session_reporter(session_reports.clone()),
                ..SessionOptions::default()
            },
            ..HarnessOptions::default()
        })
        .await?;
        assert_eq!(turn(&root, "go").await?, Some("done".into()));
        harness.close(&Context::background()).await?;
        let received = received.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(received.len(), 1, "nested {name}");
        let nested = &received[0];
        assert_eq!(nested["isError"], json!(true), "nested {name}");
        if *dropped {
            assert!(nested.get("structuredOutput").is_none(), "nested {name}");
        } else {
            assert_eq!(nested["structuredOutput"], json!(""), "nested {name}");
        }
        assert_eq!(
            nested["diagnostics"][0]["code"],
            json!("invalid_structured_output"),
            "nested {name}"
        );
        assert!(
            nested["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains(fragment)),
            "nested {name}: {:?}",
            nested["diagnostics"]
        );
        // A nested break is the caller's error result, never the host's report.
        assert!(
            session_reports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "nested {name} reported"
        );
        assert!(extension_rx.try_recv().is_err(), "nested {name} reported");

        // A model-issued call stands, and the host learns of the owning extension.
        let registry = create_registry();
        registry.install(Extension {
            name: "broken-outputs".into(),
            tools: vec![returning(name, result.clone(), schema.clone())],
            ..Extension::default()
        })?;
        let faux = faux_provider(FauxOptions::default());
        let mut call = faux_assistant_message(vec![faux_tool_call(
            *name,
            Default::default(),
            Some("c1".into()),
        )]);
        call.stop_reason = pi_rs_ai::StopReason::ToolUse;
        faux.set_responses([
            call.into(),
            faux_assistant_message(vec![faux_text("done")]).into(),
        ]);
        let models = Models::default();
        models.set_provider(faux.provider);
        let session_reports: SeenReports = Arc::default();
        let (extension_tx, mut extension_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        let (harness, root) = open_chat(HarnessOptions {
            models: models.into(),
            registry: Arc::new(HarnessRegistry::Local(registry)),
            report_hook: Some(Arc::new(move |extension: &str, error: &HookError| {
                let _ = extension_tx.send((extension.to_owned(), error.to_string()));
            })),
            session: SessionOptions {
                report: session_reporter(session_reports.clone()),
                ..SessionOptions::default()
            },
            ..HarnessOptions::default()
        })
        .await?;
        let turned = turn(&root, "go").await;
        assert_eq!(turned?, Some("done".into()));
        let entries = root
            .entries(Default::default(), 32, None, &Context::background())
            .await?
            .items;
        harness.close(&Context::background()).await?;
        let stored = entries
            .iter()
            .filter_map(|entry| entry.model.as_ref())
            .flatten()
            .find(|message| message["role"] == "toolResult")
            .ok_or("missing tool result")?;
        // The model never sees structured output: its call stands.
        assert_eq!(stored["isError"], json!(false), "model {name}");
        assert!(
            !stored["content"].to_string().contains(fragment),
            "model {name}: {}",
            stored["content"]
        );
        let (extension, reported) = extension_rx.try_recv()?;
        assert_eq!(extension, "broken-outputs", "model {name}");
        assert!(reported.contains(fragment), "model {name}: {reported}");
        let session = session_reports.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            session
                .iter()
                .any(|(kind, message)| *kind == ReportKind::Extension && message.contains(fragment)),
            "model {name}: {session:?}"
        );
    }

    // A wrapped tool's failure names the last extension that wraps it, not its provider.
    let registry = create_registry();
    let broken = ToolResult {
        output: Some(Vec::new()),
        structured_output: Some(json!({"count":"three"})),
        ..ToolResult::default()
    };
    let wrapped = returning("wrapped", broken, Some(count_schema()));
    registry.install(Extension {
        name: "provider-ext".into(),
        tools: vec![wrapped.clone()],
        ..Extension::default()
    })?;
    registry.install(Extension {
        name: "wrapper-ext".into(),
        wraps: vec![wrap_tool(&wrapped, |tool| tool)],
        ..Extension::default()
    })?;
    let faux = faux_provider(FauxOptions::default());
    let mut call = faux_assistant_message(vec![faux_tool_call(
        "wrapped",
        Default::default(),
        Some("c1".into()),
    )]);
    call.stop_reason = pi_rs_ai::StopReason::ToolUse;
    faux.set_responses([
        call.into(),
        faux_assistant_message(vec![faux_text("done")]).into(),
    ]);
    let models = Models::default();
    models.set_provider(faux.provider);
    let (extension_tx, mut extension_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let (harness, root) = open_chat(HarnessOptions {
        models: models.into(),
        registry: Arc::new(HarnessRegistry::Local(registry)),
        report_hook: Some(Arc::new(move |extension: &str, error: &HookError| {
            let _ = extension_tx.send((extension.to_owned(), error.to_string()));
        })),
        ..HarnessOptions::default()
    })
    .await?;
    assert_eq!(turn(&root, "go").await?, Some("done".into()));
    harness.close(&Context::background()).await?;
    let (extension, reported) = extension_rx.try_recv()?;
    assert_eq!(extension, "wrapper-ext");
    assert!(
        reported.contains("does not match its schema: count"),
        "{reported}"
    );
    Ok(())
}

#[test]
fn safe_now_reports_a_panicking_clock_once_and_answers_reentrant_reads() {
    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = reports.clone();
    let report: ClockReport = Arc::new(move |message, clock| {
        seen.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message.to_owned());
        // Reentry, as a handler that commits does: the clock is read again before this report
        // returns.
        if seen.lock().unwrap_or_else(|e| e.into_inner()).len() < 5 {
            let _ = clock();
        }
    });
    let clock = safe_now(Arc::new(|| panic!("clock broke")), report);
    // A broken clock must not stop commits: every read answers the system fallback.
    assert!(clock() > 1_000_000_000_000);
    assert!(clock() > 1_000_000_000_000);
    let reported = reports.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(reported[0].contains("clock broke"), "{reported:?}");
}

/// A task that waits for its abort and ends `aborted` through its abort handler.
fn waiting_task() -> Arc<pi_rs_durable::tasks::Task<Invocation>> {
    let work: PhaseHandler<Invocation> = Arc::new(|_, _, _| {
        Box::pin(async {
            std::future::pending::<()>().await;
            Ok(())
        })
    });
    let abort: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move {
            inv.commit(ctx, async |_, _| {
                Ok(TaskState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                })
            })
            .await?;
            Ok(())
        })
    });
    Arc::new(define_task(TaskDefinition {
        name: "test.waiting".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({"phase":"work"}))),
        phases: [("work".into(), work)].into_iter().collect(),
        abort,
        migrate: None,
        hooks: (),
    }))
}

/// Wait until the task runs, returning its record with the fallback clock's `startedAt`.
async fn wait_running(
    harness: &Harness,
    id: pi_rs_durable::TaskId,
    ctx: &Context,
) -> Result<Arc<pi_rs_durable::TaskRecord>, Box<dyn std::error::Error>> {
    loop {
        let record = harness.get_task(id, ctx).await?.ok_or("missing task")?;
        if matches!(record.state, TaskState::Running { .. }) {
            return Ok(record);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn a_panicking_clock_and_reporter_keep_the_harness_working()
-> Result<(), Box<dyn std::error::Error>> {
    let registry = create_registry();
    registry.install(Extension {
        name: "waiting".into(),
        tasks: vec![waiting_task()],
        ..Extension::default()
    })?;
    let ctx = Context::background();
    let session = SessionOptions {
        now: Arc::new(|| panic!("clock broke")),
        report: Some(Arc::new(|_, _| panic!("report broke"))),
        ..SessionOptions::default()
    };
    let harness = Harness::open_with(
        Arc::new(MemoryStorage::new()),
        HarnessOptions {
            registry: Arc::new(HarnessRegistry::Local(registry)),
            session,
            ..HarnessOptions::default()
        },
        &ctx,
    )
    .await?;
    let root = harness.root(&ctx).await?;
    let id = root
        .commit(
            async |tx| {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root.id);
                tx.create_task(waiting_task().as_ref(), Value::Null, options)
                    .await
            },
            &ctx,
            Default::default(),
        )
        .await?;
    harness.resume()?;
    // The clock falls back: the task still starts, with a real `startedAt`.
    let running =
        tokio::time::timeout(Duration::from_secs(20), wait_running(&harness, id, &ctx)).await??;
    assert!(running.started_at.unwrap_or(0) > 0, "fallback clock");
    // Abort still works, and the abort handler still commits.
    harness.abort_task(id, &ctx).await?;
    let ended =
        tokio::time::timeout(Duration::from_secs(20), harness.wait_for_task(id, &ctx)).await??;
    assert!(matches!(
        &ended.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    harness.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn generation_reads_the_current_models_selector_at_each_request()
-> Result<(), Box<dyn std::error::Error>> {
    let current = Arc::new(Mutex::new(answering("first")));
    let select = {
        let current = current.clone();
        Arc::new(move || current.lock().unwrap_or_else(|e| e.into_inner()).clone())
            as Arc<dyn Fn() -> Models + Send + Sync>
    };
    let (harness, root) = open_chat(HarnessOptions {
        models: ModelsInput::Dynamic(select),
        ..HarnessOptions::default()
    })
    .await?;
    assert_eq!(turn(&root, "one").await?, Some("first".into()));
    *current.lock().unwrap_or_else(|e| e.into_inner()) = answering("second");
    assert_eq!(turn(&root, "two").await?, Some("second".into()));
    harness.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn still_accepts_a_plain_models_object() -> Result<(), Box<dyn std::error::Error>> {
    let (harness, root) = open_chat(HarnessOptions {
        models: answering("plain").into(),
        ..HarnessOptions::default()
    })
    .await?;
    assert_eq!(turn(&root, "one").await?, Some("plain".into()));
    harness.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn tools_read_the_current_models_at_each_access() -> Result<(), Box<dyn std::error::Error>> {
    let faux = faux_provider(FauxOptions::default());
    let mut call = faux_assistant_message(vec![faux_tool_call(
        "probe",
        Default::default(),
        Some("call-1".into()),
    )]);
    call.stop_reason = pi_rs_ai::StopReason::ToolUse;
    faux.set_responses([
        call.into(),
        faux_assistant_message(vec![faux_text("done")]).into(),
    ]);
    let first = Models::default();
    first.set_provider(faux.provider.clone());
    // Marks the second Models: tools see views of the host's Models that add the Harness identity.
    let second = Models::default();
    second.set_provider(faux.provider.clone());
    second.set_provider(
        faux_provider(FauxOptions {
            provider: Some("second-only".into()),
            ..FauxOptions::default()
        })
        .provider,
    );
    let current = Arc::new(Mutex::new(first));
    let select = {
        let current = current.clone();
        Arc::new(move || current.lock().unwrap_or_else(|e| e.into_inner()).clone())
            as Arc<dyn Fn() -> Models + Send + Sync>
    };
    let views: Arc<Mutex<Vec<Models>>> = Arc::default();
    let probe = ToolRegistration {
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        declaration: pi_rs_ai::types::Tool {
            name: "probe".into(),
            description: "Records the models before and after the host swaps it".into(),
            parameters: json!({"type":"object"}),
            constrained_sampling: None,
        },
        execute: {
            let (views, current, second) = (views.clone(), current.clone(), second.clone());
            Arc::new(move |_, inv: &Invocation, _| {
                let (views, current, second) = (views.clone(), current.clone(), second.clone());
                Box::pin(async move {
                    views
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(inv.models());
                    *current.lock().unwrap_or_else(|e| e.into_inner()) = second.clone();
                    views
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(inv.models());
                    Ok(ToolResult::default().into())
                })
            })
        },
        deferred: false,
        callers: None,
        replay_safe: false,
    };
    let registry = create_registry();
    registry.install(Extension {
        name: "probe-ext".into(),
        tools: vec![probe],
        ..Extension::default()
    })?;
    let (harness, root) = open_chat(HarnessOptions {
        models: ModelsInput::Dynamic(select),
        registry: Arc::new(HarnessRegistry::Local(registry)),
        ..HarnessOptions::default()
    })
    .await?;
    assert_eq!(turn(&root, "go").await?, Some("done".into()));
    harness.close(&Context::background()).await?;
    let views = views.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(views.len(), 2);
    assert!(views[0].get_provider("second-only").is_none());
    assert!(views[1].get_provider("second-only").is_some());
    Ok(())
}

#[tokio::test]
async fn generation_reads_the_current_settings_selector_at_each_request()
-> Result<(), Box<dyn std::error::Error>> {
    let timeouts: Arc<Mutex<Vec<Option<u64>>>> = Arc::default();
    let recorded = timeouts.clone();
    let reply: FauxFactory = Arc::new(move |_, options, _, _| {
        let recorded = recorded.clone();
        Box::pin(async move {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(options.request.timeout_ms);
            Ok(faux_assistant_message(vec![faux_text("ok")]))
        })
    });
    let faux = faux_provider(FauxOptions::default());
    faux.set_responses([FauxStep::Factory(reply.clone()), FauxStep::Factory(reply)]);
    let models = Models::default();
    models.set_provider(faux.provider);
    let settings = Arc::new(Mutex::new(json!({"stream":{"timeoutMs":1000}})));
    let select = {
        let settings = settings.clone();
        Arc::new(move || settings.lock().unwrap_or_else(|e| e.into_inner()).clone())
            as Arc<dyn Fn() -> Value + Send + Sync>
    };
    let (harness, root) = open_chat(HarnessOptions {
        models: models.into(),
        settings: select,
        ..HarnessOptions::default()
    })
    .await?;
    assert_eq!(turn(&root, "one").await?, Some("ok".into()));
    *settings.lock().unwrap_or_else(|e| e.into_inner()) = json!({"stream":{"timeoutMs":2000}});
    assert_eq!(turn(&root, "two").await?, Some("ok".into()));
    harness.close(&Context::background()).await?;
    assert_eq!(
        *timeouts.lock().unwrap_or_else(|e| e.into_inner()),
        vec![Some(1000), Some(2000)]
    );
    Ok(())
}
