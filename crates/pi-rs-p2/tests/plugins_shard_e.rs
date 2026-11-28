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
#[tokio::test]
async fn backup_chunks_are_store_scoped_and_aborts_leave_no_slot() -> TestResult {
    use pi_rs_chord::sync::{FileSystemSyncRemote, PutBatchResult};
    use pi_rs_p2::plugins::backup::{Begun, CHUNK_BYTES, Uploads};

    let fixture = tempfile::tempdir()?;
    let remote = Arc::new(FileSystemSyncRemote::new(fixture.path()));
    let uploads = Uploads::new(remote.clone(), "device".into());
    let Begun::Upload(aborted) = uploads.begin("session-first", 0, 1, false).await? else {
        return Err("expected a new upload".into());
    };
    assert!(
        uploads
            .append("session-second", &aborted, vec![9])
            .await
            .is_err()
    );
    assert!(uploads.finish("session-second", &aborted).await.is_err());
    uploads.abort("session-second", &aborted).await;
    assert!(
        uploads
            .append("session-first", &aborted, vec![0; CHUNK_BYTES + 1])
            .await
            .is_err()
    );
    uploads
        .append("session-first", &aborted, vec![1, 2])
        .await?;
    uploads.abort("session-first", &aborted).await;
    assert!(uploads.finish("session-first", &aborted).await.is_err());
    assert!(remote.get_batch("session-first", 0)?.is_none());

    let Begun::Upload(upload) = uploads.begin("session-first", 0, 1, true).await? else {
        return Err("expected an upload after abort".into());
    };
    let payload = vec![0xa5; CHUNK_BYTES];
    uploads
        .append("session-first", &upload, payload.clone())
        .await?;
    uploads
        .append("session-first", &upload, vec![0xf0, 0x0d])
        .await?;
    assert_eq!(
        uploads.finish("session-first", &upload).await?,
        PutBatchResult::Created
    );
    let mut restored = Vec::new();
    std::io::Read::read_to_end(
        &mut remote
            .get_batch("session-first", 0)?
            .ok_or("missing backup")?,
        &mut restored,
    )?;
    assert_eq!(&restored[..CHUNK_BYTES], payload);
    assert_eq!(&restored[CHUNK_BYTES..], &[0xf0, 0x0d]);
    assert_eq!(remote.list("session-first")?.batches[0].archive, Some(true));
    let Begun::Result(result) = uploads.begin("session-first", 0, 1, false).await? else {
        return Err("a committed slot requested another body".into());
    };
    assert_eq!(
        result,
        PutBatchResult::Exists {
            to: 1,
            device: "device".into()
        }
    );
    Ok(())
}

#[tokio::test]
async fn backup_bookmark_and_remote_restore_cover_updates_after_an_archive() -> TestResult {
    use pi_rs_chord::{
        sqlite::SqliteDatabase,
        store::{ColumnRecord, ColumnType, Query, StoreEngine, TableDefinition},
        sync::FileSystemSyncRemote,
    };
    use pi_rs_p2::plugins::backup::BackupStore;

    let fixture = tempfile::tempdir()?;
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    let remote = Arc::new(FileSystemSyncRemote::new(fixture.path().join("remote")));
    let origin =
        StoreEngine::open(SqliteDatabase::open(":memory:", Default::default()).await?).await?;
    let table = TableDefinition::new(
        "backup_fixture",
        1,
        indexmap::IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Str))]),
        vec!["id".into()],
        indexmap::IndexMap::new(),
    )?;
    origin.commit(table.declaration("fixture")?).await?;
    let backup = BackupStore::open(
        origin.clone(),
        "server".into(),
        &source,
        "target",
        remote.clone(),
        "device".into(),
    )
    .await?;
    backup.run(|_| {}).await?;
    origin
        .commit(vec![table.put("fixture", json!({"id": "expected"}))?])
        .await?;
    drop(backup);
    let reopened = BackupStore::open(
        origin,
        "server".into(),
        &source,
        "target",
        remote.clone(),
        "device".into(),
    )
    .await?;
    reopened.run(|_| {}).await?;
    let history = remote.list("server")?;
    assert_eq!(history.batches.len(), 2);
    assert!(
        history
            .batches
            .iter()
            .any(|batch| batch.after == 0 && batch.archive == Some(true))
    );
    let empty = SqliteDatabase::open(":memory:", Default::default()).await?;
    let restored = BackupStore::restore(
        remote,
        "device".into(),
        "server",
        &destination,
        "target",
        empty,
    )
    .await?;
    assert_eq!(restored.meta().await?.last_seq, 2);
    assert_eq!(restored.count(Query::new("fixture", table)?).await?, 1);
    Ok(())
}
#[tokio::test]
async fn backup_worker_streams_its_store_over_the_authenticated_server_link() -> TestResult {
    use pi_rs_chord::{
        host::{PluginFacet, define_facet},
        link::memory_link_pair,
        remote::{RemoteOptions, RemotePeer},
        sqlite::SqliteDatabase,
        sync::FileSystemSyncRemote,
    };
    use pi_rs_p2::{
        api::v1::p2_contracts::lookup,
        hosts::server::{ServerOptions, start_server},
        plugins::backup::{self, BACKUP_UPLOADS, BackupService, BackupStore},
        runtime::server::{install_service, service_of},
    };
    use std::{path::PathBuf, sync::Mutex};

    const FIXTURE: &str = "PI_RS_SHARD_E_BACKUP_FIXTURE";
    let Some(root) = std::env::var_os(FIXTURE).map(PathBuf::from) else {
        let fixture = tempfile::tempdir()?;
        let executable = std::env::current_exe()?;
        let directory = fixture.path();
        let output = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::process::Command::new(executable)
                .args([
                    "--exact",
                    "backup_worker_streams_its_store_over_the_authenticated_server_link",
                    "--nocapture",
                ])
                .env(FIXTURE, directory)
                .env("P2_BACKUP_DIR", directory.join("remote"))
                .env_remove("P2_BACKUP")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    };

    let mut options = ServerOptions::new(root.join("p2"));
    options.port = 0;
    options.fake = true;
    options.dev = false;
    backup::register(&mut options.registry)?;
    let registry = options.registry.clone();
    let server = start_server(options).await?;
    let context = Context::background();
    let backup_local = server
        .host
        .get(Arc::new(lookup("p2.backup")?), json!({}).into(), &context)
        .await?;
    let service = service_of::<BackupService>(&server.host, BACKUP_UPLOADS)?;
    service.run().await?;

    let (server_link, worker_link) = memory_link_pair();
    let server_options =
        RemoteOptions::new(json!({"kind":"worker","session":"sample","generation":1}));
    let serving = server.host.accept(server_link, server_options)?;
    let connection = Arc::new(Mutex::new(None::<Arc<RemotePeer>>));
    let (session, _parent) = handshake(&server.data_dir).await?;
    let fake = faux_provider(Default::default());
    let models = Models::default();
    models.set_provider(fake.provider.clone());
    let retained = connection.clone();
    let worker = start_session_worker_configured(
        session,
        &registry,
        Some(&["backup".into()]),
        models,
        move |_| {
            Ok(vec![PluginFacet::new(
                "backup-fixture-peer",
                define_facet(move |facet| {
                    let options = RemoteOptions::new(json!({"kind":"server"}));
                    let host = facet.host();
                    let peer = Arc::new(host.connect(worker_link.clone(), options)?);
                    install_service(&host, "p2.server-peer", &peer)?;
                    *retained.lock().unwrap_or_else(|error| error.into_inner()) = Some(peer);
                    Ok(())
                }),
            )])
        },
    )
    .await?;
    let parts = worker.process.harness()?;
    let conversation = parts.durable.root(&context).await?;
    fake.set_responses([faux_assistant_message(vec![faux_text("Back me up")]).into()]);
    let submission = conversation
        .submit(
            serde_json::from_value(json!({"type":"input","content":"Please answer"}))?,
            &context,
        )
        .await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), submission.wait(&context))
            .await??
            .status,
        SubmissionStatus::Done
    );
    let expected_head = parts.store.meta().await?.last_seq;
    let remote = Arc::new(FileSystemSyncRemote::new(root.join("remote")));
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let statuses = backup_local
                .member("statuses")?
                .value()?
                .to_json()
                .to_serde()?;
            if statuses["session-sample"]["state"] == "synced"
                && statuses["session-sample"]["syncedHead"]
                    .as_i64()
                    .is_some_and(|head| head >= expected_head)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, pi_rs_chord::Error>(())
    })
    .await??;
    let listing = remote.list("session-sample")?;
    let uploaded_head = pi_rs_chord::sync::remote_head(&listing)?;
    assert!(uploaded_head >= u64::try_from(expected_head)?);
    let device = backup_local
        .member("device")?
        .value()?
        .to_json()
        .to_serde()?;
    let restored = BackupStore::restore(
        remote,
        device
            .as_str()
            .ok_or("missing server backup device")?
            .into(),
        "session-sample",
        &root.join("restored"),
        "fixture",
        SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    assert!(u64::try_from(restored.meta().await?.last_seq)? >= uploaded_head);

    // An owner on a different link cannot obtain the worker-only interface.
    let (server_link, owner_link) = memory_link_pair();
    let owner = server.host.accept(
        server_link,
        RemoteOptions::new(json!({"kind":"owner","access":"local"})),
    )?;
    let options = RemoteOptions::new(json!({"kind":"server"}));
    let client = RemotePeer::connect(owner_link, options, server.host.batch())?;
    let denied = client.hold(
        Arc::new(lookup("p2.backup-remote")?),
        json!({}).into(),
        &context,
    )?;
    let error = tokio::time::timeout(Duration::from_secs(5), denied.wait(&context)).await?;
    assert!(error.is_err());
    drop((owner, client, denied));
    worker.close().await?;
    drop((connection, serving));
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn backup_store_handles_account_pause_remote_empty_and_stopped_states() -> TestResult {
    use pi_rs_chord::{
        Error,
        data::ChordError,
        sqlite::SqliteDatabase,
        store::{ColumnRecord, ColumnType, Query, StoreEngine, TableDefinition},
        sync::FileSystemSyncRemote,
    };
    use pi_rs_p2::plugins::backup::{BackupStore, paused_while_off};

    let fixture = tempfile::tempdir()?;
    let store_dir = fixture.path().join("store");
    let first_remote_dir = fixture.path().join("remote-a");
    let second_remote_dir = fixture.path().join("remote-b");
    let table = TableDefinition::new(
        "items",
        1,
        indexmap::IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Str))]),
        vec!["id".into()],
        indexmap::IndexMap::new(),
    )?;
    let origin =
        StoreEngine::open(SqliteDatabase::open(":memory:", Default::default()).await?).await?;
    origin.commit(table.declaration("app")?).await?;
    origin
        .commit(vec![table.put("app", json!({"id": "one"}))?])
        .await?;

    // Before any backup, paused_while_off is None.
    assert!(paused_while_off(&store_dir).await?.is_none());

    let account_name = Arc::new(std::sync::Mutex::new(Some("org-1/user-a".to_owned())));
    let account_provider = {
        let account_name = account_name.clone();
        Arc::new(move || {
            let current = account_name
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            Box::pin(async move { Ok(current) }) as pi_rs_chord::sync::SyncFuture<Option<String>>
        })
    };

    let remote_a = Arc::new(FileSystemSyncRemote::new(&first_remote_dir));
    let backup = BackupStore::open(
        origin.clone(),
        "server".into(),
        &store_dir,
        "radius",
        remote_a,
        "dev-1".into(),
    )
    .await?
    .with_account(Some(account_provider.clone()));

    let statuses = std::sync::Mutex::new(Vec::new());
    let record = |s| {
        statuses.lock().unwrap_or_else(|e| e.into_inner()).push(s);
    };
    let take_statuses = || -> Vec<serde_json::Value> {
        std::mem::take(&mut *statuses.lock().unwrap_or_else(|e| e.into_inner()))
    };

    backup.run(&record).await?;
    let recorded = take_statuses();
    assert_eq!(
        recorded.last().and_then(|s| s["state"].as_str()),
        Some("synced")
    );
    assert_eq!(
        recorded.last().and_then(|s| s["syncedHead"].as_u64()),
        Some(2)
    );

    // While off, paused_while_off reports state=paused reason=off with syncedHead=2.
    let paused = paused_while_off(&store_dir)
        .await?
        .ok_or("expected paused status")?;
    assert_eq!(paused["state"], "paused");
    assert_eq!(paused["reason"], "off");
    assert_eq!(paused["syncedHead"], 2);

    // Switching to another signed-in account pauses without uploading.
    *account_name.lock().unwrap_or_else(|e| e.into_inner()) = Some("org-2/user-b".into());
    origin
        .commit(vec![table.put("app", json!({"id": "two"}))?])
        .await?;
    backup.run(&record).await?;
    let recorded = take_statuses();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["state"], "paused");
    assert_eq!(recorded[0]["reason"], "other_account");
    assert_eq!(recorded[0]["syncedHead"], 2);

    // Returning to the binding account recovers from an emptied remote by uploading an archive.
    *account_name.lock().unwrap_or_else(|e| e.into_inner()) = Some("org-1/user-a".into());
    let remote_b = Arc::new(FileSystemSyncRemote::new(&second_remote_dir));
    let recovered = BackupStore::open(
        origin.clone(),
        "server".into(),
        &store_dir,
        "radius",
        remote_b.clone(),
        "dev-1".into(),
    )
    .await?
    .with_account(Some(account_provider));
    recovered.run(&record).await?;
    let recorded = take_statuses();
    assert_eq!(
        recorded.last().and_then(|s| s["state"].as_str()),
        Some("synced")
    );
    assert_eq!(
        recorded.last().and_then(|s| s["syncedHead"].as_u64()),
        Some(3)
    );
    let restored = BackupStore::restore(
        remote_b,
        "dev-1".into(),
        "server",
        &fixture.path().join("restored"),
        "radius",
        SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    assert_eq!(restored.count(Query::new("app", table)?).await?, 2);

    // Terminal error puts backup into stopped/refused and prevents further retries.
    #[derive(Clone)]
    struct FailingRemote;
    impl pi_rs_chord::sync::AsyncRemote for FailingRemote {
        fn list(
            &self,
            _: String,
        ) -> pi_rs_chord::sync::SyncFuture<pi_rs_chord::sync::RemoteListing> {
            Box::pin(async { Err(ChordError::new("backup_terminal", "protocol too old").into()) })
        }
        fn get_batch(
            &self,
            _: String,
            _: u64,
        ) -> pi_rs_chord::sync::SyncFuture<Option<pi_rs_chord::sync::Body>> {
            Box::pin(async { Ok(None) })
        }
        fn get_snapshot(
            &self,
            _: String,
            _: u64,
        ) -> pi_rs_chord::sync::SyncFuture<Option<pi_rs_chord::sync::Body>> {
            Box::pin(async { Ok(None) })
        }
        fn put_batch(
            &self,
            _: String,
            _: pi_rs_chord::sync::BatchInfo,
            _: pi_rs_chord::sync::Body,
        ) -> pi_rs_chord::sync::SyncFuture<pi_rs_chord::sync::PutBatchResult> {
            Box::pin(async { Err(ChordError::new("backup_terminal", "protocol too old").into()) })
        }
    }
    let refused_store = BackupStore::open(
        origin,
        "server".into(),
        &store_dir,
        "other-target",
        Arc::new(FailingRemote),
        "dev-1".into(),
    )
    .await?
    .with_terminal(Some(Arc::new(
        |err: &Error| matches!(err, Error::Chord(c) if c.code == "backup_terminal"),
    )));
    refused_store.run(&record).await?;
    let recorded = take_statuses();
    assert_eq!(
        recorded.last().and_then(|s| s["state"].as_str()),
        Some("stopped")
    );
    assert_eq!(
        recorded.last().and_then(|s| s["reason"].as_str()),
        Some("refused")
    );
    // Second run is a no-op once stopped.
    refused_store.run(&record).await?;
    assert!(take_statuses().is_empty());
    Ok(())
}
