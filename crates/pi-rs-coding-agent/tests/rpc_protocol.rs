use std::{io, sync::Arc, time::Duration};

use async_trait::async_trait;
use pi_rs_agent::{Agent, AgentInitialState, AgentOptions, QueueMode};
use pi_rs_ai::{
    ImageContent, Models, ModelsOptions,
    faux::{FauxHandle, FauxOptions, FauxStep, faux_assistant_message, faux_provider, faux_text},
};
use pi_rs_coding_agent::{
    core::{
        agent_session::{
            AgentSession, AgentSessionConfig, AuthStorage, Compactor,
            ExtensionRunner as SessionExtensions, InMemoryModelRegistry, InMemorySettings,
            ModelRegistry, NoopExtensionRunner, SessionError, SessionManager as SessionStore,
            SessionReason, SettingsStore, StaticAuth, TailCompactor,
        },
        agent_session_runtime::{
            AgentSessionRuntime, CreateAgentSessionRuntimeFactory, CreateAgentSessionRuntimeResult,
            RuntimeServices, RuntimeTarget, create_agent_session_runtime,
        },
        agent_session_services::DiskSession,
        event_bus::EventBus,
        extensions::{
            runner::{ExtensionRunner, command},
            types::{
                Extension, ExtensionFactory, ExtensionHost, ExtensionInitContext,
                ExtensionRegistration, UiDialogOptions, UiWidgetPlacement,
            },
        },
        session_manager::SessionManager,
    },
    modes::rpc::{
        JsonlReader, RpcClient, RpcDisposition, RpcExtensionUiRequest, RpcExtensionUiResponse,
        RpcUiMethod, run_stream, serialize_json_line,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, duplex, split},
    sync::broadcast,
    time::timeout,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const WAIT: Duration = Duration::from_secs(5);

struct Native(Arc<ExtensionRunner>);

impl SessionExtensions for Native {
    fn emit(&self, _event: &Value) -> Value {
        Value::Null
    }
    fn native(&self) -> Option<&Arc<ExtensionRunner>> {
        Some(&self.0)
    }
}

async fn runtime(
    cwd: &std::path::Path,
    faux: &FauxHandle,
    extension: Option<ExtensionFactory>,
) -> Result<AgentSessionRuntime, SessionError> {
    let model = faux.get_model(None).cloned().ok_or(SessionError::NoModel)?;
    let models = Models::new(ModelsOptions::default());
    models.set_provider(faux.provider.clone());
    let factory: CreateAgentSessionRuntimeFactory = Arc::new(move |target| {
        let model = model.clone();
        let models = models.clone();
        let extension = extension.clone();
        Box::pin(async move {
            let auth: Arc<dyn AuthStorage> = Arc::new(StaticAuth::allow_all());
            let registry: Arc<dyn ModelRegistry> = Arc::new(InMemoryModelRegistry::new(
                vec![model.clone()],
                auth.clone(),
            ));
            let settings: Arc<dyn SettingsStore> = Arc::new(InMemorySettings::default());
            let compactor: Arc<dyn Compactor> = Arc::new(TailCompactor { keep_recent: 1 });
            let extensions: Arc<dyn SessionExtensions> = if let Some(extension) = extension {
                let bus = Arc::new(EventBus::new());
                let host = Arc::new(ExtensionHost::new(bus.clone()));
                let runner = Arc::new(ExtensionRunner::new(Vec::new(), host, bus, &target.cwd));
                let loaded = runner.load_extensions(&[extension]).await;
                if let Some(error) = loaded.errors.first() {
                    return Err(SessionError::Extension(error.error.clone()));
                }
                Arc::new(Native(runner))
            } else {
                Arc::new(NoopExtensionRunner)
            };
            let agent = Agent::new(AgentOptions {
                initial_state: AgentInitialState {
                    model: Some(model),
                    ..AgentInitialState::default()
                },
                stream_fn: Some(pi_rs_agent::models_stream_fn(models)),
                ..AgentOptions::default()
            })?;
            let session = AgentSession::new(AgentSessionConfig {
                agent,
                session_manager: target.session_manager,
                settings: settings.clone(),
                auth: auth.clone(),
                models: registry.clone(),
                compactor: compactor.clone(),
                extensions: extensions.clone(),
                cwd: target.cwd.clone(),
                retry_prompt: None,
            })?;
            Ok(CreateAgentSessionRuntimeResult {
                session,
                services: RuntimeServices {
                    cwd: target.cwd,
                    agent_dir: target.agent_dir,
                    settings,
                    auth,
                    models: registry,
                    compactor,
                    extensions,
                },
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    });
    let manager: Arc<dyn SessionStore> =
        Arc::new(DiskSession::new(SessionManager::in_memory_at(cwd)));
    create_agent_session_runtime(
        factory,
        RuntimeTarget {
            cwd: manager.cwd(),
            agent_dir: None,
            session_manager: manager,
            reason: SessionReason::Startup,
        },
    )
    .await
}

#[tokio::test]
async fn rpc_strict_lf_crlf_utf8_and_final_record() -> TestResult {
    let value = json!({"text": "a\u{2028}b\u{2029}c", "cr": "a\rb", "lf": "a\nb"});
    let mut encoded = serialize_json_line(&value)?;
    let split_at = encoded
        .iter()
        .position(|&byte| byte == 0xe2)
        .ok_or("Unicode separator escaped")?
        + 1;
    assert_eq!(encoded.iter().filter(|&&byte| byte == b'\n').count(), 1);
    let mut first = encoded.split_off(split_at);
    let (mut send, receive) = duplex(512);
    send.write_all(&encoded).await?;
    let mut reader = JsonlReader::new(receive);
    assert!(
        timeout(Duration::from_millis(5), reader.next_line())
            .await
            .is_err()
    );
    first.pop();
    first.extend_from_slice(b"\r\n{\"final\":true}\r");
    send.write_all(&first).await?;
    send.shutdown().await?;
    let line = reader.next_line().await?.ok_or("missing Unicode record")?;
    assert!(!line.ends_with('\r'));
    assert_eq!(serde_json::from_str::<Value>(&line)?, value);
    assert_eq!(
        reader.next_line().await?.as_deref(),
        Some("{\"final\":true}")
    );
    assert!(reader.next_line().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn rpc_command_shapes_and_wire_errors() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions::default());
    let mut runtime = runtime(cwd.path(), &faux, None).await?;
    let name = "first\u{2028}second\u{2029}third";
    let mut input = Vec::new();
    let commands = [
        json!({"id":"state", "type":"get_state"}),
        json!({"id":"commands", "type":"get_commands"}),
        json!({"id":"tree", "type":"get_tree"}),
        json!({"id":"clone", "type":"clone"}),
        json!({"id":"badModel", "type":"set_model", "provider":"missing", "modelId":"x"}),
        json!({"id":"cycleModel", "type":"cycle_model"}),
        json!({"id":"cycleThinking", "type":"cycle_thinking_level"}),
        json!({"id":"setQueueMode", "type":"set_steering_mode", "mode":"all"}),
        json!({"id":"setFollowMode", "type":"set_follow_up_mode", "mode":"all"}),
        json!({"id":"setAutoCompaction", "type":"set_auto_compaction", "enabled":false}),
        json!({"id":"name", "type":"set_session_name", "name":format!("\u{feff} {name}  ")}),
        json!({"id":"after", "type":"get_state"}),
        json!({"id":"blank", "type":"set_session_name", "name":"   "}),
        json!({"id":"entries", "type":"get_entries"}),
        json!({"id":"badEntry", "type":"get_entries", "since":"missing"}),
        json!({"id":"stats", "type":"get_session_stats"}),
        json!({"id":"last", "type":"get_last_assistant_text"}),
        json!({"id":"export", "type":"export_html"}),
        json!({"id":"unknown", "type":"foobar"}),
        json!({"id":"invalid", "type":"set_steering_mode", "mode":"sometimes"}),
    ];
    for command in commands {
        input.extend(serialize_json_line(&command)?);
    }
    input.extend_from_slice(b"{\"type\":\"get_messages\",\"id\":\"messages\"}\r\n{bad json\n{\"type\":\"get_available_thinking_levels\"}");
    let mut output = Vec::new();
    timeout(
        WAIT,
        run_stream(&mut runtime, input.as_slice(), &mut output),
    )
    .await??;
    let output: Vec<Value> = output
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()?;
    let by_id = |id: &str| {
        output
            .iter()
            .find(|value| value["id"] == id)
            .ok_or_else(|| format!("missing response {id}"))
    };
    let state = &by_id("state")?["data"];
    assert_eq!(state["model"]["id"], "faux-1");
    assert!(
        !state["sessionId"]
            .as_str()
            .ok_or("missing sessionId")?
            .is_empty()
    );
    assert!(
        !state
            .as_object()
            .ok_or("state is not object")?
            .contains_key("sessionFile")
    );
    assert_eq!(state["isStreaming"], false);
    assert_eq!(state["isCompacting"], false);
    assert_eq!(state["steeringMode"], "one-at-a-time");
    assert_eq!(state["pendingMessageCount"], 0);
    assert_eq!(by_id("commands")?["data"], json!({"commands":[]}));
    assert_eq!(by_id("tree")?["data"], json!({"tree":[], "leafId":null}));
    assert_eq!(
        by_id("clone")?["error"],
        "Cannot clone session: no current entry selected"
    );
    assert_eq!(by_id("badModel")?["error"], "Model not found: missing/x");
    assert_eq!(by_id("cycleModel")?["data"], Value::Null);
    assert!(by_id("cycleModel")?.get("data").is_some());
    assert!(
        by_id("cycleThinking")?
            .get("data")
            .is_some_and(Value::is_null)
    );
    assert_eq!(
        by_id("name")?,
        &json!({ "id":"name", "type":"response", "command":"set_session_name", "success":true })
    );
    assert_eq!(by_id("after")?["data"]["sessionName"], name);
    assert_eq!(by_id("after")?["data"]["steeringMode"], "all");
    assert_eq!(by_id("after")?["data"]["followUpMode"], "all");
    assert_eq!(by_id("after")?["data"]["autoCompactionEnabled"], false);
    assert_eq!(by_id("blank")?["error"], "Session name cannot be empty");
    let entries = &by_id("entries")?["data"];
    assert_eq!(entries["entries"][0]["name"], name);
    assert_eq!(entries["entries"][0]["type"], "session_info");
    assert_eq!(entries["leafId"], entries["entries"][0]["id"]);
    assert_eq!(by_id("badEntry")?["error"], "Entry not found: missing");
    assert_eq!(by_id("stats")?["data"]["userMessages"], 0);
    assert!(
        by_id("stats")?["data"]["tokens"]
            .get("cacheWrite")
            .is_some()
    );
    assert_eq!(by_id("last")?["data"], json!({}));
    assert_eq!(
        by_id("export")?["error"],
        "Cannot export in-memory session to HTML"
    );
    assert_eq!(
        by_id("unknown")?,
        &json!({ "id":"unknown", "type":"response", "command":"foobar", "success":false, "error":"Unknown command: foobar" })
    );
    assert_eq!(by_id("invalid")?["command"], "set_steering_mode");
    assert_eq!(by_id("invalid")?["success"], false);
    assert_eq!(by_id("messages")?["data"], json!({"messages":[]}));
    assert!(output.iter().any(|event| {
        event["command"] == "parse"
            && event.get("id").is_none()
            && event["error"]
                .as_str()
                .is_some_and(|error| error.starts_with("Failed to parse command:"))
    }));
    assert!(
        output
            .iter()
            .any(|event| event["command"] == "get_available_thinking_levels"
                && event["data"]["levels"] == json!(["off"]))
    );
    Ok(())
}

struct DialogExtension {
    registration: ExtensionRegistration,
}

#[async_trait]
impl Extension for DialogExtension {
    fn name(&self) -> &str {
        "<test:dialog>"
    }
    fn registration(&self) -> &ExtensionRegistration {
        &self.registration
    }
    fn set_registration(&mut self, registration: ExtensionRegistration) {
        self.registration = registration;
    }

    async fn init(&mut self, context: &mut ExtensionInitContext<'_>) -> Result<(), String> {
        context.register_command(command(
            "dialog",
            Arc::new(|_, ctx| {
                Box::pin(async move {
                    let selected = ctx
                        .base
                        .ui
                        .select("select\u{2028}item", vec!["one".into(), "two".into()])
                        .await;
                    let confirmed = ctx.base.ui.confirm("confirm", "Continue?").await;
                    let cancelled = ctx.base.ui.confirm("cancel", "Continue?").await;
                    let typed = ctx
                        .base
                        .ui
                        .input_with_options(
                            "input",
                            None,
                            UiDialogOptions {
                                timeout_ms: Some(10),
                                cancel: None,
                                blocked_kind: None,
                            },
                        )
                        .await;
                    let edit = ctx.base.ui.editor("editor", Some("initial".into())).await;
                    ctx.base.ui.set_status(
                        "result",
                        Some(format!(
                            "{selected:?} {confirmed} {cancelled} {typed:?} {edit:?}"
                        )),
                    );
                    ctx.base
                        .ui
                        .set_widget("widget", None, Some(UiWidgetPlacement::BelowEditor));
                    ctx.base.ui.set_title("title");
                    ctx.base.ui.paste_to_editor("pasted");
                    ctx.base.ui.notify_with_type("done", None);
                    ctx.base.shutdown();
                    Ok(())
                })
            }),
        ))
    }
}

async fn next_type(events: &mut broadcast::Receiver<Value>, kind: &str) -> io::Result<Value> {
    loop {
        let event = events.recv().await.map_err(io::Error::other)?;
        if event["type"] == kind {
            return Ok(event);
        }
    }
}

#[tokio::test]
async fn rpc_ui_dialog_round_trip_and_shutdown_are_concurrent() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions::default());
    let mut runtime = runtime(
        cwd.path(),
        &faux,
        Some(Arc::new(|| {
            Ok(Box::new(DialogExtension {
                registration: ExtensionRegistration::new("<test:dialog>"),
            }))
        })),
    )
    .await?;
    let (server, client) = duplex(4096);
    let (server_read, server_write) = split(server);
    let (client_read, client_write) = split(client);
    let client = RpcClient::from_stream(client_read, client_write);
    let exercise = async {
        let commands = client.get_commands().await?;
        let command = commands
            .first()
            .ok_or_else(|| io::Error::other("missing extension command"))?;
        assert_eq!(command.name, "dialog");
        assert_eq!(
            serde_json::to_value(command)?["sourceInfo"]["origin"],
            "top-level"
        );
        let mut events = client.on_event()?;
        let drive_ui = async {
            let request = next_type(&mut events, "extension_ui_request").await?;
            assert_eq!(request["title"], "select\u{2028}item");
            assert_eq!(request["options"], json!(["one", "two"]));
            assert!(request.get("timeout").is_none());
            let select: RpcExtensionUiRequest = serde_json::from_value(request)?;
            // While the prompt awaits a UI response, an unrelated RPC request still completes.
            assert!(!client.get_state().await?.is_streaming);
            client.answer_ui(&RpcExtensionUiResponse {
                id: select.id,
                value: Some("two".into()),
                confirmed: None,
                cancelled: None,
            })?;
            let confirm: RpcExtensionUiRequest =
                serde_json::from_value(next_type(&mut events, "extension_ui_request").await?)?;
            assert!(matches!(confirm.method, RpcUiMethod::Confirm { .. }));
            client.answer_ui(&RpcExtensionUiResponse {
                id: confirm.id,
                value: None,
                confirmed: Some(true),
                cancelled: None,
            })?;
            let cancel: RpcExtensionUiRequest =
                serde_json::from_value(next_type(&mut events, "extension_ui_request").await?)?;
            assert!(matches!(cancel.method, RpcUiMethod::Confirm { .. }));
            client.answer_ui(&RpcExtensionUiResponse {
                id: cancel.id,
                value: None,
                confirmed: Some(true),
                cancelled: Some(true),
            })?;
            let input = next_type(&mut events, "extension_ui_request").await?;
            assert_eq!(input["method"], "input");
            assert_eq!(input["timeout"], 10);
            assert!(input.get("placeholder").is_none());
            let editor: RpcExtensionUiRequest =
                serde_json::from_value(next_type(&mut events, "extension_ui_request").await?)?;
            assert!(
                matches!(editor.method, RpcUiMethod::Editor { ref prefill, .. } if prefill.as_deref() == Some("initial"))
            );
            client.answer_ui(&RpcExtensionUiResponse {
                id: editor.id,
                value: Some("edited".into()),
                confirmed: None,
                cancelled: None,
            })?;
            let status = next_type(&mut events, "extension_ui_request").await?;
            assert_eq!(status["method"], "setStatus");
            assert_eq!(
                status["statusText"],
                "Some(\"two\") true false None Some(\"edited\")"
            );
            let widget = next_type(&mut events, "extension_ui_request").await?;
            assert_eq!(widget["method"], "setWidget");
            assert_eq!(widget["widgetPlacement"], "belowEditor");
            assert!(widget.get("widgetLines").is_none());
            assert_eq!(
                next_type(&mut events, "extension_ui_request").await?["method"],
                "setTitle"
            );
            assert_eq!(
                next_type(&mut events, "extension_ui_request").await?["method"],
                "set_editor_text"
            );
            let notice = next_type(&mut events, "extension_ui_request").await?;
            assert_eq!(notice["method"], "notify");
            assert!(notice.get("notifyType").is_none());
            Ok::<_, io::Error>(())
        };
        let (disposition, ()) =
            tokio::try_join!(client.prompt("/dialog", Vec::new(), None), drive_ui)?;
        assert_eq!(disposition, RpcDisposition::Handled);
        Ok(())
    };
    timeout(WAIT, async {
        tokio::try_join!(
            run_stream(&mut runtime, server_read, server_write),
            exercise
        )
    })
    .await??;
    assert_eq!(faux.state.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn rpc_prompt_acceptance_queue_and_abort_while_faux_streams() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions {
        tokens_per_second: Some(4.0),
        token_size: Some((1, 1)),
        ..FauxOptions::default()
    });
    faux.set_responses([FauxStep::from(faux_assistant_message(vec![faux_text(
        "a slow streaming response",
    )]))]);
    let mut runtime = runtime(cwd.path(), &faux, None).await?;
    let (server_in, mut client_out) = duplex(4096);
    let (mut client_in, server_out) = duplex(4096);
    let exercise = async {
        use pi_rs_coding_agent::modes::rpc::RpcStreamingBehavior;
        let mut reader = JsonlReader::new(&mut client_in);
        let image: ImageContent =
            serde_json::from_value(json!({"type":"image", "data":"YWJj", "mimeType":"image/png"}))?;
        client_out
            .write_all(&serialize_json_line(
                &json!({"type":"prompt", "id":"first", "message":"go", "images":[image]}),
            )?)
            .await?;
        let first = read_record(&mut reader).await?;
        assert_eq!(
            first,
            json!({ "type":"response", "id":"first", "command":"prompt", "success":true, "data":{"disposition":"started"} })
        );
        let _ = until_record(&mut reader, |value| value["type"] == "agent_start").await?;
        client_out.write_all(&serialize_json_line(&json!({"type":"prompt", "id":"queued", "message":"later", "streamingBehavior":RpcStreamingBehavior::FollowUp}))?).await?;
        let queued = until_record(&mut reader, |value| value["id"] == "queued").await?;
        assert_eq!(queued["data"], json!({ "disposition":"queued" }));
        client_out
            .write_all(b"{\"type\":\"clear_queue\",\"id\":\"clear\"}\n")
            .await?;
        let clear = until_record(&mut reader, |value| value["id"] == "clear").await?;
        assert_eq!(
            clear["data"],
            json!({ "steering":[], "followUp":["later"] })
        );
        client_out
            .write_all(b"{\"type\":\"abort\",\"id\":\"abort\"}\n")
            .await?;
        let abort = until_record(&mut reader, |value| value["id"] == "abort").await?;
        assert_eq!(abort["success"], true);
        client_out
            .write_all(b"{\"type\":\"get_state\",\"id\":\"state\"}\n")
            .await?;
        let state = until_record(&mut reader, |value| value["id"] == "state").await?;
        assert_eq!(state["data"]["isStreaming"], false);
        client_out.shutdown().await
    };
    timeout(WAIT, async {
        tokio::try_join!(run_stream(&mut runtime, server_in, server_out), exercise)
    })
    .await??;
    Ok(())
}

async fn read_record<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut JsonlReader<R>,
) -> io::Result<Value> {
    let line = reader
        .next_line()
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing RPC record"))?;
    Ok(serde_json::from_str(&line)?)
}

async fn until_record<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut JsonlReader<R>,
    predicate: impl Fn(&Value) -> bool,
) -> io::Result<Value> {
    loop {
        let value = read_record(reader).await?;
        if predicate(&value) {
            return Ok(value);
        }
    }
}

#[tokio::test]
async fn rpc_client_supports_typed_queued_modes_and_fresh_sessions() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions::default());
    let mut runtime = runtime(cwd.path(), &faux, None).await?;
    let (server, client) = duplex(4096);
    let (server_read, server_write) = split(server);
    let (client_read, client_write) = split(client);
    let mut client = RpcClient::from_stream(client_read, client_write);
    let exercise = async {
        client.set_follow_up_mode(QueueMode::All).await?;
        let old = client.get_state().await?;
        assert_eq!(old.follow_up_mode, QueueMode::All);
        assert_eq!(
            client.steer("queued", Vec::new()).await?,
            RpcDisposition::Queued
        );
        assert_eq!(client.clear_queue().await?.steering, ["queued"]);
        assert!(
            !client
                .new_session(Some("parent.jsonl".into()))
                .await?
                .cancelled
        );
        let state = client.get_state().await?;
        assert_ne!(old.session_id, state.session_id);
        assert!(client.get_entries(None).await?.entries.is_empty());
        client.stop().await
    };
    timeout(WAIT, async {
        tokio::try_join!(
            run_stream(&mut runtime, server_read, server_write),
            exercise
        )
    })
    .await??;
    let manager = runtime.session().session_manager();
    let disk = manager.disk().ok_or("not backed by v3 session")?.lock();
    assert_eq!(
        disk.get_header().parent_session.as_deref(),
        Some("parent.jsonl")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rpc_bash_streams_before_exit_and_can_be_aborted() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions::default());
    let mut runtime = runtime(cwd.path(), &faux, None).await?;
    let (server_in, mut client_out) = duplex(4096);
    let (mut client_in, server_out) = duplex(4096);
    let exercise = async {
        let mut reader = JsonlReader::new(&mut client_in);
        client_out.write_all(&serialize_json_line(&json!({
            "type": "bash", "id": "run", "command": "printf '\\033[31mready\\033[0m'; sleep 30", "excludeFromContext": true,
        }))?).await?;
        let update = read_record(&mut reader).await?;
        assert_eq!(
            update,
            json!({"type":"bash_execution_update", "id":"run", "delta":"ready"})
        );
        client_out
            .write_all(b"{\"type\":\"abort_bash\",\"id\":\"interrupt\"}\n")
            .await?;
        let abort = until_record(&mut reader, |value| value["id"] == "interrupt").await?;
        assert_eq!(abort["success"], true);
        let response = until_record(&mut reader, |value| {
            value["type"] == "response" && value["id"] == "run"
        })
        .await?;
        assert_eq!(
            response["data"],
            json!({"output":"ready", "cancelled":true, "truncated":false})
        );
        client_out
            .write_all(b"{\"type\":\"get_messages\",\"id\":\"history\"}\n")
            .await?;
        let history = until_record(&mut reader, |value| value["id"] == "history").await?;
        assert_eq!(history["data"]["messages"][0]["role"], "bashExecution");
        assert_eq!(history["data"]["messages"][0]["excludeFromContext"], true);
        client_out.shutdown().await
    };
    timeout(WAIT, async {
        tokio::try_join!(run_stream(&mut runtime, server_in, server_out), exercise)
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn rpc_switch_fork_clone_and_html_use_persisted_sessions() -> TestResult {
    let cwd = tempfile::tempdir()?;
    let faux = faux_provider(FauxOptions::default());
    let mut runtime = runtime(cwd.path(), &faux, None).await?;
    let mut saved = SessionManager::create_in(cwd.path(), cwd.path().join("sessions"), None)?;
    let first =
        saved.append_message(json!({ "role": "user", "content": "first", "timestamp": 1 }))?;
    saved.append_message(serde_json::to_value(faux_assistant_message(vec![
        faux_text("answer"),
    ]))?)?;
    let second =
        saved.append_message(json!({ "role": "user", "content": "second", "timestamp": 2 }))?;
    let saved_path = saved
        .get_session_file()
        .ok_or("missing persisted session file")?
        .to_string_lossy()
        .into_owned();
    let html = cwd.path().join("conversation.html");
    let (server, client) = duplex(4096);
    let (server_read, server_write) = split(server);
    let (client_read, client_write) = split(client);
    let mut client = RpcClient::from_stream(client_read, client_write);
    let exercise = async {
        assert!(!client.switch_session(&saved_path).await?.cancelled);
        assert_eq!(
            client.get_state().await?.session_file.as_deref(),
            Some(saved_path.as_str())
        );
        let forks = client.get_fork_messages().await?;
        assert_eq!(
            forks
                .iter()
                .map(|fork| (fork.entry_id.as_str(), fork.text.as_str()))
                .collect::<Vec<_>>(),
            [(&*first, "first"), (&*second, "second")]
        );
        assert_eq!(
            client.get_last_assistant_text().await?.as_deref(),
            Some("answer")
        );
        assert_eq!(
            client
                .get_entries(Some(second.clone()))
                .await?
                .entries
                .len(),
            0
        );
        assert_eq!(
            client.get_tree().await?.leaf_id.as_deref(),
            Some(second.as_str())
        );
        let html = html.to_string_lossy().into_owned();
        assert_eq!(client.export_html(Some(html.clone())).await?, html);
        assert!(std::fs::read_to_string(html)?.contains("answer"));
        assert!(!client.clone_session().await?.cancelled);
        assert_ne!(
            client.get_state().await?.session_file.as_deref(),
            Some(saved_path.as_str())
        );
        assert_eq!(client.get_messages().await?.len(), 3);
        let fork = client.fork(&second).await?;
        assert!(!fork.cancelled);
        assert_eq!(fork.text.as_deref(), Some("second"));
        assert_eq!(client.get_messages().await?.len(), 2);
        client.stop().await
    };
    timeout(WAIT, async {
        tokio::try_join!(
            run_stream(&mut runtime, server_read, server_write),
            exercise
        )
    })
    .await??;
    Ok(())
}
