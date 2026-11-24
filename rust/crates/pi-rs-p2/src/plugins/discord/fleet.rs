//! Fleet of Discord bots backed by `<data_dir>/discord.json` (0600).

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

use super::{
    bot::{Bot, BotEvent, DiscordPort},
    contract::{BotStatus, BotView},
};

pub const FIRST_BOT: &str = "bot1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotEntry {
    pub token: String,
    pub added_at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotsFile {
    pub bots: BTreeMap<String, BotEntry>,
}

#[derive(Clone, Debug)]
pub struct Received {
    pub bot: String,
    pub event: BotEvent,
}

pub struct Fleet {
    path: PathBuf,
    file: Mutex<BotsFile>,
    bots: Mutex<BTreeMap<String, Arc<Bot>>>,
    listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    queue: Mutex<VecDeque<Received>>,
    wake: Arc<Notify>,
}

impl Fleet {
    pub async fn open(data_dir: &Path) -> Arc<Self> {
        let path = data_dir.join("discord.json");
        let text = tokio::fs::read_to_string(&path).await.ok();
        let (file, migrated) = parse_bots(text.as_deref());
        let tokens: Vec<(String, String)> = file
            .bots
            .iter()
            .map(|(id, e)| (id.clone(), e.token.clone()))
            .collect();
        let fleet = Arc::new(Self {
            path,
            file: Mutex::new(file),
            bots: Mutex::new(BTreeMap::new()),
            listeners: Mutex::new(Vec::new()),
            queue: Mutex::new(VecDeque::new()),
            wake: Arc::new(Notify::new()),
        });
        if migrated {
            let _ = fleet.save().await;
        }
        for (id, token) in tokens {
            let bot = fleet.candidate(&id);
            fleet.adopt(&id, bot.clone());
            tokio::spawn(async move {
                let _ = bot.connect(token).await;
            });
        }
        fleet
    }

    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(listener));
    }

    fn changed(&self) {
        let listeners = self
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for l in listeners {
            l();
        }
    }

    pub fn views(&self) -> Vec<BotView> {
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let bots = self.bots.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<(&String, &BotEntry)> = file.bots.iter().collect();
        entries.sort_by_key(|(_, e)| e.added_at);
        entries
            .into_iter()
            .map(|(id, _)| BotView {
                id: id.clone(),
                status: bots
                    .get(id)
                    .map(|b| b.status())
                    .unwrap_or(BotStatus::Connecting),
            })
            .collect()
    }

    pub fn connected(&self, id: &str) -> Option<Arc<Bot>> {
        let bots = self.bots.lock().unwrap_or_else(|e| e.into_inner());
        let bot = bots.get(id)?;
        bot.is_connected().then(|| bot.clone())
    }

    pub fn connected_port(&self, id: &str) -> Option<Arc<dyn DiscordPort>> {
        self.connected(id).map(|b| b as Arc<dyn DiscordPort>)
    }

    pub async fn add(self: &Arc<Self>, token: String) -> Result<String> {
        let id = format!("b{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let bot = self.candidate(&id);
        let status = bot.connect(token.clone()).await;
        let (app_id, user) = match &status {
            BotStatus::Connected {
                application_id,
                user,
                ..
            } => (application_id.clone(), user.clone()),
            BotStatus::Error { error } => {
                bot.close().await;
                return Err(Error::Invalid(error.clone()));
            }
            BotStatus::Connecting => {
                bot.close().await;
                return Err(Error::Invalid("Discord did not answer".into()));
            }
        };
        if self.views().iter().any(|v| {
            matches!(&v.status, BotStatus::Connected { application_id, .. } if application_id == &app_id)
        }) {
            bot.close().await;
            return Err(Error::Invalid(format!("{user} is added already")));
        }
        self.adopt(&id, bot);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bots
            .insert(
                id.clone(),
                BotEntry {
                    token,
                    added_at: now,
                },
            );
        self.save().await?;
        self.changed();
        Ok(id)
    }

    pub async fn set_token(self: &Arc<Self>, id: &str, token: String) -> Result<()> {
        let (current, added_at) = {
            let bots = self.bots.lock().unwrap_or_else(|e| e.into_inner());
            let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
            let Some(bot) = bots.get(id).cloned() else {
                return Err(Error::Invalid("No such bot".into()));
            };
            let Some(entry) = file.bots.get(id) else {
                return Err(Error::Invalid("No such bot".into()));
            };
            (bot, entry.added_at)
        };
        let bot = self.candidate(id);
        let status = bot.connect(token.clone()).await;
        let (new_app, new_user) = match &status {
            BotStatus::Connected {
                application_id,
                user,
                ..
            } => (application_id.clone(), user.clone()),
            BotStatus::Error { error } => {
                bot.close().await;
                return Err(Error::Invalid(error.clone()));
            }
            BotStatus::Connecting => {
                bot.close().await;
                return Err(Error::Invalid("Discord did not answer".into()));
            }
        };
        if let BotStatus::Connected { application_id, .. } = current.status()
            && application_id != new_app
        {
            bot.close().await;
            return Err(Error::Invalid(format!(
                "This token is for {new_user}, another bot: add it as a new bot"
            )));
        }
        current.close().await;
        self.adopt(id, bot);
        self.file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bots
            .insert(id.to_owned(), BotEntry { token, added_at });
        self.save().await?;
        self.changed();
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        let existing = self
            .bots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(bot) = existing {
            bot.close().await;
        }
        self.file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bots
            .remove(id);
        self.save().await?;
        self.changed();
        Ok(())
    }

    pub async fn next(&self) -> Received {
        loop {
            if let Some(item) = self
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
            {
                return item;
            }
            self.wake.notified().await;
        }
    }

    pub async fn close(&self) {
        let bots: Vec<Arc<Bot>> = {
            let mut map = self.bots.lock().unwrap_or_else(|e| e.into_inner());
            let vals: Vec<_> = map.values().cloned().collect();
            map.clear();
            vals
        };
        for bot in bots {
            bot.close().await;
        }
    }

    fn candidate(self: &Arc<Self>, id: &str) -> Arc<Bot> {
        let fleet = Arc::downgrade(self);
        let bot_id = id.to_owned();
        let holder: Arc<Mutex<Option<Arc<Bot>>>> = Arc::default();
        let holder_cb = holder.clone();
        let bot = Arc::new(Bot::new(move |event| {
            let Some(fleet) = fleet.upgrade() else {
                return;
            };
            let matches = {
                let bots = fleet.bots.lock().unwrap_or_else(|e| e.into_inner());
                let own = holder_cb.lock().unwrap_or_else(|e| e.into_inner());
                own.as_ref()
                    .is_some_and(|o| bots.get(&bot_id).is_some_and(|b| Arc::ptr_eq(b, o)))
            };
            if matches {
                fleet
                    .queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(Received {
                        bot: bot_id.clone(),
                        event,
                    });
                fleet.wake.notify_one();
            }
        }));
        *holder.lock().unwrap_or_else(|e| e.into_inner()) = Some(bot.clone());
        bot
    }

    fn adopt(self: &Arc<Self>, id: &str, bot: Arc<Bot>) {
        self.bots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), bot.clone());
        let fleet = Arc::downgrade(self);
        bot.subscribe(move || {
            if let Some(f) = fleet.upgrade() {
                f.changed();
            }
        });
        if bot.is_connected() {
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Received {
                    bot: id.to_owned(),
                    event: BotEvent::Ready,
                });
            self.wake.notify_one();
        }
    }

    async fn save(&self) -> Result<()> {
        let content = {
            let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
            format!("{}\n", serde_json::to_string_pretty(&*file)?)
        };
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, content).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
        }
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

/// Parses `discord.json`, migrating legacy `{ "token": "..." }` into `bot1`.
pub fn parse_bots(text: Option<&str>) -> (BotsFile, bool) {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return (BotsFile::default(), false);
    };
    let Ok(val) = serde_json::from_str::<Value>(text) else {
        eprintln!("p2 discord: discord.json is not valid JSON; no bots run");
        return (BotsFile::default(), false);
    };
    let Some(obj) = val.as_object() else {
        return (BotsFile::default(), false);
    };
    if let Some(token) = obj.get("token").and_then(Value::as_str)
        && !token.is_empty()
    {
        let mut bots = BTreeMap::new();
        bots.insert(
            FIRST_BOT.into(),
            BotEntry {
                token: token.to_owned(),
                added_at: 0,
            },
        );
        return (BotsFile { bots }, true);
    }
    let mut bots = BTreeMap::new();
    if let Some(listed) = obj.get("bots").and_then(Value::as_object) {
        for (id, entry) in listed {
            if let Some(eobj) = entry.as_object()
                && let Some(token) = eobj.get("token").and_then(Value::as_str)
                && !token.is_empty()
            {
                let added_at = eobj.get("addedAt").and_then(Value::as_u64).unwrap_or(0);
                bots.insert(
                    id.clone(),
                    BotEntry {
                        token: token.to_owned(),
                        added_at,
                    },
                );
            }
        }
    }
    (BotsFile { bots }, false)
}
