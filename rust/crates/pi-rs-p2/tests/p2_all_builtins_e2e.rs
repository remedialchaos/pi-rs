//! End-to-end verification of the native `p2` server and worker with all 49 built-in plugins
//! registered, Phase 3 cross-shard service seams wired, live HTTP endpoints, real `p2 --worker`
//! chat turn execution, and persistence across server restart.

use std::{collections::BTreeSet, error::Error, path::Path, sync::Arc, time::Duration};

use pi_rs_chord::{context::Context, host::Caller, node::workers::WorkerOptions};
use pi_rs_p2::{
    api::v1::{PluginRegistry, p2_contracts::lookup, server::service_of},
    hosts::server::{P2Server, ServerOptions, start_server},
    plugins,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

/// All 48 facet-bearing built-in plugins under `apps/p2/plugins/` (`sync` is a shared library
/// module without server/worker facets, imported directly by `radius` and `backup`).
const EXPECTED_FACET_PLUGINS: &[&str] = &[
    "agent",
    "agent-plugins",
    "artifacts",
    "backup",
    "cache-warming",
    "chat",
    "chat-mode",
    "codemode",
    "coding-tools",
    "conversations",
    "dev-tools",
    "devices",
    "discord",
    "drafts",
    "envs",
    "familiar",
    "file-index",
    "files",
    "fit-images",
    "history",
    "locations",
    "login",
    "logs",
    "mcp",
    "models",
    "ports",
    "powershell",
    "projects",
    "prompt",
    "proxy-marker",
    "quiet-tools",
    "radius",
    "rollups",
    "scope-prompt",
    "search",
    "send-file",
    "sessions",
    "settings",
    "sharing",
    "status",
    "subagents",
    "telemetry",
    "terminals",
    "tree",
    "views",
    "voice",
    "web-shell",
    "workspace",
];

#[test]
fn all_48_facet_plugins_are_registered_without_duplicates() -> TestResult {
    let mut registry = PluginRegistry::new();
    plugins::register_builtins(&mut registry)?;
    let selected: BTreeSet<String> = registry.select(None)?.into_iter().collect();
    let expected: BTreeSet<String> = EXPECTED_FACET_PLUGINS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(expected.len(), 48);
    assert_eq!(
        selected, expected,
        "registered built-in plugin set must match all 48 facet-bearing upstream plugins"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_server_with_all_builtins_runs_http_services_chat_turn_and_restart_persistence()
-> TestResult {
    tokio::time::timeout(Duration::from_secs(60), run_full_e2e()).await?
}

async fn run_full_e2e() -> TestResult {
    let dir = tempfile::tempdir()?;
    let context = Context::background();
    let server = start_all_builtins_server(dir.path()).await?;

    // 1. All 48 facet-bearing built-in plugins selected and activated cleanly on live P2Server.
    assert_eq!(server.plugins.len(), 48);

    // 2. Live HTTP endpoints: /health returns 200 OK JSON; unauthenticated / returns 401 sign-in.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let origin = format!("http://{}", server.address);
    let health = client
        .get(format!("{origin}/health"))
        .header(reqwest::header::ORIGIN, &origin)
        .send()
        .await?;
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let health_json: Value = health.json().await?;
    assert_eq!(health_json, json!({"ok": true, "name": "p2"}));

    let unauth = client.get(format!("{origin}/")).send().await?;
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 3. Cross-shard Phase 3 service seams (no mocks):
    // 3a. settings -> models SettingsSaver persists settings.json atomically.
    let settings = server.host.hold_as(
        Arc::new(lookup("p2.settings")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    settings.wait(&context).await?;
    settings
        .call(
            "update",
            json!({"set": {"theme": "nord"}, "unset": []}).into(),
            &context,
        )
        .await?;
    let persisted_settings: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json"))?)?;
    assert_eq!(persisted_settings["theme"], json!("nord"));
    settings.release();

    // 3b. ports -> radius FORWARD_RESOLVER resolves live port forwards.
    let ports = server.host.hold_as(
        Arc::new(lookup("p2.ports")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    ports.wait(&context).await?;
    ports
        .call(
            "add",
            json!({"port": 5173, "name": "dev", "public": true}).into(),
            &context,
        )
        .await?;
    let forwards: Value = ports.read("forwards")?.to_serde()?;
    let forward_id = forwards[0]["id"]
        .as_str()
        .ok_or("missing forward id")?
        .to_owned();
    let resolver = service_of::<plugins::radius::ResolveForward>(
        &server.host,
        plugins::radius::FORWARD_RESOLVER,
    )?;
    let resolved = resolver(&forward_id).ok_or("FORWARD_RESOLVER did not find added forward")?;
    assert_eq!(resolved.port, 5173);
    assert!(resolved.public);
    ports.release();

    // 3c. telemetry -> InMemoryTelemetryBackend answers live trace queries via p2.traces.
    let telemetry = server.host.hold_as(
        Arc::new(lookup("p2.traces")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    telemetry.wait(&context).await?;
    let traces: Value = telemetry
        .call("findTraces", json!({}).into(), &context)
        .await?
        .to_serde()?;
    assert!(traces["traces"].is_array());
    telemetry.release();

    // 3d. sessions -> real p2 --worker child process -> real faux chat turn.
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call(
            "create",
            json!({
                "cwd": dir.path(),
                "title": "All 49 E2E",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })
            .into(),
            &context,
        )
        .await?
        .to_serde()?;
    let session_id = created["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();

    let chat = server
        .workers
        .hold(
            Arc::new(lookup("p2.chat")?),
            json!({"session": session_id, "conversation": 1}).into(),
            owner_caller(),
            &context,
        )
        .await?;
    chat.call(
        "send",
        json!({
            "text": "hello from all-49 e2e",
            "requestId": uuid::Uuid::new_v4().to_string(),
        })
        .into(),
        &context,
    )
    .await?;

    // Wait for the real faux model assistant turn to finish and record an assistant entry.
    wait_for_assistant_entry(&chat, &context).await?;
    drop(chat);
    sessions.release();

    // 4. Close and restart a brand-new P2Server over the exact same P2_DATA_DIR; verify persistence.
    server.close().await?;

    let restarted = start_all_builtins_server(dir.path()).await?;
    let sessions2 = restarted.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions2.wait(&context).await?;
    let list: Value = wait_ready_state(&sessions2, "sessions").await?;
    assert!(
        list.as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == session_id)),
        "restarted server must preserve session {session_id}: {list:?}"
    );
    let chat2 = restarted
        .workers
        .hold(
            Arc::new(lookup("p2.chat")?),
            json!({"session": session_id, "conversation": 1}).into(),
            owner_caller(),
            &context,
        )
        .await?;
    let persisted: Value = chat2
        .call("entries", json!({"limit": 20}).into(), &context)
        .await?
        .to_serde()?;
    let items = persisted["items"]
        .as_array()
        .ok_or("missing persisted items")?;
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "p2.input"
                && item["data"]["text"] == "hello from all-49 e2e"),
        "user prompt missing after restart: {items:?}"
    );
    assert!(
        items.iter().any(|item| item["kind"] == "pi.assistant"),
        "assistant reply missing after restart: {items:?}"
    );
    drop(chat2);
    sessions2.release();
    restarted.close().await?;
    Ok(())
}

async fn wait_ready_state(held: &pi_rs_chord::host::Held, member: &str) -> TestResult<Value> {
    for _ in 0..100 {
        match held.read(member) {
            Ok(value) => return Ok(value.to_serde()?),
            Err(err) if err.to_string().contains("not_ready") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(format!("timed out waiting for {member} state to populate").into())
}

async fn wait_for_assistant_entry(
    chat: &pi_rs_chord::remote::RemoteReference,
    context: &Context,
) -> TestResult {
    for _ in 0..100 {
        let entries: Value = chat
            .call("entries", json!({"limit": 20}).into(), context)
            .await?
            .to_serde()?;
        if entries["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["kind"] == "pi.assistant"))
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("timed out waiting for assistant reply in all-49 E2E".into())
}

fn owner_caller() -> Caller {
    Caller {
        principal: Some(json!({"kind": "owner", "access": "local"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

async fn start_all_builtins_server(directory: &Path) -> TestResult<P2Server> {
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
