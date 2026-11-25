use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::StreamExt;
use indexmap::IndexMap;
use pi_rs_chord::{
    Error as ChordError, context::Context, host::Caller, json::JsonValue, sync::SyncFuture,
};
use pi_rs_p2::{
    api::v1::{FacetKind, GrantMode, OwnerAccess, PluginRegistry, Principal},
    hosts::server::{ServerOptions, start_server},
    plugins::{
        discord::{
            bot::{
                Bot, BotEvent, DiscordAttachment, DiscordAuthor, DiscordChannel, DiscordMessage,
                DiscordPort, DiscordThread, ReplyFile, message_of, refusal,
            },
            contract::{
                Binding, BotStatus, Command, OfferedFile, TurnState, Wake, discord_contract,
                discord_ingress_contract, discord_store_contract,
            },
            fleet::{FIRST_BOT, Fleet, parse_bots},
            relay::{
                DeliverInput, DeliverResult, MESSAGE_LIMIT, Relay, RelayState, SessionPort,
                StatePort, attachment_groups, chunk_text, command_of, may_wake,
            },
            server::mcp_server_names,
            worker::{discord_link_doc, discord_prompt, thread_subagent_base},
        },
        register_builtins,
    },
    runtime::session_lock::SessionLock,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn boxed<T: Send + 'static>(
    fut: impl Future<Output = pi_rs_chord::Result<T>> + Send + 'static,
) -> SyncFuture<T> {
    Box::pin(fut)
}

#[test]
fn discord_contracts_and_registry() -> TestResult {
    let mut registry = PluginRegistry::new();
    register_builtins(&mut registry)?;
    let selected = registry.select(None)?;
    assert!(selected.contains(&"discord".to_owned()));
    assert_eq!(
        registry
            .facets(FacetKind::Server, &["discord".into()])
            .len(),
        1
    );
    assert_eq!(
        registry
            .facets(FacetKind::Worker, &["discord".into()])
            .len(),
        1
    );

    let store = discord_store_contract()?;
    assert_eq!(store.name(), "p2.discord-store");

    let discord = discord_contract()?;
    assert_eq!(discord.name(), "p2.discord");

    let ingress = discord_ingress_contract()?;
    assert_eq!(ingress.name(), "p2.discord-ingress");

    Ok(())
}

#[tokio::test]
async fn discord_fleet_parse_and_mcp_names() -> TestResult {
    // Legacy single-token format migrates to FIRST_BOT ("bot1")
    let (legacy, migrated) = parse_bots(Some(r#"{"token":"tok-1"}"#));
    assert!(migrated);
    assert_eq!(legacy.bots.len(), 1);
    let b1 = legacy.bots.get(FIRST_BOT).ok_or("missing FIRST_BOT")?;
    assert_eq!(b1.token, "tok-1");

    // Modern multi-bot format
    let (modern, mod_migrated) = parse_bots(Some(
        r#"{"bots":{"bot1":{"token":"a","addedAt":1},"bot2":{"token":"b","addedAt":2}}}"#,
    ));
    assert!(!mod_migrated);
    assert_eq!(modern.bots.len(), 2);
    assert_eq!(modern.bots["bot2"].token, "b");

    // Invalid JSON returns empty file without migrating
    let (bad, bad_migrated) = parse_bots(Some("{not-json"));
    assert!(!bad_migrated);
    assert!(bad.bots.is_empty());

    // Fleet::open loads persisted bots
    let dir = tempfile::tempdir()?;
    let discord_dir = dir.path().join("discord");
    tokio::fs::create_dir_all(&discord_dir).await?;
    tokio::fs::write(discord_dir.join("discord.json"), r#"{"token":"secret-1"}"#).await?;
    let fleet = Fleet::open(&discord_dir).await;
    assert_eq!(fleet.views().len(), 1);
    assert_eq!(fleet.views()[0].id, FIRST_BOT);

    // mcp_server_names strips UTF-8 BOM, // line comments, /* */ block comments
    let cfg = "\u{feff}{\n  // comment\n  \"mcpServers\": {\n    /* block */\n    \"github\": {},\n    \"linear\": {}\n  }\n}\n";
    let names = mcp_server_names(cfg);
    assert_eq!(names, vec!["github".to_owned(), "linear".to_owned()]);
    assert!(mcp_server_names("").is_empty());

    Ok(())
}

#[test]
fn discord_bot_message_parsing_and_refusal() -> TestResult {
    let raw = json!({
        "id": "m1",
        "channel_id": "c1",
        "content": "hello <@&role99>!",
        "author": {
            "id": "u1",
            "username": "alice",
            "global_name": "Alice Display",
            "bot": false
        },
        "member": {
            "nick": "Alice Nick",
            "roles": ["role1", "role99"]
        },
        "mentions": [{ "id": "other-user" }],
        "mention_roles": ["role99"],
        "attachments": [
            { "filename": "note.txt", "url": "https://cdn.example/note.txt", "size": 42 }
        ]
    });

    let parsed = message_of(&raw, "bot42", Some("role99"));
    assert_eq!(parsed.id, "m1");
    assert_eq!(parsed.channel_id, "c1");
    assert_eq!(parsed.author.name, "Alice Nick");
    assert!(!parsed.author.bot);
    assert!(parsed.mentions_bot);
    assert_eq!(parsed.text, "hello <@bot42>!");
    assert_eq!(parsed.roles, vec!["role1".to_owned(), "role99".to_owned()]);
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].name, "note.txt");
    assert_eq!(parsed.attachments[0].size, 42);

    // Non-mention message does not mark mentions_bot
    let raw_plain = json!({
        "id": "m2",
        "channel_id": "c1",
        "content": "just chatting",
        "author": { "id": "u2", "username": "bob" },
        "mentions": [],
        "mention_roles": [],
        "attachments": []
    });
    let plain = message_of(&raw_plain, "bot42", None);
    assert!(!plain.mentions_bot);
    assert_eq!(plain.author.name, "bob");

    // Gateway / REST refusal messages
    let r4014 = refusal("gateway closed (4014)").ok_or("expected 4014 refusal")?;
    assert!(r4014.contains("Message Content Intent"));
    let r4004 = refusal("4004 authentication failed").ok_or("expected 4004 refusal")?;
    assert!(r4004.contains("token"));
    let r401 = refusal("HTTP 401 Unauthorized").ok_or("expected 401 refusal")?;
    assert!(r401.contains("token"));
    assert_eq!(refusal("transient network timeout"), None);

    Ok(())
}

type RecordedReply = (String, Option<String>, String, Vec<ReplyFile>);

struct FakeDiscord {
    bot_id: String,
    channels: Mutex<BTreeMap<String, DiscordChannel>>,
    history: Mutex<BTreeMap<String, Vec<DiscordMessage>>>,
    replies: Mutex<Vec<RecordedReply>>,
    typing_calls: Mutex<Vec<String>>,
    upload_limit_bytes: u64,
}

impl FakeDiscord {
    fn new(bot_id: &str) -> Self {
        Self {
            bot_id: bot_id.to_owned(),
            channels: Mutex::new(BTreeMap::new()),
            history: Mutex::new(BTreeMap::new()),
            replies: Mutex::new(Vec::new()),
            typing_calls: Mutex::new(Vec::new()),
            upload_limit_bytes: 25 * 1024 * 1024,
        }
    }
}

impl DiscordPort for FakeDiscord {
    fn bot_id(&self) -> String {
        self.bot_id.clone()
    }

    fn channel(&self, id: String) -> SyncFuture<DiscordChannel> {
        let channels = self.channels.lock();
        let ch = channels
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .and_then(|map| {
                map.get(&id)
                    .cloned()
                    .ok_or_else(|| ChordError::Invalid(format!("missing channel {id}")))
            });
        boxed(async move { ch })
    }

    fn messages(
        &self,
        channel_id: String,
        after: String,
        limit: usize,
    ) -> SyncFuture<Vec<DiscordMessage>> {
        let res = self
            .history
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|map| {
                let mut list: Vec<DiscordMessage> = map
                    .get(&channel_id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|m| m.id.as_str() > after.as_str())
                    .collect();
                list.truncate(limit);
                list
            });
        boxed(async move { res })
    }

    fn reply(
        &self,
        channel_id: String,
        message_id: Option<String>,
        text: String,
        files: Vec<ReplyFile>,
    ) -> SyncFuture<()> {
        let res = self
            .replies
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|mut out| out.push((channel_id, message_id, text, files)));
        boxed(async move { res })
    }

    fn upload_limit(&self, _channel_id: String) -> SyncFuture<u64> {
        let limit = self.upload_limit_bytes;
        boxed(async move { Ok(limit) })
    }

    fn typing(&self, channel_id: String) -> SyncFuture<()> {
        let res = self
            .typing_calls
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|mut out| out.push(channel_id));
        boxed(async move { res })
    }
}

struct FakeSessions {
    delivered: Mutex<Vec<(String, DeliverInput)>>,
    turns: Mutex<BTreeMap<u64, TurnState>>,
    commands: Mutex<Vec<(String, Option<u64>, Command)>>,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    next_sub: Mutex<u64>,
    next_conv: Mutex<u64>,
}

impl FakeSessions {
    fn new() -> Self {
        Self {
            delivered: Mutex::new(Vec::new()),
            turns: Mutex::new(BTreeMap::new()),
            commands: Mutex::new(Vec::new()),
            files: Mutex::new(BTreeMap::new()),
            next_sub: Mutex::new(100),
            next_conv: Mutex::new(10),
        }
    }
}

impl SessionPort for FakeSessions {
    fn usable(&self, _session: String) -> SyncFuture<Option<String>> {
        boxed(async move { Ok(None) })
    }

    fn lock(&self, _session: String) -> Option<SessionLock> {
        None
    }

    fn deliver(&self, session: String, input: DeliverInput) -> SyncFuture<DeliverResult> {
        let res = (|| {
            let mut sub_guard = self
                .next_sub
                .lock()
                .map_err(|_| ChordError::Invalid("lock poisoned".into()))?;
            let sub_id = *sub_guard;
            *sub_guard += 1;
            let conv_id = match input.conversation {
                Some(c) => c,
                None => {
                    let mut conv_guard = self
                        .next_conv
                        .lock()
                        .map_err(|_| ChordError::Invalid("lock poisoned".into()))?;
                    let c = *conv_guard;
                    *conv_guard += 1;
                    c
                }
            };
            let mut del_guard = self
                .delivered
                .lock()
                .map_err(|_| ChordError::Invalid("lock poisoned".into()))?;
            del_guard.push((session, input));
            Ok(DeliverResult {
                conversation: conv_id,
                submission_id: sub_id,
            })
        })();
        boxed(async move { res })
    }

    fn turn(&self, _session: String, submission_id: u64) -> SyncFuture<TurnState> {
        let res = self
            .turns
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .and_then(|map| {
                map.get(&submission_id)
                    .cloned()
                    .ok_or_else(|| ChordError::Invalid("missing turn".into()))
            });
        boxed(async move { res })
    }

    fn thread(&self, _session: String, _name: String, before: u64) -> SyncFuture<u64> {
        boxed(async move { Ok(before + 100) })
    }

    fn read(
        &self,
        _session: String,
        id: String,
        offset: u64,
        length: usize,
    ) -> SyncFuture<Vec<u8>> {
        let res = self
            .files
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|map| {
                let bytes = map.get(&id).cloned().unwrap_or_default();
                let start = (offset as usize).min(bytes.len());
                let end = start.saturating_add(length).min(bytes.len());
                bytes[start..end].to_vec()
            });
        boxed(async move { res })
    }

    fn command(
        &self,
        session: String,
        conversation: Option<u64>,
        command: Command,
    ) -> SyncFuture<String> {
        let res = self
            .commands
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|mut out| {
                out.push((session, conversation, command));
                "command accepted".to_owned()
            });
        boxed(async move { res })
    }
}

struct FakeState {
    inner: Mutex<RelayState>,
}

impl FakeState {
    fn new(initial: RelayState) -> Self {
        Self {
            inner: Mutex::new(initial),
        }
    }
}

impl StatePort for FakeState {
    fn value(&self) -> RelayState {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn change(&self, update: Box<dyn FnOnce(&mut RelayState) + Send>) -> SyncFuture<()> {
        let res = self
            .inner
            .lock()
            .map_err(|_| ChordError::Invalid("lock poisoned".into()))
            .map(|mut guard| update(&mut guard));
        boxed(async move { res })
    }
}

#[tokio::test]
async fn discord_relay_delivery_commands_threads_and_replies() -> TestResult {
    // Helper functions: command_of, may_wake, chunk_text, attachment_groups
    assert_eq!(command_of("/compact", "bot1"), Some(Command::Compact));
    assert_eq!(command_of("<@bot1>  STOP  ", "bot1"), Some(Command::Stop));
    assert_eq!(command_of("<@!bot1> status", "bot1"), Some(Command::Status));
    assert_eq!(command_of("unknown", "bot1"), None);

    let wake_anyone = Wake {
        anyone: true,
        users: vec![],
        roles: vec![],
    };
    let wake_restricted = Wake {
        anyone: false,
        users: vec!["u1".into()],
        roles: vec!["r-mod".into()],
    };
    assert!(may_wake(&wake_anyone, "anyone", &[]));
    assert!(may_wake(&wake_restricted, "u1", &[]));
    assert!(may_wake(&wake_restricted, "u2", &["r-mod".into()]));
    assert!(!may_wake(&wake_restricted, "u2", &["r-guest".into()]));

    // chunk_text respects MESSAGE_LIMIT (2000) and splits cleanly
    let long = format!("{}\n{}", "a".repeat(1500), "b".repeat(800));
    let chunks = chunk_text(&long, MESSAGE_LIMIT);
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|c| c.len() <= MESSAGE_LIMIT));

    // attachment_groups packs up to limit
    let files = vec![
        OfferedFile {
            id: "f1".into(),
            name: "a.bin".into(),
            size: 6,
        },
        OfferedFile {
            id: "f2".into(),
            name: "b.bin".into(),
            size: 6,
        },
        OfferedFile {
            id: "f3".into(),
            name: "c.bin".into(),
            size: 3,
        },
    ];
    let groups = attachment_groups(&files, 10);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].len(), 1);
    assert_eq!(groups[1].len(), 2);

    // Relay end-to-end: message delivery -> pending tracking -> check_pending -> reply
    let discord = Arc::new(FakeDiscord::new("bot-self"));
    discord.channels.lock().map_err(|_| "poison")?.insert(
        "ch1".into(),
        DiscordChannel {
            id: "ch1".into(),
            name: "general".into(),
            guild_id: "g1".into(),
            guild_name: "GuildOne".into(),
            thread: None,
            last_message_id: Some("0".into()),
        },
    );
    discord.channels.lock().map_err(|_| "poison")?.insert(
        "th1".into(),
        DiscordChannel {
            id: "th1".into(),
            name: "issue-42".into(),
            guild_id: "g1".into(),
            guild_name: "GuildOne".into(),
            thread: Some(DiscordThread {
                parent_id: "ch1".into(),
                created_at: 1000,
                created_id: "10".into(),
            }),
            last_message_id: Some("10".into()),
        },
    );
    discord.history.lock().map_err(|_| "poison")?.insert(
        "ch1".into(),
        vec![
            DiscordMessage {
                id: "11".into(),
                channel_id: "ch1".into(),
                author: DiscordAuthor {
                    id: "u1".into(),
                    name: "Alice".into(),
                    bot: false,
                },
                roles: vec![],
                text: "context line".into(),
                mentions_bot: false,
                attachments: vec![],
            },
            DiscordMessage {
                id: "12".into(),
                channel_id: "ch1".into(),
                author: DiscordAuthor {
                    id: "u1".into(),
                    name: "Alice".into(),
                    bot: false,
                },
                roles: vec![],
                text: "<@bot-self> please check".into(),
                mentions_bot: true,
                attachments: vec![DiscordAttachment {
                    name: "doc.txt".into(),
                    url: "https://cdn.example/doc.txt".into(),
                    size: 128,
                }],
            },
        ],
    );

    let mut bindings = BTreeMap::new();
    bindings.insert(
        "ch1".into(),
        Binding {
            bot_id: "bot1".into(),
            channel_id: "ch1".into(),
            channel_name: "general".into(),
            guild_id: "g1".into(),
            guild_name: "GuildOne".into(),
            session: "sess-1".into(),
            wake: wake_anyone.clone(),
            cursor: Some("10".into()),
            threads: IndexMap::new(),
            created_at: 1,
        },
    );
    let state = Arc::new(FakeState::new(RelayState {
        bindings,
        pending: BTreeMap::new(),
    }));
    let sessions = Arc::new(FakeSessions::new());
    sessions
        .files
        .lock()
        .map_err(|_| "poison")?
        .insert("f-out".into(), b"file-body".to_vec());

    let discord_lookup = {
        let d = discord.clone();
        move |bot: &str| -> Option<Arc<dyn DiscordPort>> {
            if bot == "bot1" { Some(d.clone()) } else { None }
        }
    };
    let relay = Arc::new(Relay::new(discord_lookup, sessions.clone(), state.clone()));

    let wake_msg = DiscordMessage {
        id: "12".into(),
        channel_id: "ch1".into(),
        author: DiscordAuthor {
            id: "u1".into(),
            name: "Alice".into(),
            bot: false,
        },
        roles: vec![],
        text: "<@bot-self> please check".into(),
        mentions_bot: true,
        attachments: vec![],
    };
    relay.on_message("bot1", wake_msg).await?;

    // Verify delivery and cursor advancement
    {
        let delivered = sessions.delivered.lock().map_err(|_| "poison")?;
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "sess-1");
        assert!(delivered[0].1.text.contains("context line"));
        assert!(delivered[0].1.text.contains("please check"));
        assert_eq!(delivered[0].1.files.len(), 1);
        assert_eq!(delivered[0].1.files[0].name, "doc.txt");
    }
    let st_after = state.value();
    assert_eq!(st_after.bindings["ch1"].cursor.as_deref(), Some("12"));
    assert_eq!(st_after.pending.len(), 1);
    let sub_id = st_after
        .pending
        .values()
        .next()
        .ok_or("missing pending")?
        .submission_id;

    // Complete turn with reply text + offered file -> check_pending posts reply & clears pending
    sessions.turns.lock().map_err(|_| "poison")?.insert(
        sub_id,
        TurnState::Answered {
            text: "Done checking!".into(),
            files: vec![OfferedFile {
                id: "f-out".into(),
                name: "out.txt".into(),
                size: 9,
            }],
            answer: 1,
        },
    );
    let mut check_set = BTreeSet::new();
    check_set.insert("sess-1".to_owned());
    relay.check_pending(Some(&check_set)).await?;

    {
        let replies = discord.replies.lock().map_err(|_| "poison")?;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, "ch1");
        assert_eq!(replies[0].1.as_deref(), Some("12"));
        assert_eq!(replies[0].2, "Done checking!");
        assert_eq!(replies[0].3.len(), 1);
        assert_eq!(replies[0].3[0].name, "out.txt");
        assert_eq!(replies[0].3[0].data, b"file-body");
    }
    assert!(state.value().pending.is_empty());

    // Command message (compact) triggers SessionPort::command instead of deliver
    let cmd_msg = DiscordMessage {
        id: "13".into(),
        channel_id: "ch1".into(),
        author: DiscordAuthor {
            id: "u1".into(),
            name: "Alice".into(),
            bot: false,
        },
        roles: vec![],
        text: "<@bot-self> /compact".into(),
        mentions_bot: true,
        attachments: vec![],
    };
    relay.on_message("bot1", cmd_msg).await?;
    assert_eq!(sessions.commands.lock().map_err(|_| "poison")?.len(), 1);

    // Bot-turn streak cap (MAX_BOT_TURNS = 3): 4th consecutive bot-to-bot mention is suppressed
    for idx in 20..24 {
        let id = idx.to_string();
        let bot_msg = DiscordMessage {
            id: id.clone(),
            channel_id: "ch1".into(),
            author: DiscordAuthor {
                id: "other-bot".into(),
                name: "HelperBot".into(),
                bot: true,
            },
            roles: vec![],
            text: "<@bot-self> ping".into(),
            mentions_bot: true,
            attachments: vec![],
        };
        discord
            .history
            .lock()
            .map_err(|_| "poison")?
            .insert("ch1".into(), vec![bot_msg.clone()]);
        relay.on_message("bot1", bot_msg).await?;
    }
    // 1 initial user delivery + 3 bot turns = 4 deliveries; 4th bot turn was blocked
    assert_eq!(sessions.delivered.lock().map_err(|_| "poison")?.len(), 4);

    // Thread auto-creation under bound parent channel
    let th_msg = DiscordMessage {
        id: "11".into(),
        channel_id: "th1".into(),
        author: DiscordAuthor {
            id: "u1".into(),
            name: "Alice".into(),
            bot: false,
        },
        roles: vec![],
        text: "<@bot-self> thread question".into(),
        mentions_bot: true,
        attachments: vec![],
    };
    discord
        .history
        .lock()
        .map_err(|_| "poison")?
        .insert("th1".into(), vec![th_msg.clone()]);
    relay.on_message("bot1", th_msg).await?;
    let th_binding = state.value().bindings["ch1"].threads.get("th1").cloned();
    assert!(th_binding.is_some());

    Ok(())
}

#[test]
fn discord_worker_prompt_and_slug() -> TestResult {
    let doc = discord_link_doc();
    assert_eq!(doc.version, 1);

    let prompt = discord_prompt("Guild / #general");
    assert!(prompt.contains("Guild / #general"));
    assert!(prompt.contains("send_file"));

    assert_eq!(
        thread_subagent_base("Bug Report #42!"),
        "discord-bug-report-42"
    );
    assert_eq!(thread_subagent_base("---"), "discord-thread");

    Ok(())
}

#[derive(Clone)]
struct MockDiscordServerState {
    ws_url: String,
    posted_replies: Arc<Mutex<Vec<(String, Value)>>>,
}

async fn mock_gateway_bot(State(st): State<MockDiscordServerState>) -> Json<Value> {
    Json(json!({ "url": st.ws_url }))
}

async fn mock_guilds() -> Json<Value> {
    Json(json!([{ "id": "g1", "name": "TestGuild" }]))
}

async fn mock_guild_channels(Path(_guild_id): Path<String>) -> Json<Value> {
    Json(json!([
        { "id": "c1", "name": "general", "type": 0, "position": 1 },
        { "id": "c2", "name": "voice", "type": 2, "position": 2 }
    ]))
}

async fn mock_guild_roles(Path(_guild_id): Path<String>) -> Json<Value> {
    Json(json!([
        { "id": "r-bot", "name": "PiBot", "tags": { "bot_id": "bot-uid" } },
        { "id": "r-admin", "name": "Admin" }
    ]))
}

async fn mock_post_message(
    State(st): State<MockDiscordServerState>,
    Path(channel_id): Path<String>,
    _headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if let Ok(mut guard) = st.posted_replies.lock() {
        guard.push((channel_id, body));
    }
    Json(json!({ "id": "reply-1" }))
}

async fn mock_ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_mock_ws)
}

async fn handle_mock_ws(mut socket: WebSocket) {
    // Hello (op 10)
    let hello = json!({
        "op": 10,
        "d": { "heartbeat_interval": 30000 }
    });
    if socket
        .send(WsMessage::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // Expect Identify (op 2)
    while let Some(Ok(msg)) = socket.next().await {
        if let WsMessage::Text(txt) = msg
            && let Ok(v) = serde_json::from_str::<Value>(&txt)
            && v.get("op").and_then(Value::as_u64) == Some(2)
        {
            let ready = json!({
                "op": 0,
                "s": 1,
                "t": "READY",
                "d": {
                    "user": { "id": "bot-uid", "username": "pi-bot" },
                    "application": { "id": "app-1" },
                    "guilds": [{ "id": "g1" }]
                }
            });
            let _ = socket.send(WsMessage::Text(ready.to_string().into())).await;
            break;
        }
    }
    while let Some(Ok(_)) = socket.next().await {}
}

#[tokio::test]
async fn discord_bot_mock_gateway_and_rest() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let base = format!("http://{addr}/api/v10");
    let ws_url = format!("ws://{addr}/ws");
    let posted_replies = Arc::new(Mutex::new(Vec::new()));
    let state = MockDiscordServerState {
        ws_url: ws_url.clone(),
        posted_replies: posted_replies.clone(),
    };

    let app = Router::new()
        .route("/api/v10/gateway/bot", get(mock_gateway_bot))
        .route("/api/v10/users/@me/guilds", get(mock_guilds))
        .route("/api/v10/guilds/{id}/channels", get(mock_guild_channels))
        .route("/api/v10/guilds/{id}/roles", get(mock_guild_roles))
        .route("/api/v10/channels/{id}/messages", post(mock_post_message))
        .route("/ws", get(mock_ws_handler))
        .with_state(state);

    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_cb = events.clone();
    let bot = Bot::with_endpoints(base, ws_url, move |ev| {
        if let Ok(mut guard) = events_cb.lock() {
            guard.push(ev);
        }
    });
    let status = bot.connect("test-token".into()).await;
    assert!(matches!(status, BotStatus::Connected { .. }));
    assert_eq!(DiscordPort::bot_id(&bot), "bot-uid");
    assert!(
        events
            .lock()
            .map_err(|_| "poison")?
            .contains(&BotEvent::Ready)
    );

    let details = bot.server("g1").await?;
    assert_eq!(details.channels.len(), 1);
    assert_eq!(details.channels[0].name, "general");
    assert_eq!(details.roles.len(), 2);

    bot.reply("c1".into(), Some("m1".into()), "hello".into(), vec![])
        .await?;
    {
        let posted = posted_replies.lock().map_err(|_| "poison")?;
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].0, "c1");
        assert_eq!(posted[0].1["content"], "hello");
    }

    bot.close().await;
    srv.abort();
    Ok(())
}

#[tokio::test]
async fn discord_server_facet_access() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = ServerOptions::new(dir.path().into());
    options.port = 0;
    register_builtins(&mut options.registry)?;
    let server = start_server(options).await?;

    let ctx = Context::background();
    let host_root = server.host.root();
    let root = host_root.hold(
        Arc::new(discord_store_contract()?),
        JsonValue::Object(IndexMap::new()),
    )?;
    root.wait(&ctx)
        .await
        .map_err(|e| format!("root wait: {e}"))?;

    let discord_svc = Arc::new(discord_contract()?);
    let owner = server.host.hold_as(
        discord_svc.clone(),
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Owner {
                access: OwnerAccess::Local,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    owner
        .wait(&ctx)
        .await
        .map_err(|e| format!("owner wait: {e}"))?;
    assert_eq!(
        owner.variant().map_err(|e| format!("owner variant: {e}"))?,
        "owner"
    );

    let mut view_sessions = IndexMap::new();
    view_sessions.insert("s1".into(), GrantMode::View);
    let guest = server.host.hold_as(
        discord_svc,
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Guest {
                id: "g1".into(),
                name: "Guest".into(),
                email: None,
                home: "s1".into(),
                sessions: view_sessions,
                revision: 1,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    assert!(guest.wait(&ctx).await.is_err());

    drop(root);
    drop(owner);
    let _ = server.close().await;
    Ok(())
}
