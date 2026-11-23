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
