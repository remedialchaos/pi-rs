use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use pi_rs_ai::{
    AnyModel, AssistantImages, ClassifierAnswer, ClassifierResult, Cost, ImageContent,
    InputContent, Models, StopReason, Usage,
    faux::{FauxOptions, faux_provider},
};
use pi_rs_chord::context::Context;
use pi_rs_codemode::StoreWrites;
use pi_rs_durable::{
    codemode::*,
    documents::{CheckpointInfo, DocumentSemantics},
    env::{ExecutionEnv, LocalExecutionEnv},
    storage_types::DocumentFork,
};
use serde_json::{Value, json};
use tokio::sync::{Barrier, Mutex, Notify};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct States {
    run: CodemodeRunState,
    journal: CodemodeJournalState,
    store: CodemodeStoreState,
}

#[derive(Default)]
struct Documents {
    states: Mutex<States>,
    changed: Notify,
    reject_ack: AtomicBool,
}

impl Documents {
    async fn after_ack(&self, seq: usize) {
        loop {
            let changed = self.changed.notified();
            if self
                .states
                .lock()
                .await
                .journal
                .entries
                .get(seq - 1)
                .is_some_and(|entry| entry.issued_after.is_some())
            {
                return;
            }
            changed.await;
        }
    }
}

#[async_trait]
impl CodemodeDocuments for Documents {
    async fn begin(
        &self,
        mut fresh: CodemodeStart,
        context: &Context,
    ) -> Result<CodemodeSnapshot, CodemodeError> {
        context.check()?;
        let mut state = self.states.lock().await;
        if let Some(result) = &state.run.result {
            return Ok(CodemodeSnapshot::Finished(result.clone()));
        }
        let rerun = state.run.start.is_some();
        if !rerun {
            fresh.store = state.store.values.clone();
            state.run.start = Some(fresh);
        }
        let start = state
            .run
            .start
            .clone()
            .ok_or_else(|| CodemodeError::Documents("start missing".into()))?;
        // Cross the actual persistence boundary, including JSON null versus omitted values.
        let journal = serde_json::from_value(serde_json::to_value(&state.journal)?)?;
        Ok(CodemodeSnapshot::Started {
            start,
            journal,
            rerun,
        })
    }

    async fn record_fingerprint(
        &self,
        fingerprint: String,
        context: &Context,
    ) -> Result<(), CodemodeError> {
        context.check()?;
        let mut state = self.states.lock().await;
        state
            .run
            .start
            .as_mut()
            .ok_or_else(|| CodemodeError::Documents("start missing".into()))?
            .fingerprint
            .get_or_insert(fingerprint);
        Ok(())
    }

    async fn update_journal(
        &self,
        update: CodemodeJournalUpdate,
        context: &Context,
    ) -> Result<(), CodemodeError> {
        context.check()?;
        if update.acknowledged.is_some() && self.reject_ack.load(Ordering::SeqCst) {
            return Err(CodemodeError::Documents("ack commit rejected".into()));
        }
        self.states.lock().await.journal.apply(update)?;
        self.changed.notify_one();
        Ok(())
    }

    async fn output_dir(
        &self,
        created: Option<String>,
        context: &Context,
    ) -> Result<Option<String>, CodemodeError> {
        context.check()?;
        let mut state = self.states.lock().await;
        if state.run.output_dir.is_none() {
            state.run.output_dir = created;
        }
        Ok(state.run.output_dir.clone())
    }

    async fn finish(
        &self,
        result: CodemodeToolResult,
        writes: Option<StoreWrites>,
        context: &Context,
    ) -> Result<CodemodeToolResult, CodemodeError> {
        context.check()?;
        let mut state = self.states.lock().await;
        if let Some(result) = &state.run.result {
            return Ok(result.clone());
        }
        if let Some(writes) = writes {
            state.store.apply(writes);
        }
        state.run.result = Some(result.clone());
        Ok(result)
    }
}

#[derive(Default)]
struct NestedRecord {
    input: Option<CodemodeNestedInput>,
    result: Option<CodemodeNestedResult>,
}

#[derive(Default)]
struct Host {
    catalog: CodemodeCatalog,
    calls: Mutex<HashMap<String, NestedRecord>>,
    echo_effects: AtomicUsize,
    hold_starts: AtomicUsize,
    hold_cancelled: Arc<AtomicBool>,
    release_hold: AtomicBool,
    clock: AtomicU64,
}

#[async_trait]
impl CodemodeTools for Host {
    fn now(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst)
    }

    async fn available_tools(&self, _: &Context) -> Result<CodemodeCatalog, CodemodeError> {
        Ok(self.catalog.clone())
    }

    async fn previous_nested_call(
        &self,
        key: &str,
        _: &Context,
    ) -> Result<Option<CodemodeNestedInput>, CodemodeError> {
        Ok(self
            .calls
            .lock()
            .await
            .get(key)
            .and_then(|record| record.input.clone()))
    }

    async fn execute_nested(
        &self,
        request: CodemodeNestedRequest<'_>,
        context: &Context,
    ) -> Result<CodemodeNestedResult, CodemodeError> {
        context.check()?;
        let input = CodemodeNestedInput {
            name: request.name.into(),
            arguments: request.arguments.clone(),
        };
        {
            let mut calls = self.calls.lock().await;
            let record = calls.entry(request.key.into()).or_default();
            if record.input.as_ref().is_some_and(|prior| prior != &input) {
                return Err(CodemodeError::Dispatch(
                    "nested key reused for different input".into(),
                ));
            }
            if let Some(result) = &record.result {
                return Ok(result.clone());
            }
            record.input = Some(input);
        }
        let mut result = if let Some(result) = request.execute_model(context).await {
            result.nested
        } else {
            match request.name {
                "echo" => {
                    self.echo_effects.fetch_add(1, Ordering::SeqCst);
                    CodemodeNestedResult {
                        structured_output: Some(request.arguments.clone()),
                        ..Default::default()
                    }
                }
                "hold" => {
                    self.hold_starts.fetch_add(1, Ordering::SeqCst);
                    if !self.release_hold.load(Ordering::SeqCst) {
                        let signal = context.abort_signal().ok_or_else(|| {
                            CodemodeError::Dispatch("no nested cancellation".into())
                        })?;
                        let called = Arc::clone(&self.hold_cancelled);
                        let _subscription =
                            signal.subscribe(move |_| called.store(true, Ordering::SeqCst));
                        let _ = signal.cancelled().await;
                        context.check()?;
                    }
                    CodemodeNestedResult {
                        structured_output: Some(json!("released")),
                        ..Default::default()
                    }
                }
                "plain-text" => CodemodeNestedResult {
                    structured_output: Some(
                        json!([{"type":"text","text":"first"},{"type":"image","mimeType":"image/png","data":"YQ=="},{"type":"text","text":"second"}]),
                    ),
                    ..Default::default()
                },
                "plain_error" => CodemodeNestedResult {
                    structured_output: Some(json!("nested text failed")),
                    is_error: true,
                    ..Default::default()
                },
                "structured_error" => CodemodeNestedResult {
                    structured_output: Some(json!({"exitCode":2})),
                    is_error: true,
                    errors: vec!["nonzero".into()],
                    ..Default::default()
                },
                _ => {
                    return Err(CodemodeError::Dispatch(format!(
                        "unexpected tool {}",
                        request.name
                    )));
                }
            }
        };
        result.duration_ms = Some(3);
        self.calls
            .lock()
            .await
            .entry(request.key.into())
            .or_default()
            .result = Some(result.clone());
        Ok(result)
    }
}

fn spec(name: &str, text: &str, structured: bool) -> CodemodeToolSpec {
    CodemodeToolSpec {
        name: name.into(),
        description: text.into(),
        parameters: json!({"type":"object"}),
        structured_output_schema: structured.then(|| json!({})),
        deferred: false,
    }
}

fn tools() -> Host {
    let mut plain = spec("plain-text", "Render an ordered text preview", false);
    plain.deferred = true;
    Host {
        catalog: CodemodeCatalog {
            callable: vec![
                plain,
                spec("plain_error", "Fails with plain text", false),
                spec("structured_error", "Fails with a code", true),
                spec("echo", "Echo structured input", true),
                spec("hold", "Wait until released", true),
            ],
            extensions: vec![CodemodeExtensionSpec {
                namespace: Some(ToolNamespace {
                    name: "preview-kit".into(),
                    description: "Preview helpers".into(),
                    instructions: Some("Render text preview".into()),
                }),
                tools: vec!["plain-text".into()],
            }],
        },
        ..Host::default()
    }
}

async fn execute(
    tool: &CodemodeTool,
    host: &Host,
    documents: &Documents,
    models: Option<&Models>,
    env: Option<&dyn ExecutionEnv>,
    code: &str,
    context: &Context,
) -> Result<CodemodeToolResult, CodemodeError> {
    tool.execute(
        CodemodeToolInput { code: code.into() },
        CodemodeInvocation {
            tools: host,
            documents,
            models: models.map(|models| models as &dyn CodemodeModelProvider),
            env,
        },
        context,
    )
    .await
}

fn text(result: &CodemodeToolResult) -> String {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            InputContent::Text(item) => Some(item.text.as_str()),
            InputContent::Image(_) => None,
        })
        .collect()
}

#[test]
fn declarations_keep_namespace_order_budget_and_model_grammar() -> TestResult {
    let host = tools();
    let extension = create_codemode_extension(CodemodeToolOptions {
        models: false,
        ..Default::default()
    });
    let declaration = serde_json::to_value(extension.tool.declaration())?;
    assert_eq!(declaration["name"], "codemode");
    assert_eq!(
        declaration["constrainedSampling"]["variants"]["openai_lark"],
        CODEMODE_SOURCE_GRAMMAR
    );
    assert!(
        !extension
            .tool
            .declaration()
            .description
            .contains("`models`")
    );
    assert!(extension.model_tools().is_none());
    let section = extension.section(&host.catalog)?.ok_or("missing section")?;
    assert!(section.contains("### `echo`"));
    assert!(section.contains("## preview-kit\nPreview helpers"));
    assert!(!section.contains("plain-text"));
    assert!(!section.contains("Render text preview"));
    let short = codemode_section(&host.catalog, 0)?.ok_or("missing short section")?;
    assert!(short.contains("find them with `searchTools()`"));
    assert!(short.contains("## preview-kit"));
    assert!(!short.contains("###"));

    let run = codemode_run_doc()?;
    let journal = codemode_journal_doc()?;
    let store = codemode_store_doc()?;
    assert_eq!(run.semantics, DocumentSemantics::Task);
    assert_eq!(journal.semantics, DocumentSemantics::Task);
    assert_eq!(
        store.semantics,
        DocumentSemantics::RewindableConversation {
            fork: DocumentFork::AsOf
        }
    );
    let value = store.initial(None)?;
    assert_eq!(Value::Object(value.clone()), json!({"values":{}}));
    assert!(!(store.checkpoint_when)(
        &value,
        &[],
        CheckpointInfo {
            deltas_since_base: 63
        }
    ));
    assert!((store.checkpoint_when)(
        &value,
        &[],
        CheckpointInfo {
            deltas_since_base: 64
        }
    ));
    assert!(!(journal.checkpoint_when)(
        &journal.initial(None)?,
        &[],
        CheckpointInfo {
            deltas_since_base: 1000
        }
    ));
    Ok(())
}

#[tokio::test]
async fn sandbox_runs_real_tools_and_preserves_output_modes_and_suffix() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions {
        models: false,
        ..Default::default()
    });
    let host = tools();
    let documents = Documents::default();
    let result = execute(&tool, &host, &documents, None, None, r#"
        const found = await searchTools('preview', {namespace:'preview_kit'});
        const ns = await describeNamespace('preview-kit');
        const sig = await describeTool('plain_text');
        const plain = await tools.plain_text({});
        const failure = await tools.plain_error({}).catch(e => [e.message, e.structuredOutput]);
        const coded = await tools.structured_error({});
        console.log('console comes last');
        text('begin');
        store('seen', coded.exitCode);
        return [found.map(t => t.name), ns.tools, sig.includes('Promise<string>'), plain, failure, coded.exitCode, typeof models];
    "#, &Context::background()).await?;
    assert!(!result.is_error, "{result:?}");
    let rendered = text(&result);
    assert!(rendered.starts_with("Script completed\nWall time 0.0 seconds\nOutput:\n==> text 1/2 <==\nbegin\n==> text 2/2 <==\n"));
    assert!(rendered.contains(r#"[["plain_text"],["plain_text"],true,"first\nsecond",["nested text failed","nested text failed"],2,"undefined"]"#), "{rendered}");
    assert!(
        rendered.ends_with("<console_output>\nconsole comes last\n</console_output>"),
        "{rendered}"
    );
    assert_eq!(
        result
            .details
            .calls
            .iter()
            .map(|row| (&*row.name, row.status))
            .collect::<Vec<_>>(),
        [
            ("plain-text", CodemodeCallStatus::Ok),
            ("plain_error", CodemodeCallStatus::Error),
            ("structured_error", CodemodeCallStatus::Error)
        ]
    );
    assert_eq!(result.details.calls[2].error.as_deref(), Some("nonzero"));
    assert_eq!(
        documents.states.lock().await.store.values.get("seen"),
        Some(&json!(2))
    );
    let cached = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        "return 'do not run'",
        &Context::background(),
    )
    .await?;
    assert_eq!(cached, result);
    Ok(())
}

#[tokio::test]
async fn file_spill_keeps_both_ends_and_saves_each_unique_image_once() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    let documents = Documents::default();
    let directory = tempfile::tempdir()?;
    documents.states.lock().await.run.output_dir =
        Some(directory.path().to_string_lossy().into_owned());
    let env = LocalExecutionEnv::new(directory.path().to_string_lossy());
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        Some(&env),
        r#"// @options: {"max_output_tokens": 8.0}
        text('HEAD-01234567890' + 'x'.repeat(64) + '01234567890-TAIL');
        image('data:image/png;base64,iVBORw0KGgo=');
        image('data:image/png;base64,iVBORw0KGgo=');
        image('data:image/jpeg;base64,/9j/4A==');
    "#,
        &Context::background(),
    )
    .await?;
    assert!(!result.is_error, "{result:?}");
    let path = Path::new(
        result
            .details
            .full_output_path
            .as_deref()
            .ok_or("missing full output file")?,
    );
    assert_eq!(path, directory.path().join("output.txt"));
    assert_eq!(
        std::fs::read_to_string(path)?,
        format!("HEAD-01234567890{}01234567890-TAIL", "x".repeat(64))
    );
    let rendered = text(&result);
    assert!(
        rendered.contains("HEAD-01234567890…16 tokens truncated…01234567890-TAIL"),
        "{rendered}"
    );
    assert!(rendered.contains("(read with offset/limit)]"));
    assert_eq!(rendered.matches("image-1.png").count(), 2);
    assert_eq!(rendered.matches("image-2.jpg").count(), 1);
    assert_eq!(
        std::fs::read(directory.path().join("image-1.png"))?,
        b"\x89PNG\r\n\x1a\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("image-2.jpg"))?,
        b"\xff\xd8\xff\xe0"
    );
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 3);
    Ok(())
}

#[tokio::test]
async fn failure_keeps_partial_output_and_discards_store_writes() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    let documents = Documents::default();
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        r#"
        await tools.echo({key: 1});
        text('partial');
        store('bad', true);
        throw new TypeError('broken');
    "#,
        &Context::background(),
    )
    .await?;
    assert!(result.is_error);
    let rendered = text(&result);
    assert!(
        rendered.contains("partial\nScript error:\nTypeError: broken"),
        "{rendered}"
    );
    assert!(
        rendered.ends_with("Tool calls made before the failure (they are not undone): echo (ok)")
    );
    assert!(documents.states.lock().await.store.values.is_empty());
    Ok(())
}

const REPLAY: &str = r#"
    const hold = tools.hold({ value: 1 });
    const seen = await observe({ a: 1, b: 2 });
    const answer = await tools.echo({ value: 9 });
    const released = await hold;
    store('done', [seen, answer.value]);
    return [seen === null, answer.value, released];
"#;

async fn interrupt(
    tool: &CodemodeTool,
    host: &Host,
    documents: &Documents,
    code: &str,
) -> TestResult {
    let (context, cancel) = Context::background().with_cancel();
    let (result, ack) = tokio::join!(
        execute(tool, host, documents, None, None, code, &context),
        async {
            let result =
                tokio::time::timeout(Duration::from_secs(20), documents.after_ack(2)).await;
            cancel.cancel("restart now");
            result
        },
    );
    ack?;
    assert!(
        matches!(result, Err(CodemodeError::Aborted(_))),
        "{result:?}"
    );
    assert!(host.hold_cancelled.load(Ordering::SeqCst));
    assert!(documents.states.lock().await.run.result.is_none());
    Ok(())
}

#[tokio::test]
async fn rerun_reattaches_tools_and_replays_out_of_order_globals_without_repeating_effects()
-> TestResult {
    let observed = Arc::new(AtomicUsize::new(0));
    let capture = Arc::clone(&observed);
    let tool = create_codemode_tool(CodemodeToolOptions {
        globals: vec![CodemodeGlobal {
            name: "observe".into(),
            spread: false,
            execute: Arc::new(move |_, _| {
                capture.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(Some(Value::Null)) })
            }),
        }],
        ..Default::default()
    });
    let host = tools();
    let documents = Documents::default();
    interrupt(&tool, &host, &documents, REPLAY).await?;
    let (recorded, fingerprinted) = {
        let state = documents.states.lock().await;
        (
            state
                .journal
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            state
                .run
                .start
                .as_ref()
                .is_some_and(|start| start.fingerprint.is_some()),
        )
    };
    assert_eq!(recorded, [2, 3]);
    assert!(fingerprinted);
    host.release_hold.store(true, Ordering::SeqCst);
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        &REPLAY.replace("a: 1, b: 2", "b: 2, a: 1"),
        &Context::background(),
    )
    .await?;
    assert!(!result.is_error, "{result:?}");
    assert!(text(&result).ends_with(r#"[true,9,"released"]"#));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(host.echo_effects.load(Ordering::SeqCst), 1);
    assert_eq!(host.hold_starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        documents.states.lock().await.store.values.get("done"),
        Some(&json!([null, 9]))
    );
    Ok(())
}

#[tokio::test]
async fn divergent_replay_refuses_to_admit_a_changed_held_key() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions {
        globals: vec![CodemodeGlobal {
            name: "observe".into(),
            spread: false,
            execute: Arc::new(|_, _| Box::pin(async { Ok(None) })),
        }],
        ..Default::default()
    });
    let host = tools();
    let documents = Documents::default();
    interrupt(&tool, &host, &documents, REPLAY).await?;
    let changed = REPLAY.replace("value: 1", "value: 2");
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        &changed,
        &Context::background(),
    )
    .await?;
    assert!(result.is_error);
    assert!(
        text(&result)
            .contains("call 1 is tools.hold({\"value\":2}), but was tools.hold({\"value\":1})"),
        "{result:?}"
    );
    assert_eq!(host.echo_effects.load(Ordering::SeqCst), 1);
    assert_eq!(host.hold_starts.load(Ordering::SeqCst), 1);
    assert!(documents.states.lock().await.store.values.is_empty());
    Ok(())
}

#[tokio::test]
async fn nested_admission_waits_for_the_durable_issued_count() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    let documents = Documents::default();
    documents.reject_ack.store(true, Ordering::SeqCst);
    let code = "return await tools.echo({value:1})";
    let failed = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        code,
        &Context::background(),
    )
    .await;
    assert!(
        matches!(failed, Err(CodemodeError::Documents(message)) if message == "ack commit rejected")
    );
    assert_eq!(host.echo_effects.load(Ordering::SeqCst), 0);
    assert!(host.calls.lock().await.is_empty());
    documents.reject_ack.store(false, Ordering::SeqCst);
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        code,
        &Context::background(),
    )
    .await?;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(host.echo_effects.load(Ordering::SeqCst), 1);
    Ok(())
}

fn faux_models(
    entered: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    gate: Option<Arc<Barrier>>,
) -> TestResult<Models> {
    let fake = faux_provider(FauxOptions::default());
    let judge: AnyModel = serde_json::from_value(json!({
        "type":"classifier", "api":"typesafe-system-one", "provider":"faux", "baseUrl":"http://localhost:0", "headers":{"authorization":"secret"},
        "id":"judge", "name":"Judge", "input":["text","image"], "contextWindow":4096, "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))?;
    let painter: AnyModel = serde_json::from_value(json!({
        "type":"image", "api":"openrouter-images", "provider":"faux", "baseUrl":"http://localhost:0",
        "id":"painter", "name":"Painter", "input":["text"], "output":["image"], "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))?;
    let mut catalog = fake.provider.models.to_vec();
    catalog.extend([judge, painter]);
    let mut provider = fake.provider.with_models(catalog);
    let active = Arc::new(AtomicUsize::new(0));
    provider.classify = Some(Arc::new(move |model, input, _| {
        let entered = Arc::clone(&entered);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        let gate = gate.clone();
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            if let Some(gate) = gate {
                gate.wait().await;
            }
            active.fetch_sub(1, Ordering::SeqCst);
            let fail = input.state.get("fail").and_then(Value::as_bool) == Some(true);
            let checked_url = if model.base_url == "https://untrusted.invalid" {
                0.0
            } else {
                0.75
            };
            ClassifierResult {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                answers: [(
                    "ok".into(),
                    ClassifierAnswer::Bool {
                        probability: checked_url,
                    },
                )]
                .into(),
                usage: Some(Usage {
                    cost: Cost {
                        total: 0.25,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                stop_reason: if fail {
                    StopReason::Error
                } else {
                    StopReason::Stop
                },
                error_message: fail.then(|| "quota exceeded".into()),
                timestamp: 1,
            }
        })
    }));
    provider.images = Some(Arc::new(|model, _, _| {
        Box::pin(async move {
            AssistantImages {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                output: vec![InputContent::Image(ImageContent {
                    kind: Default::default(),
                    mime_type: "image/png".into(),
                    data: "iVBORw0KGgo=".into(),
                })],
                response_id: None,
                usage: None,
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 1,
            }
        })
    }));
    let models = Models::default();
    models.set_provider(provider);
    Ok(models)
}

#[tokio::test]
async fn model_tools_use_real_faux_registry_validate_context_and_do_not_pass_script_credentials()
-> TestResult {
    let entered = Arc::new(AtomicUsize::new(0));
    let models = faux_models(Arc::clone(&entered), Arc::new(AtomicUsize::new(0)), None)?;
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = Host::default();
    let documents = Documents::default();
    let result = execute(&tool, &host, &documents, Some(&models), None, r#"
        const c = await models.getModelOfType('classifier','faux','judge');
        const available = await models.getAvailableOfType('image');
        const ctx = {state:{}, questions:{ok:{type:'bool',instructions:'approve?',criteria:{true:'yes',false:'no'}}}};
        const altered = {...c, baseUrl:'https://untrusted.invalid', headers:{authorization:'steal'}};
        const result = await models.classify(altered, ctx);
        const failed = await models.classify(c, {...ctx,state:{fail:true}});
        const invalid = await models.classify(c, {prompt:'bad'}).catch(e=>e.message);
        const images = await models.generateImages(available[0], {input:[{type:'text',text:'a dot'}]});
        return ['headers' in c, result.answers.ok.probability, result.usage.cost.total, failed.stopReason, failed.errorMessage, invalid.includes('context.state must be an object'), images.output.length, ALL_TOOLS.length];
    "#, &Context::background()).await?;
    assert!(!result.is_error, "{result:?}");
    let rendered = text(&result);
    assert!(
        rendered.contains(r#"[false,0.75,0.25,"error","quota exceeded",true,1,0]"#),
        "{rendered}"
    );
    assert!(rendered.contains("returned 1 image that the script did not show"));
    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(
        result
            .details
            .calls
            .iter()
            .map(|row| (&*row.name, row.status))
            .collect::<Vec<_>>(),
        [
            ("models.classify", CodemodeCallStatus::Ok),
            ("models.classify", CodemodeCallStatus::Error),
            ("models.classify", CodemodeCallStatus::Error),
            ("models.generateImages", CodemodeCallStatus::Ok)
        ]
    );
    let calls = host.calls.lock().await;
    assert_eq!(
        calls
            .get("c3")
            .and_then(|call| call.result.as_ref())
            .and_then(|result| result.usage)
            .map(|usage| usage.cost.total),
        Some(0.25)
    );
    drop(calls);
    let shown = execute(
        &tool,
        &Host::default(),
        &Documents::default(),
        Some(&models),
        None,
        r#"
        const model = await models.getModelOfType('image','faux','painter');
        const result = await models.generateImages(model, {input:[{type:'text',text:'a dot'}]});
        image(result.output[0]);
    "#,
        &Context::background(),
    )
    .await?;
    assert!(!shown.is_error, "{shown:?}");
    assert!(!text(&shown).contains("did not show"));
    assert!(
        text(&shown)
            .contains("[Image (image/png, 8 bytes) could not be saved: no execution environment]")
    );
    assert!(
        shown
            .output
            .iter()
            .any(|item| matches!(item, InputContent::Image(_)))
    );
    Ok(())
}

#[tokio::test]
async fn limits_paid_models_to_four_in_flight() -> TestResult {
    let entered = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let models = faux_models(
        Arc::clone(&entered),
        Arc::clone(&peak),
        Some(Arc::new(Barrier::new(4))),
    )?;
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = Host::default();
    let documents = Documents::default();
    let context = Context::background();
    let result = tokio::time::timeout(Duration::from_secs(20), execute(&tool, &host, &documents, Some(&models), None, r#"
        const model = await models.getModelOfType('classifier','faux','judge');
        const input = {state:{},questions:{ok:{type:'bool',instructions:'approve?',criteria:{true:'yes',false:'no'}}}};
        return (await Promise.all(Array.from({length:8}, () => models.classify(model, input)))).length;
    "#, &context)).await??;
    assert!(!result.is_error, "{result:?}");
    assert!(text(&result).ends_with('8'));
    assert_eq!(entered.load(Ordering::SeqCst), 8);
    assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_MODEL_CALLS);
    Ok(())
}

#[tokio::test]
async fn zero_budget_reports_failed_spill_without_dropping_a_successful_store() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    let documents = Documents::default();
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        "// @options: {\"max_output_tokens\": 0}\nstore('x', 1); return 'visible only in spill';",
        &Context::background(),
    )
    .await?;
    assert!(!result.is_error);
    assert!(result.details.full_output_path.is_none());
    let rendered = text(&result);
    assert!(rendered.contains("…6 tokens truncated…"));
    assert!(!rendered.contains("visible only in spill"));
    assert!(rendered.ends_with("[Could not save the full output: no execution environment]"));
    assert_eq!(
        documents.states.lock().await.store.values.get("x"),
        Some(&json!(1))
    );
    Ok(())
}

#[tokio::test]
async fn a_finished_script_cancels_outstanding_nested_effects_and_commits_only_pre_exit_writes()
-> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    let documents = Documents::default();
    documents
        .states
        .lock()
        .await
        .store
        .values
        .insert("x".into(), json!("previous"));
    let result = execute(
        &tool,
        &host,
        &documents,
        None,
        None,
        r#"
        tools.hold({}).catch(() => {});
        await describeTool('hold');
        text(load('x'));
        store('x', 'updated');
        exit();
        text('after');
        store('x', 'late');
    "#,
        &Context::background(),
    )
    .await?;
    assert!(!result.is_error);
    assert!(text(&result).ends_with("previous"));
    assert_eq!(
        result.details.calls[0].status,
        CodemodeCallStatus::Cancelled
    );
    assert!(host.hold_cancelled.load(Ordering::SeqCst));
    assert_eq!(
        documents.states.lock().await.store.values.get("x"),
        Some(&json!("updated"))
    );
    Ok(())
}

#[tokio::test]
async fn resume_checks_version_deadline_and_fingerprint_before_any_new_effect() -> TestResult {
    let tool = create_codemode_tool(CodemodeToolOptions::default());
    let host = tools();
    for problem in ["version", "deadline", "fingerprint"] {
        let documents = Documents::default();
        documents.reject_ack.store(true, Ordering::SeqCst);
        let code = "// @options: {\"timeout_ms\": 30000}\nreturn await tools.echo({})";
        let interrupted = execute(
            &tool,
            &host,
            &documents,
            None,
            None,
            code,
            &Context::background(),
        )
        .await;
        assert!(matches!(interrupted, Err(CodemodeError::Documents(_))));
        {
            let mut state = documents.states.lock().await;
            let start = state.run.start.as_mut().ok_or("missing start")?;
            match problem {
                "version" => start.version = 0,
                "deadline" => start.deadline = Some(0),
                _ => start.fingerprint = Some("previous guest".into()),
            }
        }
        documents.reject_ack.store(false, Ordering::SeqCst);
        let result = execute(
            &tool,
            &host,
            &documents,
            None,
            None,
            code,
            &Context::background(),
        )
        .await?;
        assert!(result.is_error);
        let message = match problem {
            "version" => "it started under code mode version",
            "deadline" => "its deadline passed while it was interrupted",
            _ => "the sandbox changed since it started",
        };
        assert!(text(&result).contains(message), "{result:?}");
    }
    assert!(host.calls.lock().await.is_empty());
    Ok(())
}
