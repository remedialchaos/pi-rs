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
    plugins::{
        chat_mode::{
            SEND_ROUTERS_SERVICE, SendRouterInput, SendRoutersService,
            contract::{
                Attachment, AttachmentRef, Author, CHAT_ENTRY, DEFAULT_AGENT_NAME, MENTION_ENTRY,
                ReplyTo, chat_message, is_valid_agent_name, mention, mention_token, mentions_agent,
                spoken_mention,
            },
            digest::{
                ComposeMentionInput, EXCERPT_LENGTH, compose_mention, digest, digest_files,
                excerpt, quote_of, read_enough, unmentioned,
            },
            display::chat_entry_display,
        },
        prompt::{
            app_guide,
            context_files::{LocalPromptFiles, load_prompt_resources},
            contract::{PLUGIN_SKILLS_SERVICE, PluginSkillsService, TemplateSource},
            templates::{
                expand_prompt, parse_command_args, parse_frontmatter, parse_template,
                substitute_args,
            },
        },
        tree::{
            branch_target::{BranchRef, branch_of, branch_target},
            contract::{Tree, TreeBranch, TreeNode, TreeRole},
            toc::{TurnMessages, table_of_contents},
        },
        views::{
            contract::{
                AnnotationInput, MESSAGE_CONTEXTS_SERVICE, MessageContext, MessageContextsService,
                SelectInlineInput, Selection, TRANSCRIPT, View, ViewsState, is_valid_view_id,
                is_valid_view_kind, live_views,
            },
            doc::{
                ViewSpec, add_annotation, describe_annotation, free_view_id, mark_sent, open_view,
                remove_view, select_inline, view_context,
            },
            layout::{CHAT, adopt, arranged_tree, arrangement_error, initial_layout, reconcile},
        },
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

// ============================================================================
// views pure unit tests
// ============================================================================

#[test]
fn views_doc_and_context_helpers() -> TestResult {
    assert!(is_valid_view_id("notes-1_a"));
    assert!(!is_valid_view_id("Notes"));
    assert!(is_valid_view_kind("pi.artifact"));
    assert!(!is_valid_view_kind(""));

    let mut state = ViewsState::default();
    let id1 = free_view_id(&state, "Notes & Plans!", "markdown");
    assert_eq!(id1, "notes-plans");
    let v1 = open_view(
        &mut state,
        ViewSpec {
            id: id1.clone(),
            kind: "markdown".into(),
            title: "Notes & Plans!".into(),
            item: Some("docs/notes.md".into()),
        },
        1000,
    )?;
    assert_eq!(v1.id, "notes-plans");
    assert_eq!(
        free_view_id(&state, "Notes & Plans!", "markdown"),
        "notes-plans-2"
    );

    // Reopening with same id updates focus and preserves created.
    let v1_again = open_view(
        &mut state,
        ViewSpec {
            id: id1.clone(),
            kind: "markdown".into(),
            title: "Notes Updated".into(),
            item: None,
        },
        2000,
    )?;
    assert_eq!(v1_again.created, 1000);
    assert_eq!(v1_again.focus, 2000);
    assert_eq!(v1_again.item.as_deref(), Some("docs/notes.md"));

    // Custom views are filtered out by live_views.
    state.views.push(View {
        id: "old-custom".into(),
        kind: "custom".into(),
        item: None,
        title: "Old".into(),
        selection: None,
        focus: 100,
        created: 100,
    });
    assert_eq!(live_views(&state).len(), 1);

    // Selection + inline selection + annotations -> view_context + mark_sent.
    state.views[0].selection = Some(Selection {
        summary: "lines 1-5".into(),
        data: None,
        sent: None,
    });
    select_inline(
        &mut state,
        SelectInlineInput {
            kind: "md".into(),
            item: "art-1".into(),
            title: "Spec".into(),
            selection: Some(Selection {
                summary: "heading 2".into(),
                data: None,
                sent: None,
            }),
        },
    );
    let ann_id = add_annotation(
        &mut state,
        AnnotationInput::Comment {
            view: id1.clone(),
            quote: "hello   world".into(),
            note: "fix typo".into(),
        },
        "Owner",
        3000,
    )?;
    let _ink_id = add_annotation(
        &mut state,
        AnnotationInput::Ink {
            view: TRANSCRIPT.into(),
            strokes: vec![vec![0.1, 0.2, 0.8, 0.9]],
            aspect: 1.5,
            under: Some("target".into()),
            note: Some("circle".into()),
        },
        "Alice",
        3001,
    )?;
    assert!(describe_annotation(&state.views, &state.annotations[1]).contains("10–80% across"));

    let carried = view_context(&state).expect("carried context");
    assert!(carried.text.contains("<views>"));
    assert!(carried.text.contains("Selected in view \"notes-plans\""));
    assert!(carried.text.contains("Selected in md \"art-1\""));
    assert!(carried.text.contains("fix typo"));
    assert_eq!(carried.annotations.len(), 2);

    mark_sent(&mut state, &carried);
    assert!(view_context(&state).is_none());

    assert!(remove_view(&mut state, &id1));
    assert!(!remove_view(&mut state, &id1));
    // Only transcript annotation remains after view removal.
    assert_eq!(state.annotations.len(), 1);
    assert!(state.annotations[0].id != ann_id);
    Ok(())
}

#[test]
fn views_layout_reconcile_and_arrangement() {
    let mut known: std::collections::HashSet<String> =
        ["transcript".into(), "notes".into(), "diff".into()]
            .into_iter()
            .collect();
    let valid = json!({
        "row": ["transcript", { "tabs": ["notes", "diff"], "active": "diff" }],
        "sizes": [0.4, 0.6]
    });
    assert!(arrangement_error(&valid, &known).is_none());
    assert!(arrangement_error(&json!({ "tabs": [] }), &known).is_some());
    assert!(arrangement_error(&json!({ "row": ["transcript", "transcript"] }), &known).is_some());
    assert!(arrangement_error(&json!("unknown"), &known).is_some());

    let layout = initial_layout();
    let reconciled = reconcile(
        layout,
        &[("notes".into(), 50_000), ("diff".into(), 55_000)],
        60_000,
    );
    assert_eq!(reconciled.phone, "diff");
    let adopted = adopt(reconciled, &valid, 70_000, &["notes".into(), "diff".into()]);
    assert_eq!(adopted.arranged, 70_000);
    assert!(adopted.hidden.is_empty());

    known.remove("diff");
    let tree = arranged_tree(&valid, &known).expect("tree without closed diff");
    assert!(!pi_rs_p2::plugins::views::layout::views_in(&tree).contains(&"diff".to_string()));
    assert!(pi_rs_p2::plugins::views::layout::views_in(&tree).contains(&CHAT.to_string()));
}

// ============================================================================
// views worker facet integration tests
// ============================================================================

#[tokio::test]
async fn views_worker_permissions_methods_and_send_router() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();
    let calls = Arc::new(Mutex::new(MockChatCalls::default()));

    let mut registry = PluginRegistry::new();
    register_chat_mode_test_mocks(&mut registry, calls.clone())?;
    pi_rs_p2::plugins::views::register(&mut registry)?;

    let route_contract = Arc::new(pi_rs_chord::contracts::Contract::local(
        "test.views-route",
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
    let rc = route_contract.clone();
    registry.register(
        "test-views-route",
        FacetKind::Worker,
        define_facet(move |h| {
            let hh = h.host();
            h.serve(
                rc.clone(),
                move |_, _, _| {
                    let mut members = Members::new();
                    let hh2 = hh.clone();
                    members.method("dispatch", move |args, ctx| {
                        let hh2 = hh2.clone();
                        Box::pin(async move {
                            let input: SendRouterInput = serde_json::from_value(args.to_serde()?)?;
                            let routers =
                                service_of::<SendRoutersService>(&hh2, SEND_ROUTERS_SERVICE)?;
                            let router = routers.get("views").ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("missing views router".into())
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

    let (worker, _parent) = spawn_worker(fixture.path(), "sess-vw", &registry).await?;
    let host = worker.host.clone();

    // Extension p2-views is installed.
    assert!(
        worker
            .process
            .registries
            .extensions
            .snapshot()
            .extension("p2-views")
            .is_some()
    );

    let views_c = Arc::new(lookup("p2.views")?);
    let params = JsonValue::from_serde(json!({ "session": "sess-vw" }));

    // Unauthorized guest is forbidden.
    let denied = host.hold_as(
        views_c.clone(),
        params.clone(),
        Caller {
            principal: Some(serde_json::to_value(guest(
                "guest-wrong",
                "other-sess",
                GrantMode::Full,
            ))?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    assert!(denied.wait(&context).await.is_err());

    // View-only guest gets reader variant.
    let reader = host.hold_as(
        views_c.clone(),
        params.clone(),
        Caller {
            principal: Some(serde_json::to_value(guest(
                "guest-view",
                "sess-vw",
                GrantMode::View,
            ))?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    reader.wait(&context).await?;
    assert_eq!(reader.variant()?, "reader");
    assert!(
        reader
            .call(
                "open",
                JsonValue::from_serde(json!({ "kind": "markdown", "item": "README.md" })),
                &context
            )
            .await
            .is_err()
    );

    // Owner gets editor variant.
    let owner_caller = Caller {
        principal: Some(serde_json::to_value(owner())?),
        incarnation: "inc-1".into(),
    };
    let editor = host.hold_as(views_c, params, owner_caller.clone(), &context)?;
    editor.wait(&context).await?;
    assert_eq!(editor.variant()?, "editor");

    let opened = editor
        .call(
            "open",
            JsonValue::from_serde(json!({ "kind": "markdown", "item": "README.md" })),
            &context,
        )
        .await?
        .to_serde()?;
    let view_id = opened["id"].as_str().expect("view id").to_string();

    editor
        .call(
            "select",
            JsonValue::from_serde(json!({
                "id": view_id,
                "selection": { "summary": "Intro paragraph" }
            })),
            &context,
        )
        .await?;
    let ann = editor
        .call(
            "annotate",
            JsonValue::from_serde(json!({
                "kind": "comment",
                "view": view_id,
                "quote": "Intro",
                "note": "Clarify"
            })),
            &context,
        )
        .await?
        .to_serde()?;
    assert!(ann["id"].as_str().is_some());

    // Register a MessageContextsService provider and verify send router combines both.
    let carried_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let carried_flag2 = carried_flag.clone();
    let msg_contexts = service_of::<MessageContextsService>(&host, MESSAGE_CONTEXTS_SERVICE)?;
    msg_contexts.contribute(
        "test-extra",
        Arc::new(move |_input, _ctx| {
            let cf = carried_flag2.clone();
            Box::pin(async move {
                Ok(Some(MessageContext {
                    text: "<extra>Extra context</extra>".into(),
                    note: "extra note".into(),
                    carried: Box::new(move |_| {
                        Box::pin(async move {
                            cf.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                }))
            })
        }),
    );

    let route_held = host.hold_as(
        route_contract,
        JsonValue::Object(Default::default()),
        owner_caller,
        &context,
    )?;
    route_held.wait(&context).await?;
    let res = route_held
        .call(
            "dispatch",
            JsonValue::from_serde(serde_json::to_value(SendRouterInput {
                conversation: 1,
                author: author("owner", "o1", "Owner"),
                text: "Please review".into(),
                request_id: "vr-1".into(),
                when_busy: None,
                attachments: vec![],
                reply_to: None,
            })?),
            &context,
        )
        .await?;
    assert!(!matches!(res, JsonValue::Null));
    assert!(carried_flag.load(std::sync::atomic::Ordering::SeqCst));
    let submits = calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .submits
        .clone();
    assert_eq!(submits.len(), 1);
    assert!(
        submits[0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("<views>")
    );
    assert!(
        submits[0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("<extra>Extra context</extra>")
    );

    // Close view cleans up.
    editor
        .call(
            "close",
            JsonValue::from_serde(json!({ "id": view_id })),
            &context,
        )
        .await?;

    worker.close().await?;
    Ok(())
}

// ============================================================================
// tree pure unit tests
// ============================================================================

#[test]
fn tree_branch_target_and_toc() {
    let bref = BranchRef {
        session: "s1".into(),
        conversation: 3,
    };
    let target = branch_target(&bref, "Fix bug");
    assert_eq!(target.target_type, "branch");
    assert_eq!(branch_of(Some(&target.id)), Some(bref));
    assert_eq!(branch_of(Some("[\"s1\",0]")), None);
    assert_eq!(branch_of(None), None);

    let tree = Tree {
        nodes: vec![
            TreeNode {
                entry: 1,
                parent: None,
                role: TreeRole::User,
                text: "Hello".into(),
                conversation: 1,
            },
            TreeNode {
                entry: 2,
                parent: Some(1),
                role: TreeRole::Assistant,
                text: "Step 1".into(),
                conversation: 1,
            },
            TreeNode {
                entry: 3,
                parent: Some(2),
                role: TreeRole::Assistant,
                text: "Final answer".into(),
                conversation: 1,
            },
            TreeNode {
                entry: 4,
                parent: Some(3),
                role: TreeRole::User,
                text: "Branch msg".into(),
                conversation: 2,
            },
        ],
        branches: vec![
            TreeBranch {
                conversation: 1,
                label: None,
            },
            TreeBranch {
                conversation: 2,
                label: Some("Alt".into()),
            },
        ],
        main: 1,
        current: Some(3),
    };
    let toc = table_of_contents(&tree);
    assert_eq!(toc.turns.len(), 1);
    assert!(toc.turns[0].head);
    assert!(toc.turns[0].current);
    // Intermediate assistant step (entry 2) was collapsed; exchange pairs entry 1 + entry 3.
    match &toc.turns[0].messages {
        TurnMessages::Exchange { user, answer } => {
            assert_eq!(user.entry, 1);
            assert_eq!(answer.as_ref().map(|a| a.entry), Some(3));
        }
        TurnMessages::Answer { .. } => panic!("expected exchange"),
    }
    assert_eq!(toc.turns[0].branches.len(), 1);
    assert_eq!(toc.turns[0].branches[0].conversation, 2);
}

// ============================================================================
// tree worker facet integration tests
// ============================================================================

#[tokio::test]
async fn tree_worker_permissions_branch_select_rename_and_read_only() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();

    let mut registry = PluginRegistry::new();
    let sessions_c = Arc::new(lookup("p2.sessions")?);
    let tree_reports = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tree_reports_w = tree_reports.clone();
    registry.register(
        "mock-sessions",
        FacetKind::Worker,
        define_facet(move |h| {
            let tr = tree_reports_w.clone();
            h.serve(
                sessions_c.clone(),
                move |_, _, object| {
                    let mut members = Members::variant("worker");
                    let session_state = object.writable(JsonValue::Null)?;
                    members.state("session", session_state)?;
                    for m in ["renameSelf", "configureSelf"] {
                        members.method(m, |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    }
                    let tr2 = tr.clone();
                    members.method("treeSelf", move |args, _| {
                        let tr2 = tr2.clone();
                        Box::pin(async move {
                            tr2.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(args.to_serde()?);
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;
    pi_rs_p2::plugins::tree::register(&mut registry)?;

    let (worker, _parent) = spawn_worker(fixture.path(), "sess-tr", &registry).await?;
    let host = worker.host.clone();
    let tree_c = Arc::new(lookup("p2.tree")?);
    let params = JsonValue::from_serde(json!({ "session": "sess-tr" }));

    // Guest is forbidden.
    let guest_held = host.hold_as(
        tree_c.clone(),
        params.clone(),
        Caller {
            principal: Some(serde_json::to_value(guest(
                "guest-1",
                "sess-tr",
                GrantMode::Full,
            ))?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    assert!(guest_held.wait(&context).await.is_err());

    // Owner holds p2.tree.
    let owner_held = host.hold_as(
        tree_c,
        params,
        Caller {
            principal: Some(serde_json::to_value(owner())?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    owner_held.wait(&context).await?;
    assert_eq!(owner_held.variant()?, "owner");

    // Seed root conversation with two entries (user + assistant) via Durable.
    let session = &worker.process.harness()?.durable.session;
    let (user_entry_id, asst_entry_id) = session
        .commit(
            async |tx| {
                let u = tx
                    .append_entry(
                        1,
                        pi_rs_durable::EntryDraft {
                            kind: "p2.input".into(),
                            model: Some(vec![json!({ "role": "user", "content": "First prompt" })]),
                            data: Some(json!({
                                "author": { "kind": "owner", "id": "o1", "name": "Owner" },
                                "text": "First prompt",
                                "attachments": []
                            })),
                            head: None,
                            edits: None,
                        },
                    )
                    .await?;
                let a = tx
                    .append_entry(
                        1,
                        pi_rs_durable::EntryDraft {
                            kind: "pi.assistant".into(),
                            model: Some(vec![json!({
                                "role": "assistant",
                                "content": [{ "type": "text", "text": "First reply" }]
                            })]),
                            data: None,
                            head: None,
                            edits: None,
                        },
                    )
                    .await?;
                Ok((u.id, a.id))
            },
            &context,
        )
        .await?;

    let initial_tree: Tree = serde_json::from_value(
        owner_held
            .call("tree", JsonValue::Null, &context)
            .await?
            .to_serde()?,
    )?;
    assert_eq!(initial_tree.main, 1);
    assert_eq!(initial_tree.nodes.len(), 2);
    assert_eq!(initial_tree.current, Some(asst_entry_id));

    // Fork a new branch.
    let branched = owner_held
        .call(
            "branch",
            JsonValue::from_serde(json!({ "conversation": 1, "label": "Explore" })),
            &context,
        )
        .await?
        .to_serde()?;
    let fork_id = branched["conversation"].as_u64().expect("fork id");
    assert_ne!(fork_id, 1);

    // Conversation 1 is now read-only because fork_id is main.
    assert!(
        worker
            .process
            .registries
            .read_only_reason(1, &context)
            .await?
            .is_some()
    );
    assert!(
        worker
            .process
            .registries
            .read_only_reason(fork_id, &context)
            .await?
            .is_none()
    );

    // Rename the fork; blank label rejected for non-root.
    assert!(
        owner_held
            .call(
                "rename",
                JsonValue::from_serde(json!({ "conversation": fork_id, "label": "   " })),
                &context
            )
            .await
            .is_err()
    );
    owner_held
        .call(
            "rename",
            JsonValue::from_serde(json!({ "conversation": fork_id, "label": "Renamed" })),
            &context,
        )
        .await?;

    // Navigate to user_entry_id -> returns user text and creates/switches to new branch before it.
    let nav = owner_held
        .call(
            "navigate",
            JsonValue::from_serde(json!({ "entry": user_entry_id })),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(nav["text"].as_str(), Some("First prompt"));

    // Select back to conversation 1.
    owner_held
        .call(
            "select",
            JsonValue::from_serde(json!({ "conversation": 1 })),
            &context,
        )
        .await?;
    assert!(
        worker
            .process
            .registries
            .read_only_reason(1, &context)
            .await?
            .is_none()
    );
    assert!(
        !tree_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );

    worker.close().await?;
    Ok(())
}

// ============================================================================
// prompt pure unit tests
// ============================================================================

#[tokio::test]
async fn prompt_templates_frontmatter_and_context_files() -> TestResult {
    let raw = "---\ndescription: \"Summarize diff\"\nargument-hint: \"[scope]\"\n---\nReview $1 and ${2:-all} (${@:2:2})";
    let (fm, body) = parse_frontmatter(raw);
    assert_eq!(
        fm.get("description").and_then(Value::as_str),
        Some("Summarize diff")
    );
    assert_eq!(body, "Review $1 and ${2:-all} (${@:2:2})");

    let tpl = parse_template("review.md", raw, TemplateSource::Project, "/tmp/review.md");
    assert_eq!(tpl.name, "review");
    assert_eq!(tpl.argument_hint.as_deref(), Some("[scope]"));

    let args = parse_command_args("src \"two words\" three four");
    assert_eq!(args, vec!["src", "two words", "three", "four"]);
    assert_eq!(
        substitute_args(&tpl.content, &args),
        "Review src and two words (two words three)"
    );
    assert_eq!(substitute_args("All: ${ARGUMENTS:-none}", &[]), "All: none");

    let expanded = expand_prompt("/review src", &[tpl], &[], |_| async { None }).await;
    assert_eq!(expanded.text, "Review src and all ()");

    let guide = app_guide("/opt/p2", "/var/p2-data", Some("dev-box"));
    assert!(guide.contains("\"/opt/p2\""));
    assert!(guide.contains("dev-box"));

    // Filesystem discovery test.
    let dir = TempDir::new()?;
    let data = dir.path().join("data");
    let cwd = dir.path().join("project");
    let plugin_sk = dir.path().join("plugin-skills");
    std::fs::create_dir_all(data.join("skills/global-sk"))?;
    std::fs::create_dir_all(data.join("prompts"))?;
    std::fs::create_dir_all(cwd.join(".pi/skills/proj-sk"))?;
    std::fs::create_dir_all(cwd.join(".pi/prompts"))?;
    std::fs::create_dir_all(plugin_sk.join("plug-sk"))?;

    std::fs::write(data.join("AGENTS.md"), "Global rules")?;
    std::fs::write(cwd.join("AGENTS.md"), "Project rules")?;
    std::fs::write(
        data.join("skills/global-sk/SKILL.md"),
        "---\ndescription: Global skill\n---\nGlobal body",
    )?;
    std::fs::write(
        cwd.join(".pi/skills/proj-sk/SKILL.md"),
        "---\ndescription: Project skill\ndisable-model-invocation: true\n---\nProject body",
    )?;
    std::fs::write(
        plugin_sk.join("plug-sk/SKILL.md"),
        "---\ndescription: Plugin skill\n---\nPlugin body",
    )?;
    std::fs::write(
        cwd.join(".pi/prompts/hello.md"),
        "---\ndescription: Greet\n---\nHello $1!",
    )?;

    let loaded = load_prompt_resources(
        data.to_str().unwrap_or(""),
        cwd.to_str().unwrap_or(""),
        &LocalPromptFiles,
        data.join("skills").to_str().unwrap_or(""),
        data.join("prompts").to_str().unwrap_or(""),
        &[plugin_sk.to_string_lossy().into_owned()],
    )
    .await?;

    assert!(loaded.agents.contains("Global rules"));
    assert!(loaded.agents.contains("Project rules"));
    assert!(!loaded.project.agents.contains("Global rules"));
    assert!(loaded.project.agents.contains("Project rules"));
    // proj-sk has disable-model-invocation: true so it's in skill_files (for /skill:) but not <available_skills>.
    assert_eq!(loaded.skill_files.len(), 3);
    assert!(loaded.skills.contains("global-sk"));
    assert!(loaded.skills.contains("plug-sk"));
    assert!(!loaded.skills.contains("proj-sk"));
    assert_eq!(loaded.templates.len(), 1);
    assert_eq!(loaded.templates[0].name, "hello");
    Ok(())
}

// ============================================================================
// prompt worker facet integration tests
// ============================================================================

#[tokio::test]
async fn prompt_worker_permissions_and_expand() -> TestResult {
    let fixture = TempDir::new()?;
    let context = Context::background();
    std::fs::create_dir_all(fixture.path().join(".pi/prompts"))?;
    std::fs::create_dir_all(fixture.path().join(".pi/skills/my-skill"))?;
    std::fs::write(
        fixture.path().join(".pi/prompts/greet.md"),
        "---\ndescription: Greet user\n---\nHi $1!",
    )?;
    std::fs::write(
        fixture.path().join(".pi/skills/my-skill/SKILL.md"),
        "---\ndescription: My skill\n---\nSkill instructions here.",
    )?;

    let mut registry = PluginRegistry::new();
    pi_rs_p2::plugins::prompt::register(&mut registry)?;

    let (worker, _parent) = spawn_worker(fixture.path(), "sess-pr", &registry).await?;
    let host = worker.host.clone();

    assert!(
        worker
            .process
            .registries
            .extensions
            .snapshot()
            .extension("p2-prompt")
            .is_some()
    );
    assert!(service_of::<PluginSkillsService>(&host, PLUGIN_SKILLS_SERVICE).is_ok());

    let res_c = Arc::new(lookup("p2.prompt-resources")?);
    let params = JsonValue::from_serde(json!({ "session": "sess-pr", "conversation": 1 }));

    // View-only guest is forbidden.
    let viewer = host.hold_as(
        res_c.clone(),
        params.clone(),
        Caller {
            principal: Some(serde_json::to_value(guest(
                "guest-view",
                "sess-pr",
                GrantMode::View,
            ))?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    assert!(viewer.wait(&context).await.is_err());

    // Participant (full-grant guest) can expand templates and skills.
    let participant = host.hold_as(
        res_c,
        params,
        Caller {
            principal: Some(serde_json::to_value(guest(
                "guest-full",
                "sess-pr",
                GrantMode::Full,
            ))?),
            incarnation: "inc-1".into(),
        },
        &context,
    )?;
    participant.wait(&context).await?;
    assert_eq!(participant.variant()?, "participant");

    let tpl_exp = participant
        .call(
            "expand",
            JsonValue::from_serde(json!({ "text": "/greet World" })),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(tpl_exp["text"].as_str(), Some("Hi World!"));
    assert!(tpl_exp.get("skill").is_none());

    let sk_exp = participant
        .call(
            "expand",
            JsonValue::from_serde(json!({ "text": "/skill:my-skill extra arg" })),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(sk_exp["skill"]["name"].as_str(), Some("my-skill"));
    assert!(
        sk_exp["text"]
            .as_str()
            .unwrap_or("")
            .contains("Skill instructions here.")
    );
    assert!(sk_exp["text"].as_str().unwrap_or("").contains("extra arg"));

    let unknown = participant
        .call(
            "expand",
            JsonValue::from_serde(json!({ "text": "/unknown foo" })),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(unknown["text"].as_str(), Some("/unknown foo"));

    worker.close().await?;
    Ok(())
}
