//! Discord Gateway + REST v10 client and `DiscordPort` trait.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use pi_rs_chord::{Error, Result, sync::SyncFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::contract::{BotStatus, Guild, NamedId, ServerDetails};

/// Permissions requested in the bot invite URL:
/// view channels (10), send messages (11), embed links (14), attach files (15),
/// read message history (16), send messages in threads (38).
const PERMISSIONS: u64 = (1 << 10) | (1 << 11) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 38);
const RETRY_SECS: u64 = 60;
const GATEWAY_INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 15); // Guilds | GuildMessages | MessageContent

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordAttachment {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordAuthor {
    pub id: String,
    pub name: String,
    pub bot: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
    pub author: DiscordAuthor,
    pub roles: Vec<String>,
    pub text: String,
    pub mentions_bot: bool,
    pub attachments: Vec<DiscordAttachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordThread {
    pub parent_id: String,
    pub created_at: u64,
    pub created_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordChannel {
    pub id: String,
    pub name: String,
    pub guild_id: String,
    pub guild_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<DiscordThread>,
    pub last_message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyFile {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BotEvent {
    Message(DiscordMessage),
    Ready,
}

/// Abstract Discord operations used by `Relay` (implemented by `Bot` and test fakes).
pub trait DiscordPort: Send + Sync {
    fn bot_id(&self) -> String;
    fn channel(&self, id: String) -> SyncFuture<DiscordChannel>;
    fn messages(
        &self,
        channel_id: String,
        after: String,
        limit: usize,
    ) -> SyncFuture<Vec<DiscordMessage>>;
    fn reply(
        &self,
        channel_id: String,
        message_id: Option<String>,
        text: String,
        files: Vec<ReplyFile>,
    ) -> SyncFuture<()>;
    fn upload_limit(&self, channel_id: String) -> SyncFuture<u64>;
    fn typing(&self, channel_id: String) -> SyncFuture<()>;
}

struct BotInner {
    status: Mutex<BotStatus>,
    listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    on_event: Arc<dyn Fn(BotEvent) + Send + Sync>,
    generation: AtomicU64,
    client: reqwest::Client,
    api_base: String,
    gateway_url: String,
    connection: Mutex<Option<ConnectionState>>,
}

#[derive(Clone)]
struct ConnectionState {
    token: String,
    user_id: String,
    guild_names: Arc<Mutex<BTreeMap<String, String>>>,
    guild_tiers: Arc<Mutex<BTreeMap<String, u64>>>,
    shutdown: Arc<Notify>,
}

pub struct Bot {
    inner: Arc<BotInner>,
}

impl Bot {
    pub fn new(on_event: impl Fn(BotEvent) + Send + Sync + 'static) -> Self {
        Self::with_endpoints(
            "https://discord.com/api/v10".into(),
            "wss://gateway.discord.gg/?v=10&encoding=json".into(),
            on_event,
        )
    }

    pub fn with_endpoints(
        api_base: String,
        gateway_url: String,
        on_event: impl Fn(BotEvent) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(BotInner {
                status: Mutex::new(BotStatus::Connecting),
                listeners: Mutex::new(Vec::new()),
                on_event: Arc::new(on_event),
                generation: AtomicU64::new(0),
                client: reqwest::Client::new(),
                api_base,
                gateway_url,
                connection: Mutex::new(None),
            }),
        }
    }

    pub fn status(&self) -> BotStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status(), BotStatus::Connected { .. })
    }

    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.inner
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(listener));
    }

    fn set_status(inner: &BotInner, status: BotStatus) {
        *inner.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
        let listeners = inner
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for listener in listeners {
            listener();
        }
    }

    /// Connects with `token`, replacing any previous connection, and returns the resulting status.
    pub fn connect(
        &self,
        token: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = BotStatus> + Send + '_>> {
        Box::pin(async move {
            let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some(prev) = self
                .inner
                .connection
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                prev.shutdown.notify_waiters();
            }
            Self::set_status(&self.inner, BotStatus::Connecting);

            match self.try_connect(generation, token.clone()).await {
                Ok(status) => status,
                Err(err) => {
                    if self.inner.generation.load(Ordering::Acquire) != generation {
                        return self.status();
                    }
                    let msg = err.to_string();
                    let refused = refusal(&msg);
                    if refused.is_none() {
                        let inner = self.inner.clone();
                        let retry_token = token;
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(RETRY_SECS)).await;
                            if inner.generation.load(Ordering::Acquire) == generation {
                                let bot = Bot { inner };
                                let _ = bot.connect(retry_token).await;
                            }
                        });
                    }
                    let status = BotStatus::Error {
                        error: refused
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Could not reach Discord: {msg}")),
                    };
                    Self::set_status(&self.inner, status.clone());
                    status
                }
            }
        })
    }

    async fn try_connect(&self, generation: u64, token: String) -> Result<BotStatus> {
        let (ws_stream, _) = tokio::time::timeout(
            Duration::from_secs(30),
            tokio_tungstenite::connect_async(&self.inner.gateway_url),
        )
        .await
        .map_err(|_| Error::Invalid("Discord did not get ready".into()))?
        .map_err(|e| Error::Invalid(e.to_string()))?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();
        let shutdown = Arc::new(Notify::new());
        let guild_names = Arc::new(Mutex::new(BTreeMap::new()));
        let guild_tiers = Arc::new(Mutex::new(BTreeMap::new()));
        let bot_roles: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let (ready_tx, ready_rx) = oneshot::channel::<Result<BotStatus>>();
        let mut ready_tx = Some(ready_tx);

        let inner = self.inner.clone();
        let state = ConnectionState {
            token: token.clone(),
            user_id: String::new(),
            guild_names: guild_names.clone(),
            guild_tiers: guild_tiers.clone(),
            shutdown: shutdown.clone(),
        };

        let conn_state = Arc::new(Mutex::new(state));
        let loop_state = conn_state.clone();
        let loop_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut seq: Option<u64> = None;
            let mut heartbeat_interval_ms: Option<u64> = None;
            let mut next_heartbeat = tokio::time::interval(Duration::from_secs(45));
            next_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                if inner.generation.load(Ordering::Acquire) != generation {
                    let _ = ws_tx.close().await;
                    break;
                }
                tokio::select! {
                    _ = loop_shutdown.notified() => {
                        let _ = ws_tx.close().await;
                        break;
                    }
                    _ = next_heartbeat.tick(), if heartbeat_interval_ms.is_some() => {
                        let payload = json!({ "op": 1, "d": seq });
                        if ws_tx.send(WsMessage::Text(payload.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    msg = ws_rx.next() => {
                        let Some(Ok(msg)) = msg else {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Err(Error::Invalid("Discord gateway closed".into())));
                            }
                            break;
                        };
                        match msg {
                            WsMessage::Text(text) => {
                                let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                                    continue;
                                };
                                if let Some(s) = payload.get("s").and_then(Value::as_u64) {
                                    seq = Some(s);
                                }
                                let op = payload.get("op").and_then(Value::as_u64).unwrap_or(0);
                                match op {
                                    10 => {
                                        let interval = payload
                                            .get("d")
                                            .and_then(|d| d.get("heartbeat_interval"))
                                            .and_then(Value::as_u64)
                                            .unwrap_or(41_250);
                                        heartbeat_interval_ms = Some(interval);
                                        next_heartbeat = tokio::time::interval(Duration::from_millis(interval));
                                        next_heartbeat.tick().await;
                                        let identify = json!({
                                            "op": 2,
                                            "d": {
                                                "token": token,
                                                "intents": GATEWAY_INTENTS,
                                                "properties": {
                                                    "os": std::env::consts::OS,
                                                    "browser": "p2",
                                                    "device": "p2"
                                                }
                                            }
                                        });
                                        if ws_tx.send(WsMessage::Text(identify.to_string().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                    0 => {
                                        let event_type = payload.get("t").and_then(Value::as_str).unwrap_or("");
                                        let d = payload.get("d").cloned().unwrap_or(Value::Null);
                                        match event_type {
                                            "READY" => {
                                                let user = d.get("user").cloned().unwrap_or(Value::Null);
                                                let uid = user.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                                                let uname = user.get("username").and_then(Value::as_str).unwrap_or("bot").to_owned();
                                                let disc = user.get("discriminator").and_then(Value::as_str).unwrap_or("0");
                                                let tag = if disc == "0" { uname } else { format!("{uname}#{disc}") };
                                                let avatar = user.get("avatar").and_then(Value::as_str).map(|hash| {
                                                    format!("https://cdn.discordapp.com/avatars/{uid}/{hash}.png?size=64")
                                                });
                                                let app_id = d
                                                    .get("application")
                                                    .and_then(|a| a.get("id"))
                                                    .and_then(Value::as_str)
                                                    .unwrap_or(&uid)
                                                    .to_owned();
                                                let mut guilds = Vec::new();
                                                if let Some(arr) = d.get("guilds").and_then(Value::as_array) {
                                                    let mut gnames = guild_names.lock().unwrap_or_else(|e| e.into_inner());
                                                    for g in arr {
                                                        if let Some(gid) = g.get("id").and_then(Value::as_str) {
                                                            let gname = g.get("name").and_then(Value::as_str).unwrap_or(gid).to_owned();
                                                            gnames.insert(gid.to_owned(), gname.clone());
                                                            guilds.push(Guild { id: gid.to_owned(), name: gname });
                                                        }
                                                    }
                                                }
                                                {
                                                    let mut st = loop_state.lock().unwrap_or_else(|e| e.into_inner());
                                                    st.user_id = uid.clone();
                                                    *inner.connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(st.clone());
                                                }
                                                let status = BotStatus::Connected {
                                                    user: tag,
                                                    application_id: app_id.clone(),
                                                    avatar_url: avatar,
                                                    invite_url: format!(
                                                        "https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot&permissions={PERMISSIONS}"
                                                    ),
                                                    guilds,
                                                };
                                                if inner.generation.load(Ordering::Acquire) == generation {
                                                    Self::set_status(&inner, status.clone());
                                                    (inner.on_event)(BotEvent::Ready);
                                                }
                                                if let Some(tx) = ready_tx.take() {
                                                    let _ = tx.send(Ok(status));
                                                }
                                            }
                                            "GUILD_CREATE" => {
                                                if let Some(gid) = d.get("id").and_then(Value::as_str) {
                                                    let gname = d.get("name").and_then(Value::as_str).unwrap_or(gid).to_owned();
                                                    let tier = d.get("premium_tier").and_then(Value::as_u64).unwrap_or(0);
                                                    guild_names.lock().unwrap_or_else(|e| e.into_inner()).insert(gid.to_owned(), gname.clone());
                                                    guild_tiers.lock().unwrap_or_else(|e| e.into_inner()).insert(gid.to_owned(), tier);
                                                    if let BotStatus::Connected { guilds, .. } = &mut *inner.status.lock().unwrap_or_else(|e| e.into_inner()) {
                                                        if let Some(existing) = guilds.iter_mut().find(|g| g.id == gid) {
                                                            existing.name = gname;
                                                        } else {
                                                            guilds.push(Guild { id: gid.to_owned(), name: gname });
                                                        }
                                                    }
                                                    let listeners = inner.listeners.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                                    for l in listeners { l(); }
                                                }
                                            }
                                            "GUILD_DELETE" => {
                                                if let Some(gid) = d.get("id").and_then(Value::as_str) {
                                                    guild_names.lock().unwrap_or_else(|e| e.into_inner()).remove(gid);
                                                    guild_tiers.lock().unwrap_or_else(|e| e.into_inner()).remove(gid);
                                                    if let BotStatus::Connected { guilds, .. } = &mut *inner.status.lock().unwrap_or_else(|e| e.into_inner()) {
                                                        guilds.retain(|g| g.id != gid);
                                                    }
                                                    let listeners = inner.listeners.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                                    for l in listeners { l(); }
                                                }
                                            }
                                            "MESSAGE_CREATE" => {
                                                if inner.generation.load(Ordering::Acquire) != generation {
                                                    continue;
                                                }
                                                let uid = loop_state.lock().unwrap_or_else(|e| e.into_inner()).user_id.clone();
                                                let author_id = d.get("author").and_then(|a| a.get("id")).and_then(Value::as_str).unwrap_or("");
                                                if author_id == uid {
                                                    continue;
                                                }
                                                let gid = d.get("guild_id").and_then(Value::as_str).unwrap_or("");
                                                let role_id = bot_roles.lock().unwrap_or_else(|e| e.into_inner()).get(gid).cloned();
                                                let msg = message_of(&d, &uid, role_id.as_deref());
                                                (inner.on_event)(BotEvent::Message(msg));
                                            }
                                            _ => {}
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            WsMessage::Close(frame) => {
                                let reason = frame.map(|f| format!("{} {}", f.code, f.reason)).unwrap_or_else(|| "closed".into());
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Err(Error::Invalid(reason)));
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .map_err(|_| Error::Invalid("Discord did not get ready".into()))?
            .map_err(|_| Error::Invalid("Discord gateway closed".into()))?
    }

    fn conn(&self) -> Result<ConnectionState> {
        self.inner
            .connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Error::Invalid("The Discord bot is not connected".into()))
    }

    async fn api_get(&self, path: &str) -> Result<Value> {
        let conn = self.conn()?;
        let resp = self
            .inner
            .client
            .get(format!("{}{path}", self.inner.api_base))
            .header("Authorization", format!("Bot {}", conn.token))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Error::Invalid(format!(
                "Discord HTTP {}",
                resp.status().as_u16()
            )));
        }
        resp.json().await.map_err(|e| Error::Invalid(e.to_string()))
    }

    pub async fn server(&self, guild_id: &str) -> Result<ServerDetails> {
        let channels_val = self
            .api_get(&format!("/guilds/{guild_id}/channels"))
            .await?;
        let roles_val = self.api_get(&format!("/guilds/{guild_id}/roles")).await?;

        let mut channels = Vec::new();
        if let Some(arr) = channels_val.as_array() {
            for ch in arr {
                let kind = ch.get("type").and_then(Value::as_u64).unwrap_or(99);
                if (kind == 0 || kind == 5)
                    && let (Some(id), Some(name)) = (
                        ch.get("id").and_then(Value::as_str),
                        ch.get("name").and_then(Value::as_str),
                    )
                {
                    channels.push(NamedId {
                        id: id.to_owned(),
                        name: name.to_owned(),
                    });
                }
            }
        }
        channels.sort_by(|a, b| a.name.cmp(&b.name));

        let mut roles = Vec::new();
        if let Some(arr) = roles_val.as_array() {
            for role in arr {
                let id = role.get("id").and_then(Value::as_str).unwrap_or("");
                let name = role.get("name").and_then(Value::as_str).unwrap_or("");
                let managed = role
                    .get("managed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !id.is_empty() && id != guild_id && !managed {
                    roles.push(NamedId {
                        id: id.to_owned(),
                        name: name.to_owned(),
                    });
                }
            }
        }
        roles.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(ServerDetails { channels, roles })
    }

    pub async fn close(&self) {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(conn) = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            conn.shutdown.notify_waiters();
        }
    }
}

impl DiscordPort for Bot {
    fn bot_id(&self) -> String {
        self.inner
            .connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.user_id.clone())
            .unwrap_or_default()
    }

    fn channel(&self, id: String) -> SyncFuture<DiscordChannel> {
        let bot = Bot {
            inner: self.inner.clone(),
        };
        Box::pin(async move {
            let conn = bot.conn()?;
            let ch = bot.api_get(&format!("/channels/{id}")).await?;
            let kind = ch.get("type").and_then(Value::as_u64).unwrap_or(0);
            if matches!(kind, 1 | 3) {
                return Err(Error::Invalid(format!("No text channel {id}")));
            }
            let guild_id = ch
                .get("guild_id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid(format!("No text channel {id}")))?
                .to_owned();
            let guild_name = conn
                .guild_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&guild_id)
                .cloned()
                .unwrap_or_else(|| guild_id.clone());
            let is_thread = matches!(kind, 10..=12);
            let thread = if is_thread {
                let parent_id = ch
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let created_at = ch
                    .get("thread_metadata")
                    .and_then(|m| m.get("create_timestamp"))
                    .and_then(Value::as_str)
                    .and_then(parse_iso_millis)
                    .unwrap_or_else(|| snowflake_timestamp_ms(&id));
                Some(DiscordThread {
                    parent_id,
                    created_at,
                    created_id: id.clone(),
                })
            } else {
                None
            };
            Ok(DiscordChannel {
                id: ch
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                name: ch
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                guild_id,
                guild_name,
                thread,
                last_message_id: ch
                    .get("last_message_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
    }

    fn messages(
        &self,
        channel_id: String,
        after: String,
        limit: usize,
    ) -> SyncFuture<Vec<DiscordMessage>> {
        let bot = Bot {
            inner: self.inner.clone(),
        };
        Box::pin(async move {
            let conn = bot.conn()?;
            let arr = bot
                .api_get(&format!(
                    "/channels/{channel_id}/messages?after={after}&limit={limit}"
                ))
                .await?;
            let mut list: Vec<Value> = arr.as_array().cloned().unwrap_or_default();
            list.sort_by_key(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u128>().ok())
                    .unwrap_or(0)
            });
            Ok(list
                .iter()
                .map(|m| message_of(m, &conn.user_id, None))
                .collect())
        })
    }

    fn reply(
        &self,
        channel_id: String,
        message_id: Option<String>,
        text: String,
        files: Vec<ReplyFile>,
    ) -> SyncFuture<()> {
        let bot = Bot {
            inner: self.inner.clone(),
        };
        Box::pin(async move {
            let conn = bot.conn()?;
            let mut body_json = serde_json::Map::new();
            if !text.is_empty() {
                body_json.insert("content".into(), json!(text));
            }
            if let Some(ref mid) = message_id {
                body_json.insert(
                    "message_reference".into(),
                    json!({ "message_id": mid, "fail_if_not_exists": false }),
                );
            }
            body_json.insert("allowed_mentions".into(), json!({ "parse": ["users"] }));

            let url = format!("{}/channels/{channel_id}/messages", bot.inner.api_base);
            let req = bot
                .inner
                .client
                .post(url)
                .header("Authorization", format!("Bot {}", conn.token))
                .timeout(Duration::from_secs(30));

            let resp = if files.is_empty() {
                req.json(&Value::Object(body_json))
                    .send()
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?
            } else {
                let boundary = format!("----p2discord{}", uuid::Uuid::new_v4().simple());
                let payload = build_multipart(&boundary, &Value::Object(body_json), &files);
                req.header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(payload)
                .send()
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?
            };
            if !resp.status().is_success() {
                return Err(Error::Invalid(format!(
                    "Discord reply HTTP {}",
                    resp.status().as_u16()
                )));
            }
            Ok(())
        })
    }

    fn upload_limit(&self, channel_id: String) -> SyncFuture<u64> {
        let bot = Bot {
            inner: self.inner.clone(),
        };
        Box::pin(async move {
            let conn = bot.conn()?;
            let ch = bot.api_get(&format!("/channels/{channel_id}")).await?;
            let tier = ch
                .get("guild_id")
                .and_then(Value::as_str)
                .and_then(|gid| {
                    conn.guild_tiers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(gid)
                        .copied()
                })
                .unwrap_or(0);
            let mb = match tier {
                3 => 100,
                2 => 50,
                _ => 10,
            };
            Ok(mb * 1024 * 1024)
        })
    }

    fn typing(&self, channel_id: String) -> SyncFuture<()> {
        let bot = Bot {
            inner: self.inner.clone(),
        };
        Box::pin(async move {
            let conn = bot.conn()?;
            let _ = bot
                .inner
                .client
                .post(format!(
                    "{}/channels/{channel_id}/typing",
                    bot.inner.api_base
                ))
                .header("Authorization", format!("Bot {}", conn.token))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            Ok(())
        })
    }
}

fn build_multipart(boundary: &str, payload_json: &Value, files: &[ReplyFile]) -> Vec<u8> {
    let mut out = Vec::new();
    let json_bytes = serde_json::to_vec(payload_json).unwrap_or_else(|_| b"{}".to_vec());
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(b"\r\n");
    for (i, file) in files.iter().enumerate() {
        let safe_name = file.name.replace('"', "_");
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files[{i}]\"; filename=\"{safe_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(&file.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

pub fn refusal(message: &str) -> Option<&'static str> {
    if message.contains("disallowed intents") || message.contains("4014") {
        return Some(
            "Discord refused the Message Content intent: turn on \"Message Content Intent\" under Bot in the Discord Developer Portal, then connect again.",
        );
    }
    if message.contains("invalid token")
        || message.contains("TokenInvalid")
        || message.contains("4004")
        || message.contains("401")
    {
        return Some("Discord refused the token.");
    }
    None
}

pub fn message_of(message: &Value, bot_id: &str, bot_role_id: Option<&str>) -> DiscordMessage {
    let mention_roles: Vec<&str> = message
        .get("mention_roles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let by_role = bot_role_id.is_some_and(|r| mention_roles.contains(&r));

    let raw_content = message.get("content").and_then(Value::as_str).unwrap_or("");
    let text = match bot_role_id {
        Some(role) if by_role => {
            raw_content.replace(&format!("<@&{role}>"), &format!("<@{bot_id}>"))
        }
        _ => raw_content.to_owned(),
    };

    let mentioned_user = message
        .get("mentions")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter()
                .any(|u| u.get("id").and_then(Value::as_str) == Some(bot_id))
        });

    let author_obj = message.get("author");
    let member_obj = message.get("member");
    let author_id = author_obj
        .and_then(|a| a.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let author_name = member_obj
        .and_then(|m| m.get("nick"))
        .and_then(Value::as_str)
        .or_else(|| {
            author_obj
                .and_then(|a| a.get("global_name"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            author_obj
                .and_then(|a| a.get("username"))
                .and_then(Value::as_str)
        })
        .unwrap_or("unknown")
        .to_owned();
    let author_bot = author_obj
        .and_then(|a| a.get("bot"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let roles = member_obj
        .and_then(|m| m.get("roles"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let attachments = message
        .get("attachments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|att| {
                    Some(DiscordAttachment {
                        name: att.get("filename").and_then(Value::as_str)?.to_owned(),
                        url: att.get("url").and_then(Value::as_str)?.to_owned(),
                        size: att.get("size").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    DiscordMessage {
        id: message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        channel_id: message
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        author: DiscordAuthor {
            id: author_id,
            name: author_name,
            bot: author_bot,
        },
        roles,
        text,
        mentions_bot: !bot_id.is_empty() && (by_role || mentioned_user),
        attachments,
    }
}

fn snowflake_timestamp_ms(id: &str) -> u64 {
    id.parse::<u64>()
        .map(|s| (s >> 22) + 1_420_070_400_000)
        .unwrap_or(0)
}

fn parse_iso_millis(_s: &str) -> Option<u64> {
    None
}
