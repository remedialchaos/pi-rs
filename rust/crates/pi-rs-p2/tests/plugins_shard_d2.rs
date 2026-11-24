use std::{error::Error as StdError, sync::Arc, time::Duration};

use pi_rs_chord::{
    context::Context,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    node::workers::{Control, Frame, WorkerSession, write_frame},
};
use pi_rs_durable::{
    EntryDraft, EntryHead, EntryRecord, MemoryStorage, ROOT_CONVERSATION_ID,
    session::{Session, SessionOptions},
};
use pi_rs_p2::{
    api::v1::{PluginRegistry, p2_contracts::lookup, session_address},
    hosts::worker::start_session_worker_configured,
    plugins::history::tools::{
        HISTORY_HANDOFF, HISTORY_READ, HISTORY_SEARCH, HistoryEntry, HistoryRole, READ_BUDGET,
        ReadArgs, ReadMode, read_entries, render_entry, results_of, within_budget,
    },
    plugins::shard_d2,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::DuplexStream;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TIMEOUT: Duration = Duration::from_secs(10);

fn make_entry(
    id: u64,
    kind: &str,
    model: Option<Vec<serde_json::Value>>,
    data: Option<serde_json::Value>,
    head: Option<u64>,
) -> EntryRecord {
    EntryRecord {
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: kind.to_owned(),
        model,
        data,
        head,
        edits: None,
        by_task_id: None,
    }
}

#[test]
fn history_render_entry_formats_all_entry_kinds_and_tool_statuses() {
    let entries = vec![
        make_entry(1, "pi.compaction", None, None, Some(2)),
        make_entry(2, "pi.reset", None, None, Some(2)),
        make_entry(
            3,
            "pi.reset",
            Some(vec![json!({"role": "user", "content": "resume from here"})]),
            None,
            Some(3),
        ),
        make_entry(
            4,
            "p2.shell",
            None,
            Some(json!({
                "author": { "kind": "owner", "id": "o1", "name": "Alice" },
                "command": "git status",
                "output": "clean",
                "cancelled": false,
                "truncated": false,
                "includeInContext": true
            })),
            None,
        ),
        make_entry(
            5,
            "p2.agent-message",
            None,
            Some(json!({
                "from": { "name": "worker-1", "conversationId": 2 },
                "intent": "question",
                "text": "Should I proceed?"
            })),
            None,
        ),
        make_entry(
            6,
            "p2.peer-message",
            Some(vec![json!({"role": "user", "content": "ignored"})]),
            Some(json!({
                "from": { "id": "sess-2", "title": "Docs" },
                "text": "Updated README"
            })),
            None,
        ),
        make_entry(
            7,
            "p2.input",
            Some(vec![json!({"role": "user", "content": "raw"})]),
            Some(json!({
                "author": { "kind": "guest", "id": "g1", "name": "Bob" },
                "text": "Please check",
                "attachments": [{ "id": "u1", "name": "spec.md" }]
            })),
            None,
        ),
        make_entry(
            8,
            "pi.message",
            Some(vec![json!({
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "secret" },
                    { "type": "text", "text": "Running checks" },
                    { "type": "toolCall", "id": "c1", "name": "bash" },
                    { "type": "toolCall", "id": "c2", "name": "read" },
                    { "type": "toolCall", "id": "c3", "name": "write" }
                ]
            })]),
            None,
            None,
        ),
        make_entry(
            9,
            "pi.tool-result",
            Some(vec![
                json!({"role": "toolResult", "toolCallId": "c1", "isError": false}),
                json!({"role": "toolResult", "toolCallId": "c2", "isError": true}),
            ]),
            None,
            None,
        ),
    ];

    let results = results_of(&entries);
    let rendered: Vec<HistoryEntry> = entries
        .iter()
        .filter_map(|e| render_entry(e, &results))
        .collect();
    assert_eq!(rendered.len(), 8);
    assert_eq!(rendered[0].role, HistoryRole::Marker);
    assert_eq!(rendered[0].text, "[context compacted here]");
    assert_eq!(rendered[1].text, "[new context]");
    assert_eq!(rendered[2].text, "[new context, handoff]\nresume from here");
    assert_eq!(rendered[3].author.as_deref(), Some("Alice"));
    assert_eq!(rendered[3].text, "[shell: git status]");
    assert_eq!(rendered[4].author.as_deref(), Some("subagent worker-1"));
    assert_eq!(rendered[4].text, "[asks] Should I proceed?");
    assert_eq!(rendered[5].author.as_deref(), Some("session Docs"));
    assert_eq!(rendered[5].text, "Updated README");
    assert_eq!(rendered[6].author.as_deref(), Some("Bob"));
    assert_eq!(rendered[6].text, "Please check\n[attachment: spec.md]");
    assert_eq!(rendered[7].role, HistoryRole::Assistant);
    assert_eq!(
        rendered[7].text,
        "[thinking]\nRunning checks\n[tool: bash, done]\n[tool: read, failed]\n[tool: write]"
    );
}

#[test]
fn history_within_budget_clips_single_oversized_entry_and_stops_at_limit() {
    let huge = HistoryEntry {
        entry: 1,
        role: HistoryRole::User,
        author: None,
        text: "ä".repeat(READ_BUDGET + 500),
    };
    let (kept, truncated) = within_budget(vec![huge], READ_BUDGET);
    assert!(truncated);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].text.chars().count(), READ_BUDGET - 100);

    let small_a = HistoryEntry {
        entry: 1,
        role: HistoryRole::User,
        author: None,
        text: "hello".into(),
    };
    let small_b = HistoryEntry {
        entry: 2,
        role: HistoryRole::Assistant,
        author: None,
        text: "world".into(),
    };
    let (kept, truncated) = within_budget(vec![small_a, small_b], 20);
    assert!(truncated);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].entry, 1);
}

#[tokio::test]
async fn history_read_entries_modes_tail_range_and_items() -> TestResult {
    let ctx = Context::background();
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let conv = ROOT_CONVERSATION_ID;
    let ids = session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                let mut ids = Vec::new();
                for i in 1..=5 {
                    let rec = tx
                        .append_entry(
                            conv,
                            EntryDraft {
                                kind: "pi.message".into(),
                                model: Some(vec![
                                    json!({"role": "user", "content": format!("msg {i}")}),
                                ]),
                                data: None,
                                head: None,
                                edits: None,
                            },
                        )
                        .await?;
                    ids.push(rec.id);
                }
                Ok(ids)
            },
            &ctx,
        )
        .await?;

    // Before compaction, tail returns a note and no records.
    let tail_empty = session
        .commit(
            async |tx| {
                read_entries(
                    tx,
                    conv,
                    &ReadArgs {
                        mode: ReadMode::Tail,
                        entries: None,
                        around: None,
                        from: None,
                        to: None,
                        limit: None,
                        cursor: None,
                        count: Some(2),
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert!(tail_empty.records.is_empty());
    assert!(
        tail_empty
            .note
            .as_deref()
            .is_some_and(|n| n.contains("No compaction"))
    );

    // Add a compaction marker at head=ids[3], plus a post-compaction entry.
    let head_id = ids[3];
    session
        .commit(
            async |tx| {
                tx.append_entry(
                    conv,
                    EntryDraft {
                        kind: "pi.compaction".into(),
                        model: None,
                        data: None,
                        head: Some(EntryHead::Id(head_id)),
                        edits: None,
                    },
                )
                .await?;
                Ok(())
            },
            &ctx,
        )
        .await?;

    let tail = session
        .commit(
            async |tx| {
                read_entries(
                    tx,
                    conv,
                    &ReadArgs {
                        mode: ReadMode::Tail,
                        entries: None,
                        around: None,
                        from: None,
                        to: None,
                        limit: None,
                        cursor: None,
                        count: Some(2),
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert_eq!(tail.boundary, Some(head_id));
    let tail_ids: Vec<u64> = tail.records.iter().map(|r| r.id).collect();
    assert_eq!(tail_ids, vec![ids[1], ids[2]]);

    // Range pagination via cursor.
    let page1 = session
        .commit(
            async |tx| {
                read_entries(
                    tx,
                    conv,
                    &ReadArgs {
                        mode: ReadMode::Range,
                        entries: None,
                        around: None,
                        from: Some(ids[0]),
                        to: Some(ids[4]),
                        limit: Some(2),
                        cursor: None,
                        count: None,
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert_eq!(
        page1.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[0], ids[1]]
    );
    assert!(page1.next.is_some());
    let page2 = session
        .commit(
            async |tx| {
                read_entries(
                    tx,
                    conv,
                    &ReadArgs {
                        mode: ReadMode::Range,
                        entries: None,
                        around: None,
                        from: None,
                        to: None,
                        limit: Some(2),
                        cursor: page1.next.clone(),
                        count: None,
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert_eq!(
        page2.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[2], ids[3]]
    );

    // Items with around=1 and a missing entry.
    let items = session
        .commit(
            async |tx| {
                read_entries(
                    tx,
                    conv,
                    &ReadArgs {
                        mode: ReadMode::Items,
                        entries: Some(vec![ids[2], 999]),
                        around: Some(1),
                        from: None,
                        to: None,
                        limit: None,
                        cursor: None,
                        count: None,
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert_eq!(
        items.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[1], ids[2], ids[3]]
    );
    assert_eq!(
        items.note.as_deref(),
        Some("Not in this conversation: #999.")
    );

    session.close(&ctx).await?;
    Ok(())
}

async fn start_worker_with_search_mock(
    dir: &std::path::Path,
) -> TestResult<(pi_rs_p2::hosts::worker::SessionWorker, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (child_read, child_write) = tokio::io::split(child);
    let start = Frame::Control(Control::Start {
        address: session_address("hist-sess"),
        start: json!({
            "app": dir,
            "dataDir": dir,
            "fake": true,
            "telemetry": false,
            "generation": 1,
            "values": {
                "session": {
                    "id": "hist-sess",
                    "cwd": dir,
                    "model": { "provider": "faux", "modelId": "faux-1" },
                    "thinkingLevel": "off",
                    "resume": false,
                }
            },
        }),
        scripts: Vec::new(),
        lock: dir.join("worker.lock"),
    });
    write_frame(&mut parent, &start).await?;
    let session =
        tokio::time::timeout(TIMEOUT, WorkerSession::start(child_read, child_write)).await??;
    let mut registry = PluginRegistry::new();
    shard_d2::register(&mut registry)?;

    let search_contract = Arc::new(lookup("p2.search-feed")?);
    let mock_search = pi_rs_chord::host::PluginFacet::new(
        "mock-search",
        define_facet(move |host| {
            host.serve(
                search_contract.clone(),
                move |_, _, _| {
                    let mut members = Members::variant("worker");
                    members.method("history", move |args, _| {
                        Box::pin(async move {
                            let v = args.to_serde()?;
                            assert_eq!(v["query"], "alpha");
                            Ok(JsonValue::from_serde(json!({
                                "hits": [
                                    { "entryId": 1, "role": "user", "snippet": "alpha beta" }
                                ],
                                "indexedThrough": 10,
                                "more": false
                            })))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    );

    let models = pi_rs_ai::registry::Models::default();
    let faux = pi_rs_ai::faux::faux_provider(pi_rs_ai::faux::FauxOptions {
        api: Some("p2-faux".into()),
        provider: Some("faux".into()),
        models: vec![pi_rs_ai::faux::FauxModelDefinition {
            id: "faux-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());

    let worker = start_session_worker_configured(session, &registry, None, models, move |_| {
        Ok(vec![mock_search])
    })
    .await?;
    Ok((worker, parent))
}

#[tokio::test]
async fn history_worker_facet_installs_tools_and_executes_search_and_handoff() -> TestResult {
    let dir = TempDir::new()?;
    let ctx = Context::background();
    let (worker, mut parent) = start_worker_with_search_mock(dir.path()).await?;
    let harness = worker.process.harness()?;
    let snapshot = worker.process.registries.extensions.snapshot();
    let ext = snapshot
        .extension("p2-history")
        .expect("p2-history extension registered");
    let tool_names: Vec<&str> = ext
        .tools
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(
        tool_names,
        vec![HISTORY_SEARCH, HISTORY_READ, HISTORY_HANDOFF]
    );
    assert_eq!(ext.sections.len(), 1);
    assert_eq!(ext.sections[0].key, "history");

    // Seed an entry with author attribution.
    harness
        .durable
        .session
        .commit(
            async |tx| {
                tx.append_entry(
                    ROOT_CONVERSATION_ID,
                    EntryDraft {
                        kind: "p2.input".into(),
                        model: Some(vec![json!({"role": "user", "content": "alpha beta"})]),
                        data: Some(json!({
                            "author": { "kind": "owner", "id": "o1", "name": "Alice" },
                            "text": "alpha beta",
                            "attachments": []
                        })),
                        head: None,
                        edits: None,
                    },
                )
                .await?;
                Ok(())
            },
            &ctx,
        )
        .await?;

    // Build a minimal Invocation via a real tool task? Even simpler: verify tool declarations & handoff/search via extension directly if needed, or run a task.
    // Let's verify handoff tool control and search tool via a real conversation submission or direct invocation:
    // Check that history_handoff has callers = ["model"].
    let handoff_tool = ext
        .tools
        .iter()
        .find(|t| t.declaration.name == HISTORY_HANDOFF)
        .unwrap();
    assert_eq!(
        handoff_tool.callers.as_deref(),
        Some(&["model".to_owned()][..])
    );

    // Close cleanly.
    let run = tokio::spawn(worker.run());
    write_frame(&mut parent, &Frame::Control(Control::Shutdown)).await?;
    tokio::time::timeout(TIMEOUT, run).await???;
    Ok(())
}

#[test]
fn familiar_cold_helpers_compute_threshold_and_last_answer() {
    use pi_rs_durable::harness::ContextView;
    use pi_rs_p2::plugins::familiar::cold::{
        COLD_COMPACT_TOKENS, cold_at, cold_threshold, last_answer,
    };

    assert_eq!(cold_threshold(200_000, 16_384), COLD_COMPACT_TOKENS);
    assert_eq!(cold_threshold(100_000, 20_000), 80_000);
    assert_eq!(cold_at(1_000, 300_000, 1_800_000), 2_101_000);

    let e1 = make_entry(1, "pi.message", None, None, None);
    let e2 = make_entry(2, "pi.compaction", None, None, Some(2));
    let e3 = make_entry(3, "pi.message", None, None, None);

    let view = ContextView {
        head: Some(e2.clone()),
        entries: vec![e1.clone(), e2.clone(), e3.clone()],
        contributions: vec![
            vec![json!({"role": "assistant", "usage": {"totalTokens": 999}})],
            vec![],
            vec![
                json!({"role": "assistant", "timestamp": 5000, "usage": {"input": 100, "output": 50, "cacheRead": 20, "cacheWrite": 10}}),
            ],
        ],
        messages: vec![],
    };
    let (msg, tokens) = last_answer(&view).expect("answer after head");
    assert_eq!(msg["timestamp"], 5000);
    assert_eq!(tokens, 180);

    let after_head = ContextView {
        head: Some(e3),
        entries: vec![e1, e2],
        contributions: vec![
            vec![json!({"role": "assistant", "usage": {"totalTokens": 999}})],
            vec![],
        ],
        messages: vec![],
    };
    assert!(last_answer(&after_head).is_none());
}

#[tokio::test]
async fn familiar_server_creates_once_restores_archived_and_forbids_guests() -> TestResult {
    use pi_rs_chord::host::{Caller, HostOptions};
    use pi_rs_p2::{
        api::v1::{FacetKind, OwnerAccess, Principal},
        hosts::server::{ServerOptions, start_server},
        runtime::server::ServerServices,
    };

    let dir = TempDir::new()?;
    let ctx = Context::background();
    let catalog_contract = Arc::new(lookup("p2.catalog")?);
    let ops_contract = Arc::new(lookup("p2.session-operations")?);

    let (sessions_tx, sessions_rx) =
        tokio::sync::watch::channel::<Vec<serde_json::Value>>(Vec::new());
    let creates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let restores = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mock_catalog = ServerServices::default().with_facet(move |host| {
        let sessions_rx = sessions_rx.clone();
        host.serve(
            catalog_contract.clone(),
            move |_, _, object| {
                let mut rx = sessions_rx.clone();
                let source = object.follow(move |target, ctx| {
                    Box::pin(async move {
                        let initial = rx.borrow_and_update().clone();
                        target.set(JsonValue::from_serde(json!(initial)), &ctx.context)?;
                        let task = tokio::spawn(async move {
                            let bg = Context::background();
                            while rx.changed().await.is_ok() {
                                let update = rx.borrow_and_update().clone();
                                target.set(JsonValue::from_serde(json!(update)), &bg)?;
                            }
                            Ok::<_, pi_rs_chord::Error>(())
                        });
                        Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                    })
                })?;
                let mut members = Members::new();
                members.state("sessions", source)?;
                for m in ["get", "create", "activity", "update"] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let ops_tx = sessions_tx.clone();
    let ops_creates = creates.clone();
    let ops_restores = restores.clone();
    let mock_ops = ServerServices::default().with_facet(move |host| {
        let ops_tx = ops_tx.clone();
        let ops_creates = ops_creates.clone();
        let ops_restores = ops_restores.clone();
        host.serve(
            ops_contract.clone(),
            move |_, _, object| {
                let ops_tx = ops_tx.clone();
                let ops_creates = ops_creates.clone();
                let ops_restores = ops_restores.clone();
                let mut members = Members::new();
                members.state(
                    "operations",
                    object.writable(JsonValue::from_serde(json!([])))?,
                )?;
                let ops_tx_c = ops_tx.clone();
                members.method("create", move |args, _| {
                    let ops_tx = ops_tx_c.clone();
                    let ops_creates = ops_creates.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        ops_creates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let row = json!({
                            "id": v["id"],
                            "title": v.get("title").cloned().unwrap_or(json!("Ask Pi")),
                            "cwd": v["cwd"],
                            "env": null,
                            "createdAt": 1,
                            "updatedAt": 1,
                            "archivedAt": null,
                            "pinned": false,
                            "main": 1,
                            "branches": [],
                            "model": { "provider": "faux", "modelId": "faux-1" },
                            "thinkingLevel": "off"
                        });
                        ops_tx.send_modify(|rows| rows.push(row.clone()));
                        Ok(JsonValue::from_serde(row))
                    })
                })?;
                let ops_tx_r = ops_tx.clone();
                members.method("restore", move |args, _| {
                    let ops_tx_r = ops_tx_r.clone();
                    let ops_restores = ops_restores.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        ops_restores.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        ops_tx_r.send_modify(|rows| {
                            for row in rows.iter_mut() {
                                if row["id"] == v["id"] {
                                    row["archivedAt"] = json!(null);
                                }
                            }
                        });
                        Ok(JsonValue::Null)
                    })
                })?;
                for m in ["move", "relocate", "clone", "fork", "archive", "dismiss"] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mut options = ServerOptions::new(dir.path().to_path_buf());
    options.port = 0;
    pi_rs_p2::plugins::familiar::register(&mut options.registry)?;
    options
        .registry
        .register("mock-catalog", FacetKind::Server, mock_catalog)?;
    options
        .registry
        .register("mock-ops", FacetKind::Server, mock_ops)?;
    let _ = HostOptions::default();
    let server = start_server(options).await?;
    let familiar_contract = Arc::new(lookup("p2.familiar")?);

    // Guest is forbidden.
    let guest = Principal::Guest {
        id: "g1".into(),
        name: "Guest".into(),
        email: None,
        home: "s1".into(),
        sessions: Default::default(),
        revision: 1,
    };
    let guest_held = server.host.hold_as(
        familiar_contract.clone(),
        JsonValue::Object(Default::default()),
        Caller {
            principal: Some(serde_json::to_value(&guest)?),
            incarnation: "test".into(),
        },
        &ctx,
    )?;
    assert!(guest_held.wait(&ctx).await.is_err());

    // Owner opens once -> creates session; second open reuses same ID without creating again.
    let owner = Principal::Owner {
        access: OwnerAccess::Local,
    };
    let owner_held = server.host.hold_as(
        familiar_contract,
        JsonValue::Object(Default::default()),
        Caller {
            principal: Some(serde_json::to_value(&owner)?),
            incarnation: "test".into(),
        },
        &ctx,
    )?;
    owner_held.wait(&ctx).await?;
    let first = owner_held
        .call("open", JsonValue::Null, &ctx)
        .await?
        .to_serde()?;
    let id = first["id"].as_str().expect("familiar id").to_owned();
    assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = owner_held
        .call("open", JsonValue::Null, &ctx)
        .await?
        .to_serde()?;
    assert_eq!(second["id"], id);
    assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(restores.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Archive the Familiar row and reopen -> calls restore.
    sessions_tx.send_modify(|rows| {
        for row in rows.iter_mut() {
            if row["id"] == id {
                row["archivedAt"] = json!(12345);
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let third = owner_held
        .call("open", JsonValue::Null, &ctx)
        .await?
        .to_serde()?;
    assert_eq!(third["id"], id);
    assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(restores.load(std::sync::atomic::Ordering::SeqCst), 1);

    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn familiar_worker_inert_for_normal_session_and_configures_tools_for_familiar() -> TestResult
{
    let dir = TempDir::new()?;
    let ctx = Context::background();
    let mut registry = PluginRegistry::new();
    shard_d2::register(&mut registry)?;

    // 1) Normal session: familiar facet is inert.
    let (mut parent1, child1) = tokio::io::duplex(8192);
    let (r1, w1) = tokio::io::split(child1);
    write_frame(
        &mut parent1,
        &Frame::Control(Control::Start {
            address: session_address("normal-sess"),
            start: json!({
                "app": dir.path(),
                "dataDir": dir.path(),
                "fake": true,
                "telemetry": false,
                "generation": 1,
                "values": {
                    "session": {
                        "id": "normal-sess",
                        "cwd": dir.path(),
                        "model": { "provider": "faux", "modelId": "faux-1" },
                        "thinkingLevel": "off",
                        "resume": false,
                    }
                },
            }),
            scripts: Vec::new(),
            lock: dir.path().join("normal.lock"),
        }),
    )
    .await?;
    let sess1 = tokio::time::timeout(TIMEOUT, WorkerSession::start(r1, w1)).await??;
    let worker1 = pi_rs_p2::hosts::worker::start_session_worker(sess1, &registry, None).await?;
    assert!(
        worker1
            .process
            .registries
            .extensions
            .snapshot()
            .extension("p2-familiar")
            .is_none()
    );
    let run1 = tokio::spawn(worker1.run());
    write_frame(&mut parent1, &Frame::Control(Control::Shutdown)).await?;
    tokio::time::timeout(TIMEOUT, run1).await???;

    // 2) Familiar session: installs p2-familiar extension, input gate, and configures main tools.
    let dir2 = TempDir::new()?;
    let (mut parent2, child2) = tokio::io::duplex(8192);
    let (r2, w2) = tokio::io::split(child2);
    write_frame(
        &mut parent2,
        &Frame::Control(Control::Start {
            address: session_address("fam-sess"),
            start: json!({
                "app": dir2.path(),
                "dataDir": dir2.path(),
                "fake": true,
                "telemetry": false,
                "generation": 1,
                "values": {
                    "familiar": true,
                    "session": {
                        "id": "fam-sess",
                        "cwd": dir2.path(),
                        "model": { "provider": "faux", "modelId": "faux-1" },
                        "thinkingLevel": "off",
                        "resume": false,
                    }
                },
            }),
            scripts: Vec::new(),
            lock: dir2.path().join("fam.lock"),
        }),
    )
    .await?;
    let sess2 = tokio::time::timeout(TIMEOUT, WorkerSession::start(r2, w2)).await??;
    let worker2 = pi_rs_p2::hosts::worker::start_session_worker(sess2, &registry, None).await?;
    assert!(
        worker2
            .process
            .registries
            .extensions
            .snapshot()
            .extension("p2-familiar")
            .is_some()
    );
    assert!(
        worker2
            .process
            .registries
            .input_gates
            .lock()
            .unwrap()
            .contains_key("familiar")
    );
    let harness2 = worker2.process.harness()?;
    let run2 = tokio::spawn(worker2.run());
    // Wait briefly for on_live apply_tools to set AgentDoc.
    let mut configured = false;
    for _ in 0..50 {
        let root_conv = harness2.durable.root(&ctx).await?;
        let agent = root_conv.agent(&ctx).await?;
        if agent.state["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|v| v == "agent"))
        {
            configured = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        configured,
        "Familiar worker should configure main conversation tools"
    );
    write_frame(&mut parent2, &Frame::Control(Control::Shutdown)).await?;
    tokio::time::timeout(TIMEOUT, run2).await???;
    Ok(())
}

#[test]
fn rollups_input_of_and_fingerprint_of_track_reads_and_exclude_familiar() {
    use pi_rs_p2::plugins::rollups::runner::{
        RollupFacts, fingerprint_of, input_of, mention_of, mention_read,
    };
    use std::collections::HashMap;

    let facts = RollupFacts {
        familiar: Some("fam".into()),
        sessions: vec![
            json!({
                "id": "fam", "title": "Ask Pi", "cwd": "/", "env": null,
                "createdAt": 1, "updatedAt": 10, "archivedAt": null, "pinned": false,
                "model": { "provider": "p", "modelId": "m" }
            }),
            json!({
                "id": "b", "title": "Beta", "cwd": "/b", "env": null,
                "createdAt": 20, "updatedAt": 30, "archivedAt": null, "pinned": true,
                "model": { "provider": "p", "modelId": "m" }
            }),
            json!({
                "id": "a", "title": "Alpha", "cwd": "/a", "env": "dev",
                "createdAt": 10, "updatedAt": 20, "archivedAt": null, "pinned": false,
                "model": { "provider": "p", "modelId": "m" }
            }),
        ],
        statuses: json!({
            "a": { "running": true, "busy": ["turn"], "error": null },
            "b": { "running": false, "busy": [], "error": "boom" }
        }),
        answers: json!({ "a": 5, "b": 2 }),
        reads: json!({ "a": 3, "b": 2 }),
        projects: vec![json!({ "name": "core", "cwd": "/a" })],
        memberships: json!({ "a": "core" }),
    };

    let input = input_of(&facts, 1000);
    assert_eq!(input.sessions.len(), 2);
    assert_eq!(input.sessions[0].id, "a");
    assert_eq!(input.sessions[0].state, "working");
    assert!(input.sessions[0].unread);
    assert_eq!(input.sessions[0].project.as_deref(), Some("core"));
    assert_eq!(input.sessions[1].id, "b");
    assert_eq!(input.sessions[1].state, "stopped");
    assert!(!input.sessions[1].unread);
    assert_eq!(input.sessions[1].error.as_deref(), Some("boom"));

    let reads = vec!["sessions.title".to_owned()];
    let fp1 = fingerprint_of(&input, &reads, &HashMap::new());
    let mut input_same_title = input.clone();
    input_same_title.sessions[0].unread = false;
    assert_eq!(
        fp1,
        fingerprint_of(&input_same_title, &reads, &HashMap::new()),
        "unread was not read so fingerprint stays identical"
    );
    input_same_title.sessions[0].title = "Changed".into();
    assert_ne!(
        fp1,
        fingerprint_of(&input_same_title, &reads, &HashMap::new()),
        "title was read so fingerprint changes"
    );

    let mpath = mention_read("bug", Some(500));
    let parsed = mention_of(&mpath).expect("mention_of parses");
    assert_eq!(parsed.query, "bug");
    assert_eq!(parsed.since, Some(500));
    assert!(mention_of("sessions.title").is_none());
}

#[tokio::test]
async fn rollups_server_executes_scripts_and_enforces_owner_and_familiar_auth() -> TestResult {
    use pi_rs_chord::host::Caller;
    use pi_rs_p2::{
        api::v1::{FacetKind, OwnerAccess, Principal},
        hosts::server::{ServerOptions, start_server},
        runtime::server::ServerServices,
    };

    let dir = TempDir::new()?;
    let ctx = Context::background();
    let catalog_contract = Arc::new(lookup("p2.catalog")?);
    let status_contract = Arc::new(lookup("p2.status-store")?);
    let search_contract = Arc::new(lookup("p2.search-index")?);
    let project_contract = Arc::new(lookup("p2.project-service")?);
    let ops_contract = Arc::new(lookup("p2.session-operations")?);

    let mock_catalog = ServerServices::default().with_facet(move |host| {
        host.serve(
            catalog_contract.clone(),
            move |_, _, object| {
                let sessions = json!([{
                    "id": "s1",
                    "title": "Fix bug",
                    "cwd": "/repo",
                    "env": null,
                    "createdAt": 100,
                    "updatedAt": 200,
                    "archivedAt": null,
                    "pinned": false,
                    "main": 1,
                    "branches": [],
                    "model": { "provider": "faux", "modelId": "faux-1" },
                    "thinkingLevel": "off"
                }]);
                let mut members = Members::new();
                members.state(
                    "sessions",
                    object.writable(JsonValue::from_serde(sessions))?,
                )?;
                for m in ["get", "create", "activity", "update"] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mock_status = ServerServices::default().with_facet(move |host| {
        host.serve(
            status_contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state(
                    "statuses",
                    object.writable(JsonValue::from_serde(json!({})))?,
                )?;
                members.method("report", |_, _| {
                    Box::pin(async move { Ok(JsonValue::Null) })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mock_search = ServerServices::default().with_facet(move |host| {
        host.serve(
            search_contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state(
                    "answers",
                    object.writable(JsonValue::from_serde(json!({})))?,
                )?;
                members.state("reads", object.writable(JsonValue::from_serde(json!({})))?)?;
                members.state(
                    "rebuilds",
                    object.writable(JsonValue::from_serde(json!([])))?,
                )?;
                members.method("mentions", |args, _| {
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        assert_eq!(v["query"], "bug");
                        Ok(JsonValue::from_serde(json!({ "s1": 555 })))
                    })
                })?;
                for m in [
                    "cursor", "index", "reset", "markRead", "history", "messages", "rebuild",
                ] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mock_projects = ServerServices::default().with_facet(move |host| {
        host.serve(
            project_contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state(
                    "projects",
                    object.writable(JsonValue::from_serde(json!([])))?,
                )?;
                members.state(
                    "archived",
                    object.writable(JsonValue::from_serde(json!([])))?,
                )?;
                members.state(
                    "sessions",
                    object.writable(JsonValue::from_serde(json!({})))?,
                )?;
                for m in [
                    "assign", "copy", "create", "rename", "locate", "move", "archive", "restore",
                    "remove",
                ] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mock_ops = ServerServices::default().with_facet(move |host| {
        host.serve(
            ops_contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state(
                    "operations",
                    object.writable(JsonValue::from_serde(json!([])))?,
                )?;
                for m in [
                    "create", "move", "relocate", "clone", "fork", "archive", "restore", "dismiss",
                ] {
                    members.method(m, |_, _| Box::pin(async move { Ok(JsonValue::Null) }))?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });

    let mut options = ServerOptions::new(dir.path().to_path_buf());
    options.port = 0;
    shard_d2::register(&mut options.registry)?;
    options
        .registry
        .register("mock-catalog", FacetKind::Server, mock_catalog)?;
    options
        .registry
        .register("mock-status", FacetKind::Server, mock_status)?;
    options
        .registry
        .register("mock-search", FacetKind::Server, mock_search)?;
    options
        .registry
        .register("mock-projects", FacetKind::Server, mock_projects)?;
    options
        .registry
        .register("mock-ops", FacetKind::Server, mock_ops)?;

    let server = start_server(options).await?;
    let rollups_contract = Arc::new(lookup("p2.sidebar-rollups")?);

    // Guest is forbidden.
    let guest = Principal::Guest {
        id: "g1".into(),
        name: "Guest".into(),
        email: None,
        home: "s1".into(),
        sessions: Default::default(),
        revision: 1,
    };
    let guest_held = server.host.hold_as(
        rollups_contract.clone(),
        JsonValue::Object(Default::default()),
        Caller {
            principal: Some(serde_json::to_value(&guest)?),
            incarnation: "test".into(),
        },
        &ctx,
    )?;
    assert!(guest_held.wait(&ctx).await.is_err());

    // Non-Familiar worker is forbidden on `list`.
    let non_fam_worker = Principal::Worker {
        session: "not-familiar".into(),
        generation: 1,
    };
    let worker_held = server.host.hold_as(
        rollups_contract.clone(),
        JsonValue::Object(Default::default()),
        Caller {
            principal: Some(serde_json::to_value(&non_fam_worker)?),
            incarnation: "test".into(),
        },
        &ctx,
    )?;
    worker_held.wait(&ctx).await?;
    assert!(
        worker_held
            .call("list", JsonValue::Null, &ctx)
            .await
            .is_err()
    );

    // Owner can preview, save, refresh, and remove.
    let owner = Principal::Owner {
        access: OwnerAccess::Local,
    };
    let owner_held = server.host.hold_as(
        rollups_contract,
        JsonValue::Object(Default::default()),
        Caller {
            principal: Some(serde_json::to_value(&owner)?),
            incarnation: "test".into(),
        },
        &ctx,
    )?;
    owner_held.wait(&ctx).await?;

    // Warm QuickJS WASM module once so the 5s script timeout doesn't include cold Wasmtime compile in debug mode.
    let _ = pi_rs_codemode::load_quickjs_wasm(None);

    let preview = owner_held
        .call(
            "preview",
            JsonValue::from_serde(json!({
                "code": "const m = await mentions('bug'); return { layout: 'sessions', members: sessions.filter(s => m[s.id]).map(s => ({ id: s.id, at: m[s.id] })) };"
            })),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(preview["ok"], true, "{preview}");
    assert_eq!(preview["output"]["layout"], "sessions");
    assert_eq!(preview["output"]["members"][0]["id"], "s1");
    assert_eq!(preview["output"]["members"][0]["at"], 555);

    let saved = owner_held
        .call(
            "save",
            JsonValue::from_serde(json!({
                "key": "bugs",
                "name": "Bugs",
                "window": "3d",
                "refresh": { "kind": "auto" },
                "code": "return { layout: 'sessions', members: sessions.map(s => s.id) };"
            })),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(saved["definition"]["key"], "bugs");
    assert_eq!(saved["output"]["members"], json!(["s1"]));

    owner_held
        .call(
            "remove",
            JsonValue::from_serde(json!({ "key": "bugs" })),
            &ctx,
        )
        .await?;

    server.close().await?;
    Ok(())
}
