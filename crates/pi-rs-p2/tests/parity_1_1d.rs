//! Focused parity tests for Workstream 1.1d (`pi-rs-p2` server & built-in plugins).

#![forbid(unsafe_code)]

use std::{
    error::Error as StdError,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use pi_rs_ai::{
    InputContent, StopReason,
    faux::{
        FauxModelDefinition, FauxOptions, FauxStep, faux_assistant_message, faux_provider,
        faux_text, faux_tool_call,
    },
    registry::Models,
};
use pi_rs_chord::{
    context::Context,
    host::Caller,
    json::JsonValue,
    node::workers::{Control, Frame, WorkerOptions, WorkerSession, write_frame},
    sqlite::{Options as SqliteOptions, SqliteDatabase},
    state::Batch,
    telemetry::SqliteTelemetry,
};
use pi_rs_codemode::{CodemodeSandbox, SandboxOptions};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID, SubmissionStatus,
    harness::{Extension, ToolRegistration},
    tools::ToolResult,
};
use pi_rs_p2::{
    api::v1::{PluginRegistry, p2_contracts::lookup, server::service_of, session_address},
    cli::{
        compare_reports, data_report, data_report_command, migrate_data, migrate_data_command,
        mint_owner_link, parse_owner_origin, reload_command, traces_command, verify_sessions,
        verify_sessions_command,
    },
    hosts::{
        server::{P2Server, ServerOptions, start_server},
        worker::{start_session_worker, start_session_worker_configured},
    },
    plugins::{
        agent_plugins::{create_agent_plugins_extension, materialize_bundled_skills},
        artifacts::viewer::{RouteLayer, ViewerFileInfo, ViewerItem, viewer_item, viewer_layer},
        backup::{BACKUP_TARGETS, BackupTargets},
        codemode::{RunningCalls, build_codemode_extension_with},
        dev_tools::{
            OnCheckoutFn, PluginOutcomeEntry, PluginReloadResult, ReloadFn, create_reload_extension,
        },
        envs::{
            config::GondolinEnvConfig,
            sandbox_host::{
                launcher_id, parse_status_report, run_launcher_command, sandbox_argument,
                sandbox_revision, shell_quote,
            },
            secrets::{delete_secret, list_secrets, sandbox_secrets, save_secret, secrets_path},
        },
        files::upload_path,
        models::resources::ModelResources,
        powershell::{create_powershell_extension, sync_powershell_offered},
        prompt::contract::{PLUGIN_SKILLS_SERVICE, PluginSkillsService},
        register_builtins,
        telemetry::{
            SqliteTelemetryBackend, TELEMETRY_STORE_SERVICE, TelemetryBackend, telemetry_enabled,
        },
    },
    runtime::{
        local_owner::{LOCAL_CHALLENGE_PATH, LOCAL_CLAIM_PATH, local_owner_proof},
        session_lock::{LockCapability, SessionLock},
    },
};
use pi_rs_telemetry::{MetricQuery, TraceQuery};
use serde_json::{Value, json};
use tokio::io::DuplexStream;

type TestResult<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
const WORKER_TIMEOUT: Duration = Duration::from_secs(15);

fn owner_caller() -> Caller {
    Caller {
        principal: Some(json!({ "kind": "owner", "access": "local" })),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

async fn start_all_builtins_server(dir: &Path, telemetry: Option<bool>) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(dir.to_owned());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    options.telemetry = telemetry;
    register_builtins(&mut options.registry)?;
    let mut process = WorkerOptions::new(env!("CARGO_BIN_EXE_p2").into(), dir.join("worker-locks"));
    process.args.push("--worker".into());
    options.worker_process = Some(process);
    Ok(start_server(options).await?)
}

async fn handshake_worker(
    data_dir: &Path,
    cwd: &Path,
    session_id: &str,
) -> TestResult<(WorkerSession, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (child_read, child_write) = tokio::io::split(child);
    let start = Frame::Control(Control::Start {
        address: session_address(session_id),
        start: json!({
            "app": data_dir,
            "dataDir": data_dir,
            "fake": true,
            "telemetry": false,
            "generation": 1,
            "values": {
                "session": {
                    "id": session_id,
                    "cwd": cwd,
                    "model": { "provider": "faux", "modelId": "faux-1" },
                    "thinkingLevel": "off",
                    "resume": true,
                }
            }
        }),
        scripts: Vec::new(),
        lock: data_dir.join(format!("{session_id}.lock")),
    });
    write_frame(&mut parent, &start).await?;
    let worker = tokio::time::timeout(
        WORKER_TIMEOUT,
        WorkerSession::start(child_read, child_write),
    )
    .await??;
    Ok((worker, parent))
}

async fn claim_local_owner_cookie(
    client: &reqwest::Client,
    origin: &str,
    port: u16,
    data_dir: &Path,
) -> TestResult<String> {
    let key_path = data_dir.join("owner-key");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let key = loop {
        match std::fs::read_to_string(&key_path) {
            Ok(key) => break key,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(err) => return Err(err.into()),
        }
    };
    let challenge_resp: Value = client
        .post(format!("{origin}{LOCAL_CHALLENGE_PATH}"))
        .header(reqwest::header::ORIGIN, origin)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let challenge = challenge_resp["challenge"]
        .as_str()
        .ok_or("missing challenge")?;
    let proof = local_owner_proof(key.trim(), port, challenge);
    let claim_resp = client
        .post(format!("{origin}{LOCAL_CLAIM_PATH}"))
        .header(reqwest::header::ORIGIN, origin)
        .json(&json!({ "challenge": challenge, "proof": proof }))
        .send()
        .await?
        .error_for_status()?;
    let raw = claim_resp
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing Set-Cookie header")?;
    Ok(raw.split(';').next().ok_or("empty Set-Cookie")?.to_owned())
}

// ---------------------------------------------------------------------------
// Item 1: P2_BACKUP=radius
// ---------------------------------------------------------------------------

#[tokio::test]
async fn radius_offers_backup_target_on_server_host() -> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start_all_builtins_server(dir.path(), Some(false)).await?;
    let targets = service_of::<BackupTargets>(&server.host, BACKUP_TARGETS)?;
    // `radius` must already be offered on `BackupTargets`; opening while signed out fails inside
    // Radius, NOT with `"no loaded plugin offers this backup target"`.
    let Err(err) = targets.open("radius").await else {
        server.close().await?;
        return Err("expected signed-out radius target open to report auth error".into());
    };
    let msg = err.to_string();
    assert!(
        !msg.contains("no loaded plugin offers this backup target"),
        "expected radius target to be registered, got: {msg}"
    );
    server.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 2: scope-prompt -> MessageContexts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_prompt_injects_staged_block_via_message_contexts_and_drops_when_carried()
-> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start_all_builtins_server(dir.path(), Some(false)).await?;
    let context = Context::background();

    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call("create", json!({ "cwd": dir.path() }).into(), &context)
        .await?
        .to_serde()?;
    let session_id = created["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();

    let stage = tokio::time::timeout(
        Duration::from_secs(10),
        server.workers.hold(
            Arc::new(lookup("p2.scope-stage")?),
            json!({ "session": session_id }).into(),
            owner_caller(),
            &context,
        ),
    )
    .await??;
    stage
        .call(
            "stage",
            json!({
                "requestId": "req-42",
                "block": "<scope>crates/pi-rs-p2/src/cli.rs</scope>",
                "note": "Focused on CLI checks."
            })
            .into(),
            &context,
        )
        .await?;

    let chat = tokio::time::timeout(
        Duration::from_secs(10),
        server.workers.hold(
            Arc::new(lookup("p2.chat")?),
            json!({ "session": session_id, "conversation": 1 }).into(),
            owner_caller(),
            &context,
        ),
    )
    .await??;

    // First send consumes staged `req-42` via views send router -> MessageContexts.
    chat.call(
        "send",
        json!({ "requestId": "req-42", "text": "hello scoped" }).into(),
        &context,
    )
    .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (before_count, entries_json) = loop {
        let entries: Value = chat
            .call(
                "entries",
                json!({ "limit": 20, "order": "ascending" }).into(),
                &context,
            )
            .await?
            .to_serde()?;
        let items = entries["items"].as_array().cloned().unwrap_or_default();
        if items.iter().any(|i| i["kind"] == "pi.assistant") {
            break (items.len(), serde_json::to_string(&items)?);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for scoped assistant reply"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        entries_json.contains("<scope>crates/pi-rs-p2/src/cli.rs</scope>"),
        "expected injected scope block in entries: {entries_json}"
    );
    assert!(
        entries_json.contains("Focused on CLI checks."),
        "expected scope note in entries: {entries_json}"
    );

    // Second send with a fresh requestId must NOT re-inject anything; `carried` dropped `req-42`.
    chat.call(
        "send",
        json!({ "requestId": "req-43", "text": "plain follow-up" }).into(),
        &context,
    )
    .await?;
    let second_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let new_items_json = loop {
        let entries: Value = chat
            .call(
                "entries",
                json!({ "limit": 20, "order": "ascending" }).into(),
                &context,
            )
            .await?
            .to_serde()?;
        let items = entries["items"].as_array().cloned().unwrap_or_default();
        if items.len() > before_count {
            let tail = &items[before_count..];
            let s = serde_json::to_string(tail)?;
            if tail.iter().any(|i| i["kind"] == "pi.assistant") && s.contains("plain follow-up") {
                break s;
            }
        }
        assert!(
            tokio::time::Instant::now() < second_deadline,
            "timed out waiting for second un-scoped assistant reply"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        !new_items_json.contains("<scope>crates/pi-rs-p2/src/cli.rs</scope>"),
        "carried callback must consume staged block before second send: {new_items_json}"
    );
    let stage_store = pi_rs_p2::plugins::scope_prompt::ScopeStageStore::default();
    stage_store.stage("k1".into(), "b1".into(), "n1".into());
    assert!(stage_store.get("k1").is_some());
    assert!(stage_store.drop_staged("k1"));
    assert!(stage_store.get("k1").is_none());

    chat.release();
    stage.release();
    sessions.release();
    server.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 3: codemode live tools, models, store/load, and RunningCalls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codemode_executes_live_tool_store_load_and_tracks_running_calls() -> TestResult {
    let dir = tempfile::tempdir()?;
    let context = Context::background();
    let (session, _parent) = handshake_worker(dir.path(), dir.path(), "cm-worker").await?;

    let echo_executed = Arc::new(AtomicBool::new(false));
    let echo_flag = echo_executed.clone();
    let calls = Arc::new(RunningCalls::default());
    let calls_worker = calls.clone();

    let faux = Arc::new(faux_provider(FauxOptions {
        api: Some("p2-faux".into()),
        provider: Some("faux".into()),
        models: vec![FauxModelDefinition {
            id: "faux-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    let code = r#"
        const reply = await tools.test_echo({ msg: "ping" });
        await store("saved-key", { nested: reply.content?.[0]?.text ?? reply });
        return await load("saved-key");
    "#;
    let mut tool_msg = faux_assistant_message(vec![faux_tool_call(
        "codemode",
        serde_json::Map::from_iter([("code".into(), Value::String(code.into()))]),
        Some("call_cm_1".into()),
    )]);
    tool_msg.stop_reason = StopReason::ToolUse;
    faux.append_responses([
        FauxStep::from(tool_msg),
        FauxStep::from(faux_assistant_message(vec![faux_text("codemode complete")])),
    ]);
    let models = Models::default();
    models.set_provider(faux.provider.clone());
    let models_cm = models.clone();

    let worker = start_session_worker_configured(
        session,
        &PluginRegistry::new(),
        None,
        models,
        move |process| {
            let echo_ext = Extension {
                name: "test-echo".into(),
                tools: vec![ToolRegistration {
                    declaration: serde_json::from_value(json!({
                        "name": "test_echo",
                        "description": "Echoes a value",
                        "parameters": {
                            "type": "object",
                            "properties": { "msg": { "type": "string" } },
                            "required": ["msg"]
                        }
                    }))?,
                    sequential: false,
                    prepare_arguments: None,
                    cancel: None,
                    structured_output_schema: None,
                    output_limits: None,
                    execute: Arc::new(move |args, _, _| {
                        let echo_flag = echo_flag.clone();
                        Box::pin(async move {
                            echo_flag.store(true, Ordering::Release);
                            Ok(ToolResult {
                                output: Some(vec![InputContent::Text(
                                    format!("echoed:{}", args["msg"].as_str().unwrap_or("")).into(),
                                )]),
                                ..Default::default()
                            }
                            .into())
                        })
                    }),
                    replay_safe: true,
                    callers: None,
                    deferred: false,
                }],
                ..Default::default()
            };
            process.registries.extensions.install(echo_ext)?;
            let sandbox = CodemodeSandbox::new(SandboxOptions {
                memory_limit_bytes: Some(64 * 1024 * 1024),
                ..Default::default()
            })
            .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
            let cm_ext = build_codemode_extension_with(
                sandbox,
                Some(models_cm),
                Some(Arc::downgrade(process)),
                calls_worker,
            );
            process.registries.extensions.install(cm_ext)?;
            Ok(Vec::new())
        },
    )
    .await?;

    let harness = worker.process.harness()?;
    let root = harness.durable.root(&context).await?;
    let input = serde_json::from_value(json!({ "type": "input", "content": "run codemode" }))?;
    tokio::time::timeout(Duration::from_secs(45), async {
        let sub = root.submit(input, &context).await?;
        let status = sub.wait(&context).await?;
        assert_eq!(status.status, SubmissionStatus::Done);
        harness.durable.wait_for_idle(&context).await?;
        Ok::<_, pi_rs_durable::session::Error>(())
    })
    .await??;

    assert_eq!(calls.count(), 0, "RunningCalls count must drain to zero");
    calls.idle().await;
    assert!(
        echo_executed.load(Ordering::Acquire),
        "codemode script must invoke live conversation tool `test_echo`"
    );
    let entries = root
        .entries(Default::default(), 32, None, &context)
        .await?
        .items;
    let serialized = serde_json::to_string(&entries)?;
    assert!(
        serialized.contains("echoed:ping"),
        "expected tool + store/load result in entries: {serialized}"
    );

    worker.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 4: powershell platform probe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn powershell_sync_offered_handles_windows_unix_and_unknown_environments() -> TestResult {
    let dir = tempfile::tempdir()?;
    let context = Context::background();
    let (session, _parent) = handshake_worker(dir.path(), dir.path(), "ps-test").await?;
    let faux = Arc::new(faux_provider(FauxOptions {
        api: Some("p2-faux".into()),
        provider: Some("faux".into()),
        models: vec![FauxModelDefinition {
            id: "faux-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    let models = Models::default();
    models.set_provider(faux.provider.clone());
    let worker = start_session_worker_configured(
        session,
        &PluginRegistry::new(),
        None,
        models,
        move |process| {
            process
                .registries
                .extensions
                .install(create_powershell_extension())?;
            Ok(Vec::new())
        },
    )
    .await?;
    let harness = worker.process.harness()?;
    let batch = Batch::new(|_| {});
    let offered = pi_rs_chord::state::Source::new(batch, json!({}).into())?;

    let unknown_win = sync_powershell_offered(
        &harness.durable,
        |_| async { Some(true) },
        &offered,
        &context,
    )
    .await?;
    assert!(!unknown_win);
    assert_eq!(offered.value()?.to_json().to_serde()?, json!({ "1": true }));

    let unknown_unix = sync_powershell_offered(
        &harness.durable,
        |_| async { Some(false) },
        &offered,
        &context,
    )
    .await?;
    assert!(!unknown_unix);
    assert_eq!(
        offered.value()?.to_json().to_serde()?,
        json!({ "1": false })
    );

    let unknown_none =
        sync_powershell_offered(&harness.durable, |_| async { None }, &offered, &context).await?;
    assert!(unknown_none, "None probe must flag unknown=true for retry");

    worker.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 5: agent-plugins writing-p2-plugins bundled skill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_plugins_materializes_writing_p2_plugins_skill_and_contributes_to_plugin_skills()
-> TestResult {
    let dir = tempfile::tempdir()?;
    let skill_root = materialize_bundled_skills(dir.path())?;
    let skill_md = skill_root.join("writing-p2-plugins/SKILL.md");
    assert!(
        skill_md.is_file(),
        "expected bundled SKILL.md at {}",
        skill_md.display()
    );
    let content = std::fs::read_to_string(&skill_md)?;
    assert!(
        content.contains("writing-p2-plugins") || content.contains("p2"),
        "expected non-empty plugin authoring skill"
    );

    let ext = create_agent_plugins_extension(
        dir.path().to_string_lossy().into_owned(),
        Arc::new(|| Some("remote-box".into())),
    );
    assert_eq!(ext.sections.len(), 1);

    let mut registry = PluginRegistry::new();
    register_builtins(&mut registry)?;
    let (session, _parent) = handshake_worker(dir.path(), dir.path(), "skills-worker").await?;
    let worker = start_session_worker(session, &registry, Some(&["agent-plugins".into()])).await?;

    let plugin_skills = service_of::<PluginSkillsService>(&worker.host, PLUGIN_SKILLS_SERVICE)?;
    let dirs = plugin_skills.dirs();
    assert!(
        dirs.iter()
            .any(|d| d.ends_with("bundled-skills/agent-plugins")),
        "expected PluginSkillsService to include bundled-skills/agent-plugins, got: {dirs:?}"
    );
    worker.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 6: P2_PROVIDER / P2_MODEL default fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_resources_uses_env_default_model_as_fallback_behind_settings_json() -> TestResult {
    let dir = tempfile::tempdir()?;
    let resources = ModelResources::open_with_default(
        dir.path().to_path_buf(),
        true,
        Some(("faux".into(), "faux-1".into())),
    )
    .await?;
    let summary = resources.summary();
    let chosen = summary
        .default_model
        .ok_or("expected default_model from override")?;
    assert_eq!(chosen.provider, "faux");
    assert_eq!(chosen.model_id, "faux-1");

    // `settings.json` must take precedence over `default_model_override`.
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_vec(&json!({
            "defaultModel": { "provider": "fake", "modelId": "fake" }
        }))?,
    )?;
    resources.reload().await?;
    let overridden = resources
        .summary()
        .default_model
        .ok_or("expected default_model from settings.json")?;
    assert_eq!(overridden.provider, "fake");
    assert_eq!(overridden.model_id, "fake");

    // CLI binary must reject partial P2_PROVIDER without P2_MODEL.
    let partial_out = tokio::process::Command::new(env!("CARGO_BIN_EXE_p2"))
        .env("P2_DATA_DIR", dir.path())
        .env("P2_PROVIDER", "faux")
        .env_remove("P2_MODEL")
        .env_remove("PI_MOBILE_PROVIDER")
        .env_remove("PI_MOBILE_MODEL")
        .output()
        .await?;
    assert!(!partial_out.status.success());
    let stderr = String::from_utf8_lossy(&partial_out.stderr);
    assert!(
        stderr.contains("Set both P2_PROVIDER and P2_MODEL"),
        "expected P2_PROVIDER/P2_MODEL validation error, got stderr: {stderr}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 7: Missing HTTP routes (/upload, /download, /owner, /owner/check)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_upload_download_range_and_owner_routes_enforce_contracts() -> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start_all_builtins_server(dir.path(), Some(false)).await?;
    let context = Context::background();
    let origin = format!("http://{}", server.address);
    let port = server.address.port();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let cookie = claim_local_owner_cookie(&client, &origin, port, dir.path()).await?;

    // 1. GET /owner and HEAD /owner return 200 text/html (empty body on HEAD).
    let get_owner = client.get(format!("{origin}/owner")).send().await?;
    assert_eq!(get_owner.status(), reqwest::StatusCode::OK);
    let head_owner = client.head(format!("{origin}/owner")).send().await?;
    assert_eq!(head_owner.status(), reqwest::StatusCode::OK);
    assert!(head_owner.bytes().await?.is_empty());

    // 2. GET /owner/check: 401 HTML when unauthenticated, 200 JSON when authenticated.
    let unauth_check = client.get(format!("{origin}/owner/check")).send().await?;
    assert_eq!(unauth_check.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unauth_ct = unauth_check
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        unauth_ct.starts_with("text/html"),
        "got content-type: {unauth_ct}"
    );

    let auth_check: Value = client
        .get(format!("{origin}/owner/check"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(auth_check, json!({ "owner": true }));

    // Create a live session so its worker serves `p2.session-files`.
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call("create", json!({ "cwd": dir.path() }).into(), &context)
        .await?
        .to_serde()?;
    let session_id = created["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();

    // 3. POST /upload: rejects unauthenticated (403) and accepts valid owner upload.
    let unauth_upload = client
        .post(format!(
            "{origin}/upload?session={session_id}&name=note.txt"
        ))
        .header(reqwest::header::ORIGIN, &origin)
        .body("hello")
        .send()
        .await?;
    assert_eq!(unauth_upload.status(), reqwest::StatusCode::FORBIDDEN);

    let upload_resp = client
        .post(format!(
            "{origin}/upload?session={session_id}&name=note.txt"
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .header(reqwest::header::ORIGIN, &origin)
        .body("hello upload")
        .send()
        .await?;
    assert_eq!(upload_resp.status(), reqwest::StatusCode::OK);
    let uploaded: Value = upload_resp.json().await?;
    let uploaded_id = uploaded["id"].as_str().ok_or("missing upload id")?;
    assert_eq!(
        upload_path(uploaded_id, "note.txt"),
        format!("{uploaded_id}-note.txt")
    );

    // 4. Write a seekable download snapshot (`clip.mp3`) into `p2.session-files` and test GET / HEAD / Range.
    let server_caller = Caller {
        principal: Some(json!({ "kind": "server" })),
        incarnation: uuid::Uuid::new_v4().to_string(),
    };
    let session_files = server
        .workers
        .hold(
            Arc::new(lookup("p2.session-files")?),
            json!({ "session": session_id }).into(),
            server_caller,
            &context,
        )
        .await?;
    let begun: Value = session_files
        .call(
            "begin",
            json!({ "space": "downloads", "name": "clip.mp3", "source": "owner" }).into(),
            &context,
        )
        .await?
        .to_serde()?;
    let token = begun["upload"]
        .as_str()
        .ok_or("missing begin upload token")?;
    let payload = b"0123456789abcdef";
    session_files
        .call(
            "append",
            json!({ "upload": token, "offset": 0, "bytes": payload.to_vec() }).into(),
            &context,
        )
        .await?;
    let finished: Value = session_files
        .call(
            "finish",
            JsonValue::from_serde(Value::String(token.to_owned())),
            &context,
        )
        .await?
        .to_serde()?;
    let download_id = finished["id"].as_str().ok_or("missing download id")?;

    let dl_url = format!("{origin}/download?session={session_id}&id={download_id}");
    let full = client
        .get(&dl_url)
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?;
    assert_eq!(full.status(), reqwest::StatusCode::OK);
    assert_eq!(full.headers()["accept-ranges"], "bytes");
    assert_eq!(full.bytes().await?.as_ref(), payload);

    let head = client
        .head(&dl_url)
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?;
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "16");
    assert!(head.bytes().await?.is_empty());

    let partial = client
        .get(&dl_url)
        .header(reqwest::header::COOKIE, &cookie)
        .header(reqwest::header::RANGE, "bytes=2-5")
        .send()
        .await?;
    assert_eq!(partial.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.headers()["content-range"], "bytes 2-5/16");
    assert_eq!(partial.bytes().await?.as_ref(), b"2345");

    let bad_range = client
        .get(&dl_url)
        .header(reqwest::header::COOKIE, &cookie)
        .header(reqwest::header::RANGE, "bytes=99-120")
        .send()
        .await?;
    assert_eq!(
        bad_range.status(),
        reqwest::StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert_eq!(bad_range.headers()["content-range"], "bytes */16");

    session_files.release();
    sessions.release();
    server.close().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 8: dev-tools, sessions, envs, artifacts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dev_tools_extension_exposes_reload_plugins_and_p2_development_section() -> TestResult {
    let dir = tempfile::tempdir()?;
    let context = Context::background();
    let (session, _parent) = handshake_worker(dir.path(), dir.path(), "dev-sess").await?;

    let called_with = Arc::new(Mutex::new(None::<Option<Vec<String>>>));
    let capture = called_with.clone();
    let reload_fn: ReloadFn = Arc::new(move |plugins, _| {
        let capture = capture.clone();
        Box::pin(async move {
            *capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(plugins);
            Ok(PluginReloadResult {
                refused: None,
                plugins: vec![PluginOutcomeEntry {
                    plugin: "files".into(),
                    outcome: "applied".into(),
                    errors: None,
                }],
                logs: Vec::new(),
            })
        })
    });

    let faux = Arc::new(faux_provider(FauxOptions {
        api: Some("p2-faux".into()),
        provider: Some("faux".into()),
        models: vec![FauxModelDefinition {
            id: "faux-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    let mut tool_msg = faux_assistant_message(vec![faux_tool_call(
        "reload_plugins",
        serde_json::Map::from_iter([("plugins".into(), json!(["files"]))]),
        Some("call_reload_1".into()),
    )]);
    tool_msg.stop_reason = StopReason::ToolUse;
    faux.append_responses([
        FauxStep::from(tool_msg),
        FauxStep::from(faux_assistant_message(vec![faux_text("reloaded")])),
    ]);
    let models = Models::default();
    models.set_provider(faux.provider.clone());

    let checkout_flag = Arc::new(AtomicBool::new(true));
    let flag_clone = checkout_flag.clone();
    let on_checkout: OnCheckoutFn = Arc::new(move || flag_clone.load(Ordering::Acquire));

    let worker = start_session_worker_configured(
        session,
        &PluginRegistry::new(),
        None,
        models,
        move |process| {
            let ext = create_reload_extension(reload_fn, on_checkout, Arc::downgrade(process));
            process.registries.extensions.install(ext)?;
            Ok(Vec::new())
        },
    )
    .await?;

    let harness = worker.process.harness()?;
    let snapshot = harness.durable.registry.snapshot();
    let installed = snapshot
        .extension("p2-dev-tools")
        .ok_or("missing p2-dev-tools extension")?;
    assert_eq!(installed.sections.len(), 1);
    let render_fn = installed.sections[0].render.clone();
    let sec_text =
        (render_fn)(&json!({ "conversationId": ROOT_CONVERSATION_ID }), &context).await?;
    assert!(
        sec_text
            .as_deref()
            .is_some_and(|s| s.contains("reload_plugins")),
        "expected p2-development prompt section"
    );

    checkout_flag.store(false, Ordering::Release);
    assert!(
        (render_fn)(&json!({ "conversationId": ROOT_CONVERSATION_ID }), &context)
            .await?
            .is_none()
    );

    let root = harness.durable.root(&context).await?;
    let input = serde_json::from_value(json!({ "type": "input", "content": "reload now" }))?;
    tokio::time::timeout(WORKER_TIMEOUT, async {
        let sub = root.submit(input, &context).await?;
        let status = sub.wait(&context).await?;
        assert_eq!(status.status, SubmissionStatus::Done);
        harness.durable.wait_for_idle(&context).await?;
        Ok::<_, pi_rs_durable::session::Error>(())
    })
    .await??;

    assert_eq!(
        *called_with.lock().unwrap_or_else(|e| e.into_inner()),
        Some(Some(vec!["files".to_owned()]))
    );
    let entries = root
        .entries(Default::default(), 32, None, &context)
        .await?
        .items;
    assert!(serde_json::to_string(&entries)?.contains("files: applied"));

    worker.close().await?;
    Ok(())
}

#[tokio::test]
async fn sessions_server_lock_and_rename_self_work_on_live_builtins_server() -> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start_all_builtins_server(dir.path(), Some(false)).await?;
    let context = Context::background();

    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call("create", json!({ "cwd": dir.path() }).into(), &context)
        .await?
        .to_serde()?;
    let session_id = created["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();

    // Set a lock via `p2.session-lock` and verify a worker-scoped view observes it.
    let locks = server.host.hold_as(
        Arc::new(lookup("p2.session-lock")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    locks.wait(&context).await?;
    let lock = SessionLock {
        mcp: vec!["github".into()],
        project_mcp: true,
        instructions: true,
        capabilities: vec![LockCapability::Ports],
    };
    locks
        .call(
            "set",
            json!({ "session": session_id, "lock": lock }).into(),
            &context,
        )
        .await?;

    let mut worker_caller = owner_caller();
    worker_caller.principal =
        Some(json!({ "kind": "worker", "session": session_id, "generation": 1 }));
    let worker_view = server.host.hold_as(
        Arc::new(lookup("p2.session-lock")?),
        json!({}).into(),
        worker_caller.clone(),
        &context,
    )?;
    worker_view.wait(&context).await?;
    let observed: Value = worker_view.member("lock")?.value()?.to_json().to_serde()?;
    assert_eq!(observed["mcp"], json!(["github"]));

    // Clear the lock.
    locks
        .call(
            "set",
            json!({ "session": session_id, "lock": Value::Null }).into(),
            &context,
        )
        .await?;
    assert!(
        worker_view
            .member("lock")?
            .value()?
            .to_json()
            .to_serde()?
            .is_null()
    );

    // Worker `renameSelf` renames the session cleanly via `p2.sessions`.
    let worker_sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        worker_caller,
        &context,
    )?;
    worker_sessions.wait(&context).await?;
    let renamed: Value = worker_sessions
        .call(
            "renameSelf",
            json!({ "title": "Renamed Task" }).into(),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(renamed["previous"], "New session");

    let catalog = server.host.hold_as(
        Arc::new(lookup("p2.catalog")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    catalog.wait(&context).await?;
    let row: Value = catalog
        .call("get", json!({ "id": session_id }).into(), &context)
        .await?
        .to_serde()?;
    assert_eq!(row["title"], "Renamed Task");
    catalog.release();

    worker_sessions.release();
    worker_view.release();
    locks.release();
    sessions.release();
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn envs_secrets_use_sandbox_secrets_json_with_legacy_migration_and_sandbox_host_args()
-> TestResult {
    let dir = tempfile::tempdir()?;
    assert_eq!(
        secrets_path(dir.path()),
        dir.path().join("sandbox-secrets.json")
    );

    // Legacy `<data>/sandboxes/<name>/secrets.json` is readable before primary file exists.
    let legacy_dir = dir.path().join("sandboxes/box1");
    std::fs::create_dir_all(&legacy_dir)?;
    std::fs::write(
        legacy_dir.join("secrets.json"),
        serde_json::to_vec(&json!({
            "OLD_KEY": {
                "hosts": ["api.example.com"],
                "value": "old-secret",
                "set_at": 100
            }
        }))?,
    )?;
    let listed = list_secrets(dir.path(), "box1").await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "OLD_KEY");

    save_secret(
        dir.path(),
        "box1",
        "OPENAI_API_KEY",
        vec!["api.openai.com".into()],
        Some("openai".into()),
        Some("sk-test-123".into()),
        None,
    )
    .await?;
    assert!(secrets_path(dir.path()).is_file());
    let (metas, values) = sandbox_secrets(dir.path(), "box1").await?;
    assert!(metas.iter().any(|s| s.name == "OPENAI_API_KEY"));
    assert_eq!(
        values.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-123")
    );
    delete_secret(dir.path(), "box1", Some("OPENAI_API_KEY")).await?;
    let after_del = list_secrets(dir.path(), "box1").await?;
    assert!(!after_del.iter().any(|s| s.name == "OPENAI_API_KEY"));

    // Sandbox host command/argument helpers parity.
    assert_eq!(launcher_id().len(), 16);
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    assert_eq!(
        run_launcher_command("~/.pi/p2/sandbox/abc", &["status", "--json"]),
        "sh '~/.pi/p2/sandbox/abc/p2-sandbox.sh' 'status' '--json'"
    );
    let cfg = GondolinEnvConfig {
        host: "builder".into(),
        image: None,
        mounts: Some(Vec::new()),
        allow_hosts: Some(vec!["api.openai.com".into()]),
        cpus: Some(2),
        memory_mb: Some(2048),
        disk_gb: Some(32),
    };
    let arg_b64 = sandbox_argument(&cfg, &metas);
    assert!(!arg_b64.is_empty());
    let rev = sandbox_revision(&cfg, &metas);
    assert_eq!(rev.len(), 16);
    let parsed = parse_status_report(
        &json!({
            "state": "running",
            "startedAt": 123,
            "cpus": 2,
            "memoryMb": 2048,
            "revision": rev,
        }),
        &cfg,
        &metas,
    )?;
    assert_eq!(parsed.state, "running");
    assert_eq!(parsed.cpus, Some(2));
    assert_eq!(parsed.current, Some(true));
    Ok(())
}

#[test]
fn artifacts_viewer_layer_roundtrips_and_rejects_invalid_locators() -> TestResult {
    let item = ViewerItem::file(ViewerFileInfo {
        id: "dl-1".into(),
        name: "report.pdf".into(),
        size: 4096,
    });
    let layer = viewer_layer(&item)?;
    assert_eq!(layer.kind, "viewer");
    assert_eq!(layer.name, "artifacts");
    let decoded = viewer_item(Some(&layer)).ok_or("expected ViewerItem")?;
    assert_eq!(decoded, item);

    let wrong_kind = RouteLayer {
        kind: "panel".into(),
        name: "artifacts".into(),
        locator: layer.locator.clone(),
    };
    assert!(viewer_item(Some(&wrong_kind)).is_none());

    let bad_locator = RouteLayer {
        kind: "viewer".into(),
        name: "artifacts".into(),
        locator: Some(
            "viewer:{\"kind\":\"file\",\"file\":{\"id\":\"\",\"name\":\"x\",\"size\":1}}".into(),
        ),
    };
    assert!(viewer_item(Some(&bad_locator)).is_none());
    Ok(())
}

// ---------------------------------------------------------------------------
// Item 9: SQLite Telemetry & 7 Operational CLI Subcommands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telemetry_sqlite_ingests_worker_batches_and_respects_disable_toggle() -> TestResult {
    let dir_off = tempfile::tempdir()?;
    std::fs::write(
        dir_off.path().join("settings.json"),
        serde_json::to_vec(&json!({ "telemetry": false }))?,
    )?;
    assert!(!telemetry_enabled(dir_off.path(), None));
    assert!(telemetry_enabled(dir_off.path(), Some(true)));

    let server_off = start_all_builtins_server(dir_off.path(), None).await?;
    assert!(
        !dir_off.path().join("telemetry.sqlite").exists(),
        "telemetry.sqlite must not be created when settings.telemetry=false"
    );
    server_off.close().await?;

    let dir_on = tempfile::tempdir()?;
    let server_on = start_all_builtins_server(dir_on.path(), Some(true)).await?;
    assert!(
        dir_on.path().join("telemetry.sqlite").exists(),
        "telemetry.sqlite must be opened when telemetry is enabled"
    );

    // Exercise live `chord.telemetry.worker` ingest contract + `SqliteTelemetryBackend` query.
    let ingest = Arc::new(pi_rs_p2::plugins::telemetry::worker_ingest_contract()?);
    let mut worker_caller = owner_caller();
    worker_caller.principal =
        Some(json!({ "kind": "worker", "session": "s-tel", "generation": 1 }));
    let held = server_on.host.hold_as(
        ingest,
        json!({}).into(),
        worker_caller,
        &Context::background(),
    )?;
    held.wait(&Context::background()).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as f64;
    let ingest_res: Value = held
        .call(
            "ingest",
            json!({
                "spans": [{
                    "id": "0123456789abcdef",
                    "traceId": "0123456789abcdef0123456789abcdef",
                    "name": "worker.turn",
                    "startedMs": now_ms,
                    "endedMs": now_ms + 45.0,
                    "attributes": { "chord.host": "worker:s-tel" },
                    "events": [],
                    "links": [],
                    "status": { "status": "ok" }
                }],
                "metrics": []
            })
            .into(),
            &Context::background(),
        )
        .await?
        .to_serde()?;
    assert_eq!(ingest_res["written"], json!(1));
    held.release();

    let backend =
        service_of::<Arc<dyn TelemetryBackend>>(&server_on.host, TELEMETRY_STORE_SERVICE)?;
    let tel_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let list = loop {
        let traces = backend.find_traces(TraceQuery::default()).await?;
        let arr = traces["traces"].as_array().cloned().unwrap_or_default();
        if arr
            .iter()
            .any(|t| t["traceId"] == "0123456789abcdef0123456789abcdef")
        {
            break arr;
        }
        assert!(
            tokio::time::Instant::now() < tel_deadline,
            "ingested worker span did not flush to telemetry.sqlite: {arr:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let _ = list;
    let detail = backend
        .get_trace("0123456789abcdef0123456789abcdef".into())
        .await?;
    assert_eq!(detail["spans"][0]["name"], "worker.turn");
    let metrics = backend.find_metrics(MetricQuery::default()).await?;
    assert!(metrics["metrics"].is_array());
    server_on.close().await?;

    // Check `SqliteTelemetryBackend` directly against a closed store as well.
    let tel_db = SqliteDatabase::open(
        dir_on.path().join("telemetry.sqlite"),
        SqliteOptions::default(),
    )
    .await?;
    let store = SqliteTelemetry::open(tel_db.clone(), 86_400_000.0, 1000).await?;
    let direct_backend = SqliteTelemetryBackend(store.clone());
    let direct_trace = direct_backend
        .get_trace("0123456789abcdef0123456789abcdef".into())
        .await?;
    assert_eq!(direct_trace["traceId"], "0123456789abcdef0123456789abcdef");
    store.close().await?;
    tel_db.close().await?;
    Ok(())
}

#[tokio::test]
async fn cli_subcommands_cover_traces_data_report_owner_link_migrate_reload_and_verify()
-> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start_all_builtins_server(dir.path(), Some(true)).await?;
    let context = Context::background();
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call("create", json!({ "cwd": dir.path() }).into(), &context)
        .await?
        .to_serde()?;
    let session_id = created["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();
    sessions.release();

    let ingest = Arc::new(pi_rs_p2::plugins::telemetry::worker_ingest_contract()?);
    let mut worker_caller = owner_caller();
    worker_caller.principal =
        Some(json!({ "kind": "worker", "session": session_id, "generation": 1 }));
    let tel_held = server
        .host
        .hold_as(ingest, json!({}).into(), worker_caller, &context)?;
    tel_held.wait(&context).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as f64;
    tel_held
        .call(
            "ingest",
            json!({
                "spans": [{
                    "id": "fedcba9876543210",
                    "traceId": "fedcba9876543210fedcba9876543210",
                    "name": "cli.trace",
                    "startedMs": now_ms,
                    "endedMs": now_ms + 25.0,
                    "attributes": {},
                    "events": [],
                    "links": [],
                    "status": { "status": "ok" }
                }],
                "metrics": []
            })
            .into(),
            &context,
        )
        .await?;
    tel_held.release();

    let url = format!("http://{}", server.address);
    let report_live = data_report(dir.path()).await?;
    let errs = verify_sessions(&url, &report_live, dir.path()).await?;
    assert!(errs.is_empty(), "verify_sessions failed: {errs:?}");

    let verify_out = verify_sessions_command(&[
        "--data".into(),
        dir.path().display().to_string(),
        url.clone(),
    ])
    .await?;
    assert!(
        verify_out.0,
        "verify_sessions_command failed: {}",
        verify_out.1
    );

    assert!(
        verify_sessions("https://example.com", &report_live, dir.path())
            .await
            .is_err()
    );

    let loopback: url::Url = url.parse()?;
    let public_origin = parse_owner_origin("https://p2.example.test")?;
    let link = mint_owner_link(&loopback, dir.path(), &public_origin).await?;
    assert!(link.as_str().starts_with("https://p2.example.test/owner#"));
    assert!(link.fragment().is_some_and(|f| !f.is_empty()));

    assert!(
        reload_command(&["--restart".into(), "files".into()])
            .await
            .is_err()
    );

    server.close().await?;

    let report = data_report(dir.path()).await?;
    assert!(report.sessions.contains_key(&session_id));
    let report_path = dir.path().join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    let (same, msg) = data_report_command(&[
        "--data".into(),
        dir.path().display().to_string(),
        "--compare".into(),
        report_path.display().to_string(),
    ])
    .await?;
    assert!(same, "expected identical --compare: {msg}");
    assert_eq!(
        compare_reports(&json!({"a": 1}), &json!({"a": 2}), "").len(),
        1
    );

    let trace_out = traces_command(&[
        "--data".into(),
        dir.path().display().to_string(),
        "--limit".into(),
        "5".into(),
    ])
    .await?;
    assert!(
        trace_out.contains("cli.trace"),
        "expected cli.trace in traces output: {trace_out}"
    );

    let mig_dir = tempfile::tempdir()?;
    std::fs::write(
        mig_dir.path().join("settings.json"),
        serde_json::to_vec(&json!({
            "defaultThinkingLevel": "medium"
        }))?,
    )?;
    let dry = migrate_data(mig_dir.path(), true).await?;
    assert!(dry.sessions.failed.is_empty());
    let applied = migrate_data(mig_dir.path(), false).await?;
    assert!(applied.sessions.failed.is_empty());
    let (ok_cmd, _) = migrate_data_command(&[
        "--data".into(),
        mig_dir.path().display().to_string(),
        "--dry-run".into(),
    ])
    .await?;
    assert!(ok_cmd);

    Ok(())
}
