use std::{
    error::Error as StdError,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_chord::{
    context::Context,
    host::{Caller, Members, ServeOptions, define_facet},
    json::JsonValue,
    node::workers::{Control, Frame, WorkerSession, read_frame, write_frame},
};
use pi_rs_durable::{
    EntryRecord, ROOT_CONVERSATION_ID,
    harness::{InboxItem, InboxState, InputEntry},
};
use pi_rs_p2::{
    api::v1::{
        FacetKind, GrantMode, OwnerAccess, PluginRegistry, Principal, p2_contracts::lookup,
        server::service_of, session_address,
    },
    hosts::worker::{SessionWorker, start_session_worker},
    plugins::chat_mode::{
        SEND_ROUTERS_SERVICE, SendRouterInput, SendRoutersService,
        contract::{
            Attachment, AttachmentRef, Author, CHAT_ENTRY, DEFAULT_AGENT_NAME, MENTION_ENTRY,
            ReplyTo, chat_message, is_valid_agent_name, mention, mention_token, mentions_agent,
            spoken_mention,
        },
        digest::{
            ComposeMentionInput, EXCERPT_LENGTH, compose_mention, digest, digest_files, excerpt,
            quote_of, read_enough, unmentioned,
        },
        display::chat_entry_display,
    },
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncRead, DuplexStream};

const TIMEOUT: Duration = Duration::from_secs(10);
type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

fn owner() -> Principal {
    Principal::Owner {
        access: OwnerAccess::Local,
    }
}

fn guest(id: &str, session: &str, mode: GrantMode) -> Principal {
    let mut sessions = indexmap::IndexMap::new();
    sessions.insert(session.to_string(), mode);
    Principal::Guest {
        id: id.into(),
        name: "Alice".into(),
        email: None,
        home: "/tmp".into(),
        sessions,
        revision: 1,
    }
}

fn author(kind: &str, id: &str, name: &str) -> Author {
    Author {
        kind: kind.to_string(),
        id: id.to_string(),
        name: name.to_string(),
    }
}

fn make_entry(id: u64, kind: &str, data: Option<Value>, model: Option<Vec<Value>>) -> EntryRecord {
    EntryRecord {
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: kind.to_string(),
        model,
        data,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn start_frame(directory: &Path, cwd: &Path, session_id: &str) -> Frame {
    Frame::Control(Control::Start {
        address: session_address(session_id),
        start: json!({
            "app": directory,
            "dataDir": directory,
            "fake": true,
            "telemetry": false,
            "generation": 1,
            "values": { "session": {
                "id": session_id,
                "cwd": cwd,
                "model": { "provider": "faux", "modelId": "faux-1" },
                "thinkingLevel": "off",
                "resume": true,
            } },
        }),
        scripts: Vec::new(),
        lock: directory.join("worker.lock"),
    })
}

async fn next_control(parent: &mut (impl AsyncRead + Unpin)) -> TestResult<Control> {
    Ok(tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Frame::Control(control) = read_frame(parent).await?
                && !matches!(control, Control::Shutdown)
            {
                return Ok::<_, pi_rs_chord::Error>(control);
            }
        }
    })
    .await??)
}

async fn spawn_worker(
    fixture: &Path,
    session_id: &str,
    registry: &PluginRegistry,
) -> TestResult<(SessionWorker, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (child_read, child_write) = tokio::io::split(child);
    write_frame(&mut parent, &start_frame(fixture, fixture, session_id)).await?;
    let session =
        tokio::time::timeout(TIMEOUT, WorkerSession::start(child_read, child_write)).await??;
    let worker = start_session_worker(session, registry, None).await?;
    assert!(matches!(next_control(&mut parent).await?, Control::Started));
    assert!(matches!(
        next_control(&mut parent).await?,
        Control::Ready { .. }
    ));
    Ok((worker, parent))
}

// ============================================================================
// chat-mode pure unit tests
// ============================================================================

#[test]
fn chat_mode_mention_matching_and_spoken_mention() {
    assert_eq!(mention_token("Pi"), "@pi");
    assert_eq!(mention_token("Claude"), "@claude");

    assert!(is_valid_agent_name("Pi"));
    assert!(is_valid_agent_name("Claude_2-dev"));
    assert!(!is_valid_agent_name(""));
    assert!(!is_valid_agent_name("1Pi"));
    assert!(!is_valid_agent_name(&"a".repeat(33)));

    assert!(mentions_agent("@pi hello", DEFAULT_AGENT_NAME));
    assert!(mentions_agent("hey (@PI), look", DEFAULT_AGENT_NAME));
    assert!(mentions_agent("@Pi,", DEFAULT_AGENT_NAME));
    assert!(!mentions_agent("@pizza", DEFAULT_AGENT_NAME));
    assert!(!mentions_agent("me@pi.com", DEFAULT_AGENT_NAME));
    assert!(!mentions_agent("@@pi", DEFAULT_AGENT_NAME));
    assert!(!mentions_agent("@pi-hole", DEFAULT_AGENT_NAME));

    assert_eq!(
        spoken_mention("Hey Pi, summarize this", DEFAULT_AGENT_NAME),
        "@pi Hey Pi, summarize this"
    );
    assert_eq!(
        spoken_mention("Pi's answer was odd", DEFAULT_AGENT_NAME),
        "Pi's answer was odd"
    );
    assert_eq!(
        spoken_mention("Pi’s answer was odd", DEFAULT_AGENT_NAME),
        "Pi’s answer was odd"
    );
    assert_eq!(
        spoken_mention("eat pie now", DEFAULT_AGENT_NAME),
        "eat pie now"
    );
    assert_eq!(
        spoken_mention("@pi already mentioned Pi", DEFAULT_AGENT_NAME),
        "@pi already mentioned Pi"
    );
}

#[test]
fn chat_mode_excerpt_and_quote_of() {
    let short = "  hello   world \n test ";
    assert_eq!(excerpt(short), "hello world test");

    let long = "a ".repeat(40);
    let q = excerpt(&long);
    assert!(q.ends_with('…'));
    assert!(q.chars().count() <= EXCERPT_LENGTH);

    let assistant_entry = make_entry(
        10,
        "pi.assistant",
        None,
        Some(vec![json!({
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "hidden" },
                { "type": "text", "text": "Here is the answer." }
            ]
        })]),
    );
    let quoted = quote_of(&assistant_entry).expect("assistant quote");
    assert_eq!(quoted.entry_id, 10);
    assert_eq!(quoted.author.kind, "agent");
    assert_eq!(quoted.excerpt, "Here is the answer.");

    let chat_entry = make_entry(
        11,
        CHAT_ENTRY,
        Some(json!({
            "author": author("guest", "g1", "Bob"),
            "text": "should we split this?",
            "attachments": []
        })),
        None,
    );
    let quoted_chat = quote_of(&chat_entry).expect("chat quote");
    assert_eq!(quoted_chat.author.name, "Bob");
    assert_eq!(quoted_chat.excerpt, "should we split this?");
}

#[test]
fn chat_mode_unmentioned_digest_and_display() {
    // Entries are newest-first.
    let e4 = make_entry(
        4,
        CHAT_ENTRY,
        Some(json!({
            "author": author("guest", "g1", "Ann"),
            "text": "new chat",
            "attachments": [{"id": "u4", "name": "notes.txt"}]
        })),
        None,
    );
    let e3 = make_entry(
        3,
        MENTION_ENTRY,
        Some(json!({
            "author": author("owner", "o1", "Owner"),
            "text": "@pi look",
            "attachments": [],
            "included": [2]
        })),
        None,
    );
    let e2 = make_entry(
        2,
        CHAT_ENTRY,
        Some(json!({
            "author": author("owner", "o1", "Owner"),
            "text": "already included",
            "attachments": []
        })),
        None,
    );
    let e1 = make_entry(
        1,
        CHAT_ENTRY,
        Some(json!({
            "author": author("owner", "o1", "Owner"),
            "text": "before boundary",
            "attachments": []
        })),
        None,
    );

    let entries = vec![e4.clone(), e3.clone(), e2.clone(), e1];
    assert!(read_enough(&entries));
    assert!(!read_enough(&[e4.clone(), e3.clone()]));

    let pending = unmentioned(&entries, None);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, 4);

    // If e4 is already covered in a queued inbox mention, unmentioned skips it.
    let inbox = InboxState {
        items: vec![InboxItem::Steer {
            id: 99,
            content: json!(""),
            entry: Some(InputEntry {
                kind: MENTION_ENTRY.into(),
                data: Some(json!({
                    "author": author("owner", "o1", "Owner"),
                    "text": "@pi",
                    "attachments": [],
                    "included": [4]
                })),
            }),
            on_abort: None,
        }],
    };
    assert!(unmentioned(&entries, Some(&inbox)).is_empty());

    let reply = ReplyTo {
        entry_id: 2,
        author: author("owner", "o1", "Owner"),
        excerpt: "already included".into(),
    };
    let d = digest(
        &pending,
        &author("guest", "g1", "Ann"),
        "@pi help",
        Some(&reply),
    );
    assert!(d.contains("<chat>"));
    assert!(d.contains("Ann (guest): new chat [attached: notes.txt]"));
    assert!(
        d.contains("Ann (guest, replying to Owner: \"already included\") mentioned you:\n@pi help")
    );

    let files = digest_files(&pending, &["own1".to_string()]);
    assert_eq!(files, vec!["u4".to_string(), "own1".to_string()]);

    let submitted = compose_mention(ComposeMentionInput {
        author: author("guest", "g1", "Ann"),
        text: "@pi help".into(),
        chat: &pending,
        reply_to: Some(reply.clone()),
        attachments: vec![AttachmentRef {
            id: "own1".into(),
            name: "own.png".into(),
        }],
        files: vec![Attachment {
            id: "own1".into(),
            name: "own.png".into(),
            path: "/tmp/own.png".into(),
            local_path: None,
            size: 12,
            mime_type: Some("image/png".into()),
        }],
        request_id: "req-1".into(),
        when_busy: Some("steer".into()),
    });
    assert_eq!(submitted.entry.kind, MENTION_ENTRY);
    assert_eq!(submitted.entry.data.included, vec![4]);
    assert_eq!(submitted.max_images, 4);

    assert!(chat_message(e4.data.as_ref()).is_some());
    assert!(mention(e3.data.as_ref()).is_some());

    let disp_chat = chat_entry_display(CHAT_ENTRY, e4.data.as_ref()).expect("chat display");
    assert_eq!(disp_chat.note, None);
    let disp_mention =
        chat_entry_display(MENTION_ENTRY, e3.data.as_ref()).expect("mention display");
    assert_eq!(disp_mention.note.as_deref(), Some("with 1 chat message"));
}

// ============================================================================
// chat-mode worker facet integration tests
// ============================================================================

#[derive(Default)]
struct MockChatCalls {
    writes: Vec<Value>,
    submits: Vec<Value>,
}

fn register_chat_mode_test_mocks(
    registry: &mut PluginRegistry,
    calls: Arc<Mutex<MockChatCalls>>,
) -> TestResult {
    let conv_contract = Arc::new(lookup("p2.conversation")?);
    let att_contract = Arc::new(lookup("p2.attachments")?);

    registry.register(
        "mock-chat",
        FacetKind::Worker,
        define_facet(move |host| {
            let calls_w = calls.clone();
            host.serve(
                conv_contract.clone(),
                move |_, _, object| {
                    let mut members = Members::new();
                    let view = object.writable(JsonValue::from_serde(json!({
                        "entries": [],
                        "docs": {}
                    })))?;
                    let stats = object.writable(JsonValue::Null)?;
                    let model = object.writable(JsonValue::Null)?;
                    members.state("view", view)?;
                    members.state("stats", stats)?;
                    members.state("model", model)?;

                    for m in [
                        "costs",
                        "stop",
                        "dequeue",
                        "entries",
                        "image",
                        "configure",
                        "compact",
                        "reset",
                        "abortCompaction",
                        "dequeueOwn",
                    ] {
                        members.method(m, |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    }

                    let c1 = calls_w.clone();
                    members.method("write", move |args, _| {
                        let c1 = c1.clone();
                        Box::pin(async move {
                            c1.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .writes
                                .push(args.to_serde()?);
                            Ok(JsonValue::from_serde(json!({ "submissionId": 101 })))
                        })
                    })?;
                    let c2 = calls_w.clone();
                    members.method("submit", move |args, _| {
                        let c2 = c2.clone();
                        Box::pin(async move {
                            c2.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .submits
                                .push(args.to_serde()?);
                            Ok(JsonValue::from_serde(json!({ "submissionId": 102 })))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
            host.serve(
                att_contract.clone(),
                |_, _, _| {
                    let mut members = Members::new();
                    members.method("resolve", |args, _| {
                        Box::pin(async move {
                            let v = args.to_serde()?;
                            let ids = v["ids"].as_array().cloned().unwrap_or_default();
                            let out: Vec<Value> = ids
                                .into_iter()
                                .filter_map(|id| id.as_str().map(|s| s.to_string()))
                                .map(|id| {
                                    json!({
                                        "id": id,
                                        "name": format!("{id}.txt"),
                                        "path": format!("/tmp/{id}.txt"),
                                        "size": 5
                                    })
                                })
                                .collect();
                            Ok(JsonValue::from_serde(Value::Array(out)))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
            Ok(())
        }),
    )?;
    Ok(())
}

#[tokio::test]
async fn chat_mode_worker_permissions_settings_and_routing() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();
    let calls = Arc::new(Mutex::new(MockChatCalls::default()));

    let mut registry = PluginRegistry::new();
    register_chat_mode_test_mocks(&mut registry, calls.clone())?;
    pi_rs_p2::plugins::chat_mode::register(&mut registry)?;

    let (worker, _parent) = spawn_worker(fixture.path(), "sess-cm", &registry).await?;
    let host = worker.host.clone();

    let remote_contract = Arc::new(lookup("p2.chat-mode")?);
    let params = JsonValue::from_serde(json!({ "session": "sess-cm", "conversation": 1 }));

    // Unauthorized guest (different session) is forbidden.
    let wrong_guest = Caller {
        principal: Some(serde_json::to_value(guest(
            "guest-wrong",
            "other-sess",
            GrantMode::Full,
        ))?),
        incarnation: "inc-1".into(),
    };
    let denied = host.hold_as(
        remote_contract.clone(),
        params.clone(),
        wrong_guest,
        &context,
    )?;
    assert!(denied.wait(&context).await.is_err());

    // Guest gets reader variant (no set/rename methods).
    let reader_caller = Caller {
        principal: Some(serde_json::to_value(guest(
            "guest-1",
            "sess-cm",
            GrantMode::View,
        ))?),
        incarnation: "inc-1".into(),
    };
    let reader_held = host.hold_as(
        remote_contract.clone(),
        params.clone(),
        reader_caller,
        &context,
    )?;
    reader_held
        .wait(&context)
        .await
        .map_err(|e| format!("reader wait: {e}"))?;
    assert_eq!(reader_held.variant()?, "reader");
    assert_eq!(
        reader_held
            .member("enabled")?
            .value()?
            .to_json()
            .to_serde()?,
        json!(false)
    );
    assert_eq!(
        reader_held.member("name")?.value()?.to_json().to_serde()?,
        json!("Pi")
    );
    assert!(
        reader_held
            .call(
                "set",
                JsonValue::from_serde(json!({"enabled": true})),
                &context
            )
            .await
            .is_err()
    );

    // Owner gets owner variant and can toggle & rename.
    let owner_caller = Caller {
        principal: Some(serde_json::to_value(owner())?),
        incarnation: "inc-1".into(),
    };
    let owner_held = host.hold_as(
        remote_contract.clone(),
        params.clone(),
        owner_caller,
        &context,
    )?;
    owner_held
        .wait(&context)
        .await
        .map_err(|e| format!("owner wait: {e}"))?;
    assert_eq!(owner_held.variant()?, "owner");

    // Register a tiny local test contract to obtain a real Chord `Ctx` for invoking `SendRoutersService`.
    let route_contract = Arc::new(pi_rs_chord::contracts::Contract::local(
        "test.route",
        1,
        vec![],
        indexmap::IndexMap::from([(
            "dispatch".into(),
            pi_rs_chord::contracts::Member::Method {
                args: pi_rs_chord::contracts::schema::unknown(),
                result: pi_rs_chord::contracts::schema::unknown(),
            },
        )]),
    )?);
    let mut extra_reg = PluginRegistry::new();
    register_chat_mode_test_mocks(&mut extra_reg, calls.clone())?;
    let _ = extra_reg;
    // Call SendRoutersService directly via a local test facet on a fresh worker or via a method on mock-chat:
    // Actually we can call it using `mock-chat`'s `p2.conversation` or add a `route` helper?
    // Even simpler: add a `test.route` facet to the worker's registry before `spawn_worker`!
    let _ = route_contract;
    worker.close().await?;

    let mut registry2 = PluginRegistry::new();
    register_chat_mode_test_mocks(&mut registry2, calls.clone())?;
    pi_rs_p2::plugins::chat_mode::register(&mut registry2)?;
    let route_contract = Arc::new(pi_rs_chord::contracts::Contract::local(
        "test.route",
        1,
        vec![],
        indexmap::IndexMap::from([(
            "dispatch".into(),
            pi_rs_chord::contracts::Member::Method {
                args: pi_rs_chord::contracts::schema::unknown(),
                result: pi_rs_chord::contracts::schema::unknown(),
            },
        )]),
    )?);
    let rc_for_serve = route_contract.clone();
    registry2.register(
        "test-route",
        FacetKind::Worker,
        define_facet(move |h| {
            let host_handle = h.host();
            h.serve(
                rc_for_serve.clone(),
                move |_, _, _| {
                    let mut members = Members::new();
                    let hh = host_handle.clone();
                    members.method("dispatch", move |args, ctx| {
                        let hh = hh.clone();
                        Box::pin(async move {
                            let input: SendRouterInput = serde_json::from_value(args.to_serde()?)?;
                            let routers =
                                service_of::<SendRoutersService>(&hh, SEND_ROUTERS_SERVICE)?;
                            let router = routers.get("chat-mode").ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("missing chat-mode router".into())
                            })?;
                            let res = router(input, ctx).await?;
                            Ok(res.unwrap_or(JsonValue::Null))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;

    let (worker, _parent) = spawn_worker(fixture.path(), "sess-cm-2", &registry2).await?;
    let host = worker.host.clone();
    let owner_caller = Caller {
        principal: Some(serde_json::to_value(owner())?),
        incarnation: "inc-1".into(),
    };
    let owner_held = host.hold_as(
        remote_contract,
        JsonValue::from_serde(json!({ "session": "sess-cm-2", "conversation": 1 })),
        owner_caller.clone(),
        &context,
    )?;
    owner_held.wait(&context).await?;
    let route_held = host.hold_as(
        route_contract,
        JsonValue::Object(Default::default()),
        owner_caller,
        &context,
    )?;
    route_held.wait(&context).await?;

    // Send router returns null when chat mode is disabled.
    let off_res = route_held
        .call(
            "dispatch",
            JsonValue::from_serde(serde_json::to_value(SendRouterInput {
                conversation: 1,
                author: author("owner", "o1", "Owner"),
                text: "hello".into(),
                request_id: "r1".into(),
                when_busy: None,
                attachments: vec![],
                reply_to: None,
            })?),
            &context,
        )
        .await?;
    assert!(matches!(off_res, JsonValue::Null));

    // Enable chat mode and rename agent to "Claude".
    owner_held
        .call(
            "set",
            JsonValue::from_serde(json!({ "enabled": true })),
            &context,
        )
        .await?;
    owner_held
        .call(
            "rename",
            JsonValue::from_serde(json!({ "name": "Claude" })),
            &context,
        )
        .await?;

    // Invalid name rejected.
    assert!(
        owner_held
            .call(
                "rename",
                JsonValue::from_serde(json!({ "name": "123bad" })),
                &context
            )
            .await
            .is_err()
    );

    assert_eq!(
        owner_held
            .member("enabled")?
            .value()?
            .to_json()
            .to_serde()?,
        json!(true)
    );
    assert_eq!(
        owner_held.member("name")?.value()?.to_json().to_serde()?,
        json!("Claude")
    );

    // Non-mention routes to chat.write.
    let write_res = route_held
        .call(
            "dispatch",
            JsonValue::from_serde(serde_json::to_value(SendRouterInput {
                conversation: 1,
                author: author("guest", "g1", "Alice"),
                text: "just chatting".into(),
                request_id: "r2".into(),
                when_busy: None,
                attachments: vec!["att1".into()],
                reply_to: None,
            })?),
            &context,
        )
        .await?;
    assert!(!matches!(write_res, JsonValue::Null));
    assert_eq!(
        calls.lock().unwrap_or_else(|e| e.into_inner()).writes.len(),
        1
    );

    // Mention routes to chat.submit.
    let submit_res = route_held
        .call(
            "dispatch",
            JsonValue::from_serde(serde_json::to_value(SendRouterInput {
                conversation: 1,
                author: author("owner", "o1", "Owner"),
                text: "@claude summarize".into(),
                request_id: "r3".into(),
                when_busy: Some("steer".into()),
                attachments: vec![],
                reply_to: None,
            })?),
            &context,
        )
        .await?;
    assert!(!matches!(submit_res, JsonValue::Null));
    assert_eq!(
        calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submits
            .len(),
        1
    );

    worker.close().await?;
    Ok(())
}
