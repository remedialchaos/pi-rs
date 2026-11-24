//! Channel/thread message relay, wake rules, batching, chunking, and pending-reply tracking.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result, sync::SyncFuture};

use crate::runtime::session_lock::SessionLock;

use super::{
    bot::{DiscordChannel, DiscordMessage, DiscordPort, ReplyFile},
    contract::{
        Binding, COMMANDS, Command, DiscordFile, OfferedFile, Pending, ThreadBinding, TurnState,
        Wake,
    },
};

pub const PAGE: usize = 100;
pub const BATCH_CAP: usize = 200;
pub const MESSAGE_LIMIT: usize = 2000;
pub const MAX_ATTACHMENT: u64 = 25 * 1024 * 1024;
pub const MAX_FILES: usize = 20;
pub const PENDING_TIMEOUT_MS: u64 = 6 * 60 * 60 * 1000;
pub const MAX_BOT_TURNS: usize = 3;
pub const REMEMBERED_ANSWERS: usize = 200;
pub const READ_CHUNK: usize = 4 * 1024 * 1024;
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 10;

#[derive(Clone, Debug, Default)]
pub struct RelayState {
    pub bindings: BTreeMap<String, Binding>,
    pub pending: BTreeMap<String, Pending>,
}

#[derive(Clone, Debug)]
pub struct DeliverContext {
    pub channel: String,
    pub guild: String,
    pub messages: usize,
    pub thread: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DeliverInput {
    pub conversation: Option<u64>,
    pub request_id: String,
    pub author_id: String,
    pub author_name: String,
    pub text: String,
    pub context: DeliverContext,
    pub files: Vec<DiscordFile>,
    pub lock: Option<SessionLock>,
}

#[derive(Clone, Debug)]
pub struct DeliverResult {
    pub conversation: u64,
    pub submission_id: u64,
}

pub trait SessionPort: Send + Sync {
    fn usable(&self, session: String) -> SyncFuture<Option<String>>;
    fn lock(&self, session: String) -> Option<SessionLock>;
    fn deliver(&self, session: String, input: DeliverInput) -> SyncFuture<DeliverResult>;
    fn turn(&self, session: String, submission_id: u64) -> SyncFuture<TurnState>;
    fn thread(&self, session: String, name: String, before: u64) -> SyncFuture<u64>;
    fn read(&self, session: String, id: String, offset: u64, length: usize) -> SyncFuture<Vec<u8>>;
    fn command(
        &self,
        session: String,
        conversation: Option<u64>,
        command: Command,
    ) -> SyncFuture<String>;
}

pub trait StatePort: Send + Sync {
    fn value(&self) -> RelayState;
    fn change(&self, update: Box<dyn FnOnce(&mut RelayState) + Send>) -> SyncFuture<()>;
}

#[derive(Default)]
struct BotStreak {
    turns: usize,
    noticed: bool,
}

pub type DiscordLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn DiscordPort>> + Send + Sync>;

pub struct Relay {
    discord: DiscordLookup,
    sessions: Arc<dyn SessionPort>,
    state: Arc<dyn StatePort>,
    checking: Mutex<BTreeSet<String>>,
    lanes: tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    delivering: Mutex<BTreeMap<String, (String, usize)>>,
    bot_streaks: Mutex<BTreeMap<String, BotStreak>>,
    posted: Mutex<VecDeque<String>>,
}

impl Relay {
    pub fn new(
        discord: impl Fn(&str) -> Option<Arc<dyn DiscordPort>> + Send + Sync + 'static,
        sessions: Arc<dyn SessionPort>,
        state: Arc<dyn StatePort>,
    ) -> Self {
        Self {
            discord: Arc::new(discord),
            sessions,
            state,
            checking: Mutex::default(),
            lanes: tokio::sync::Mutex::default(),
            delivering: Mutex::default(),
            bot_streaks: Mutex::default(),
            posted: Mutex::default(),
        }
    }

    fn port(&self, bot: &str) -> Result<Arc<dyn DiscordPort>> {
        (self.discord)(bot).ok_or_else(|| Error::Invalid("The Discord bot is not connected".into()))
    }

    pub async fn on_message(&self, bot: &str, message: DiscordMessage) -> Result<()> {
        let port = self.port(bot)?;
        let bot_id = port.bot_id();
        if message.author.id == bot_id {
            return Ok(());
        }
        if !message.author.bot {
            self.bot_streaks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&message.channel_id);
        }
        if !message.mentions_bot {
            return Ok(());
        }
        let bindings = self.state.value().bindings;
        let (binding, thread) = match bindings.get(&message.channel_id) {
            Some(b) => (b.clone(), None),
            None => {
                let ch = port.channel(message.channel_id.clone()).await?;
                let Some(ref th) = ch.thread else {
                    return Ok(());
                };
                let Some(b) = bindings.get(&th.parent_id) else {
                    return Ok(());
                };
                (b.clone(), Some(ch))
            }
        };
        if binding.bot_id != bot || !may_wake(&binding.wake, &message.author.id, &message.roles) {
            return Ok(());
        }
        if let Some(cmd) = command_of(&message.text, &bot_id) {
            self.run_command(&binding, thread.as_ref(), &message, cmd)
                .await
        } else if self.bot_turn_allowed(&port, &message) {
            self.deliver(&binding.channel_id, message, thread).await
        } else {
            Ok(())
        }
    }

    fn bot_turn_allowed(&self, port: &Arc<dyn DiscordPort>, message: &DiscordMessage) -> bool {
        if !message.author.bot {
            return true;
        }
        let target = &message.channel_id;
        let notice = {
            let mut streaks = self.bot_streaks.lock().unwrap_or_else(|e| e.into_inner());
            let streak = streaks.entry(target.clone()).or_default();
            if streak.turns < MAX_BOT_TURNS {
                streak.turns += 1;
                return true;
            }
            eprintln!(
                "p2 discord: not answering {} in {target}: {MAX_BOT_TURNS} bot-started turns in a row",
                message.author.name
            );
            if streak.noticed {
                false
            } else {
                streak.noticed = true;
                true
            }
        };
        if notice {
            let port = port.clone();
            let target = target.clone();
            let msg_id = message.id.clone();
            tokio::spawn(async move {
                let _ = port
                    .reply(
                        target,
                        Some(msg_id),
                        format!(
                            "Not answering more bots here until a person writes ({MAX_BOT_TURNS} bot turns in a row)."
                        ),
                        Vec::new(),
                    )
                    .await;
            });
        }
        false
    }

    async fn run_command(
        &self,
        binding: &Binding,
        thread: Option<&DiscordChannel>,
        message: &DiscordMessage,
        cmd: Command,
    ) -> Result<()> {
        let port = self.port(&binding.bot_id)?;
        let where_id = thread
            .map(|t| t.id.clone())
            .unwrap_or_else(|| binding.channel_id.clone());
        if let Some(refusal) = self.sessions.usable(binding.session.clone()).await? {
            return port
                .reply(
                    where_id,
                    Some(message.id.clone()),
                    format!("Pi is not listening here right now: {refusal}"),
                    Vec::new(),
                )
                .await;
        }
        let conversation =
            thread.and_then(|t| binding.threads.get(&t.id).map(|tb| tb.conversation));
        if thread.is_some() && conversation.is_none() {
            let text = if cmd == Command::Stop {
                "Nothing is running."
            } else {
                "This thread has no agent yet: mention the bot to start one."
            };
            return port
                .reply(where_id, Some(message.id.clone()), text.into(), Vec::new())
                .await;
        }
        let text = match self
            .sessions
            .command(binding.session.clone(), conversation, cmd)
            .await
        {
            Ok(t) => t,
            Err(err) => format!("Could not reach Pi: {err}"),
        };
        port.reply(where_id, Some(message.id.clone()), text, Vec::new())
            .await
    }

    fn is_command(binding: &Binding, message: &DiscordMessage, bot_id: &str) -> bool {
        message.mentions_bot
            && may_wake(&binding.wake, &message.author.id, &message.roles)
            && command_of(&message.text, bot_id).is_some()
    }

    pub async fn deliver(
        &self,
        channel_id: &str,
        trigger: DiscordMessage,
        thread: Option<DiscordChannel>,
    ) -> Result<()> {
        let target = thread
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_else(|| channel_id.to_owned());
        let start_typing = self.begin_typing(channel_id, &target, &trigger);
        let lane = {
            let mut lanes = self.lanes.lock().await;
            lanes
                .entry(target.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lane.lock().await;
        let res = self
            .deliver_inner(channel_id, &trigger, thread.as_ref())
            .await;
        if start_typing {
            self.end_typing(&target);
        }
        res
    }

    fn begin_typing(&self, channel_id: &str, target: &str, trigger: &DiscordMessage) -> bool {
        let bindings = self.state.value().bindings;
        let Some(binding) = bindings.get(channel_id) else {
            return false;
        };
        let cursor = if target == channel_id {
            binding.cursor.as_deref()
        } else {
            binding
                .threads
                .get(target)
                .and_then(|t| t.cursor.as_deref())
        };
        if let Some(c) = cursor
            && snowflake_num(&trigger.id) <= snowflake_num(c)
        {
            return false;
        }
        let bot = binding.bot_id.clone();
        {
            let mut del = self.delivering.lock().unwrap_or_else(|e| e.into_inner());
            let entry = del
                .entry(target.to_owned())
                .or_insert_with(|| (bot.clone(), 0));
            entry.1 += 1;
        }
        if let Some(port) = (self.discord)(&bot) {
            let target = target.to_owned();
            tokio::spawn(async move {
                let _ = port.typing(target).await;
            });
        }
        true
    }

    fn end_typing(&self, target: &str) {
        let mut del = self.delivering.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = del.get_mut(target) {
            entry.1 = entry.1.saturating_sub(1);
            if entry.1 == 0 {
                del.remove(target);
            }
        }
    }

    async fn deliver_inner(
        &self,
        channel_id: &str,
        trigger: &DiscordMessage,
        thread: Option<&DiscordChannel>,
    ) -> Result<()> {
        let Some(binding) = self.state.value().bindings.get(channel_id).cloned() else {
            return Ok(());
        };
        let port = self.port(&binding.bot_id)?;
        let bot_id = port.bot_id();
        let where_id = thread
            .map(|t| t.id.clone())
            .unwrap_or_else(|| channel_id.to_owned());
        if let Some(refusal) = self.sessions.usable(binding.session.clone()).await? {
            return port
                .reply(
                    where_id,
                    Some(trigger.id.clone()),
                    format!("Pi is not listening here right now: {refusal}"),
                    Vec::new(),
                )
                .await;
        }

        let mut conversation = None;
        let mut cursor = binding.cursor.clone();
        if let Some(th) = thread {
            let known = match binding.threads.get(&th.id).cloned() {
                Some(k) => k,
                None => {
                    let created_at = th
                        .thread
                        .as_ref()
                        .map(|t| t.created_at)
                        .unwrap_or_else(now_ms);
                    let conv = self
                        .sessions
                        .thread(binding.session.clone(), th.name.clone(), created_at)
                        .await?;
                    let value = ThreadBinding {
                        conversation: conv,
                        name: th.name.clone(),
                        cursor: None,
                    };
                    let tid = th.id.clone();
                    let cid = channel_id.to_owned();
                    let vclone = value.clone();
                    self.state
                        .change(Box::new(move |draft| {
                            if let Some(b) = draft.bindings.get_mut(&cid) {
                                b.threads.insert(tid, vclone);
                            }
                        }))
                        .await?;
                    value
                }
            };
            conversation = Some(known.conversation);
            cursor = known.cursor;
        }

        if let Some(ref c) = cursor
            && snowflake_num(&trigger.id) <= snowflake_num(c)
        {
            return Ok(());
        }

        let mut window = BatchWindow::new(BATCH_CAP);
        let before = thread
            .and_then(|t| t.thread.as_ref())
            .filter(|_| cursor.is_none())
            .map(|t| t.created_id.clone());

        if let Some(ref until) = before {
            for message in
                history(&port, channel_id, binding.cursor.as_deref(), Some(until)).await?
            {
                if message.author.id != bot_id && !Self::is_command(&binding, &message, &bot_id) {
                    window.add(Part::Channel, message);
                }
            }
        }
        for message in history(&port, &where_id, cursor.as_deref(), Some(&trigger.id)).await? {
            if message.author.id != bot_id && !Self::is_command(&binding, &message, &bot_id) {
                window.add(Part::Here, message);
            }
        }
        if !window.has(&trigger.id) {
            window.add(Part::Here, trigger.clone());
        }

        let plural = |count: usize| format!("{count} message{}", if count == 1 { "" } else { "s" });
        let left = if window.omitted() > 0 {
            format!(
                "; the {} earliest are left out, the newest {} follow",
                window.omitted(),
                window.kept()
            )
        } else {
            String::new()
        };
        let channel_name = format!("#{}", binding.channel_name);
        let header = match thread {
            None => format!(
                "[Discord {channel_name} ({}): {} since the last delivery{left}]",
                binding.guild_name,
                plural(window.total())
            ),
            Some(th) if before.is_none() => format!(
                "[Discord thread \"{}\" in {channel_name} ({}): {} since the last delivery{left}]",
                th.name,
                binding.guild_name,
                plural(window.total())
            ),
            Some(th) => {
                let ch_count = window.count(Part::Channel);
                let ch_prefix = if ch_count > 0 {
                    format!(
                        "{} of {channel_name} before the thread, then ",
                        plural(ch_count)
                    )
                } else {
                    String::new()
                };
                format!(
                    "[Discord thread \"{}\" in {channel_name} ({}), its first delivery: {ch_prefix}{} of the thread{left}]",
                    th.name,
                    binding.guild_name,
                    plural(window.count(Part::Here))
                )
            }
        };

        let here_title = if before.is_some() && window.count(Part::Channel) > 0 {
            Some("[The thread]")
        } else {
            None
        };
        let before_title = format!("[{channel_name}, before the thread]");
        let sections = window.sections(Some(&before_title), here_title);
        let batch = batch_of(&sections, &bot_id, &header);

        let delivered = match self
            .sessions
            .deliver(
                binding.session.clone(),
                DeliverInput {
                    conversation,
                    request_id: format!("discord:{}", trigger.id),
                    author_id: trigger.author.id.clone(),
                    author_name: trigger.author.name.clone(),
                    text: batch.text,
                    context: DeliverContext {
                        channel: binding.channel_name.clone(),
                        guild: binding.guild_name.clone(),
                        messages: window.total(),
                        thread: thread.map(|t| t.name.clone()),
                    },
                    files: batch.files,
                    lock: self.sessions.lock(binding.session.clone()),
                },
            )
            .await
        {
            Ok(d) => d,
            Err(err) => {
                port.reply(
                    where_id,
                    Some(trigger.id.clone()),
                    format!("Could not reach Pi: {err}"),
                    Vec::new(),
                )
                .await?;
                return Ok(());
            }
        };

        let cid = channel_id.to_owned();
        let tid = thread.map(|t| t.id.clone());
        let trigger_id = trigger.id.clone();
        let bot_id_owned = binding.bot_id.clone();
        let session_owned = binding.session.clone();
        let where_owned = where_id;
        let sub_id = delivered.submission_id;
        let now = now_ms();
        self.state
            .change(Box::new(move |draft| {
                let Some(current) = draft.bindings.get_mut(&cid) else {
                    return;
                };
                if let Some(ref th_id) = tid {
                    if let Some(known) = current.threads.get_mut(th_id) {
                        known.cursor = Some(trigger_id.clone());
                    }
                } else {
                    current.cursor = Some(trigger_id.clone());
                }
                draft.pending.insert(
                    trigger_id.clone(),
                    Pending {
                        bot_id: bot_id_owned,
                        session: session_owned,
                        channel_id: cid,
                        reply_channel_id: where_owned,
                        message_id: trigger_id,
                        submission_id: sub_id,
                        since: now,
                    },
                );
            }))
            .await
    }

    pub async fn catch_up(&self, bot: &str) -> Result<()> {
        let port = self.port(bot)?;
        let bot_id = port.bot_id();
        let bindings: Vec<Binding> = self.state.value().bindings.into_values().collect();
        for binding in bindings {
            if binding.bot_id != bot {
                continue;
            }
            let mut places: Vec<(String, Option<String>, bool)> =
                vec![(binding.channel_id.clone(), binding.cursor.clone(), false)];
            for (tid, known) in &binding.threads {
                places.push((tid.clone(), known.cursor.clone(), true));
            }
            for (place_id, cursor, is_thread) in places {
                let res: Result<()> = async {
                    let mut newest: Option<DiscordMessage> = None;
                    for message in history(&port, &place_id, cursor.as_deref(), None).await? {
                        if message.author.id == bot_id {
                            continue;
                        }
                        if !message.author.bot {
                            self.bot_streaks
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&place_id);
                        }
                        if message.mentions_bot
                            && may_wake(&binding.wake, &message.author.id, &message.roles)
                            && !Self::is_command(&binding, &message, &bot_id)
                        {
                            newest = Some(message);
                        }
                    }
                    let Some(newest) = newest else {
                        return Ok(());
                    };
                    if !self.bot_turn_allowed(&port, &newest) {
                        return Ok(());
                    }
                    let thread = if is_thread {
                        Some(port.channel(place_id).await?)
                    } else {
                        None
                    };
                    self.deliver(&binding.channel_id, newest, thread).await
                }
                .await;
                if let Err(err) = res {
                    eprintln!("p2 discord: catching up failed: {err}");
                }
            }
        }
        Ok(())
    }

    pub async fn check_pending(&self, sessions: Option<&BTreeSet<String>>) -> Result<()> {
        let now = now_ms();
        let pending_list: Vec<Pending> = self.state.value().pending.into_values().collect();
        let mut settled: Vec<(Pending, TurnState)> = Vec::new();

        for pending in pending_list {
            if let Some(filter) = sessions
                && !filter.contains(&pending.session)
            {
                continue;
            }
            if (self.discord)(&pending.bot_id).is_none()
                && now.saturating_sub(pending.since) < PENDING_TIMEOUT_MS
            {
                continue;
            }
            {
                let mut checking = self.checking.lock().unwrap_or_else(|e| e.into_inner());
                if !checking.insert(pending.message_id.clone()) {
                    continue;
                }
            }
            if let Some(turn) = self.settled(&pending, now).await {
                settled.push((pending, turn));
            } else {
                self.checking
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&pending.message_id);
            }
        }

        let mut runs: BTreeMap<String, Vec<(Pending, TurnState)>> = BTreeMap::new();
        for (pending, turn) in settled {
            let key = match &turn {
                TurnState::Answered { answer, .. } => format!("{}:{answer}", pending.session),
                _ => pending.message_id.clone(),
            };
            runs.entry(key).or_default().push((pending, turn));
        }

        for (key, run) in runs {
            let ids: Vec<String> = run.iter().map(|(p, _)| p.message_id.clone()).collect();
            self.post_run(&key, run).await;
            let mut checking = self.checking.lock().unwrap_or_else(|e| e.into_inner());
            for id in ids {
                checking.remove(&id);
            }
        }
        Ok(())
    }

    async fn settled(&self, pending: &Pending, now: u64) -> Option<TurnState> {
        let turn = match self
            .sessions
            .turn(pending.session.clone(), pending.submission_id)
            .await
        {
            Ok(t) => t,
            Err(err) => {
                eprintln!("p2 discord: reading a turn failed: {err}");
                if now.saturating_sub(pending.since) < PENDING_TIMEOUT_MS {
                    return None;
                }
                return Some(TurnState::Unanswered {
                    reason: "the session did not answer in time.".into(),
                    files: Vec::new(),
                });
            }
        };
        if !matches!(turn, TurnState::Pending) {
            return Some(turn);
        }
        if now.saturating_sub(pending.since) < PENDING_TIMEOUT_MS {
            None
        } else {
            Some(TurnState::Unanswered {
                reason: "the session did not answer in time.".into(),
                files: Vec::new(),
            })
        }
    }

    async fn post_run(&self, key: &str, mut run: Vec<(Pending, TurnState)>) {
        run.sort_by_key(|(p, _)| snowflake_num(&p.message_id));
        if let Some((newest_pending, newest_turn)) = run.last()
            && matches!(
                newest_turn,
                TurnState::Answered { .. } | TurnState::Unanswered { .. }
            )
            && !self
                .posted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|k| k == key)
        {
            let mut files_map: BTreeMap<String, OfferedFile> = BTreeMap::new();
            for (_, t) in &run {
                match t {
                    TurnState::Answered { files, .. } | TurnState::Unanswered { files, .. } => {
                        for f in files {
                            files_map.insert(f.id.clone(), f.clone());
                        }
                    }
                    _ => {}
                }
            }
            let merged_files: Vec<OfferedFile> = files_map.into_values().collect();
            let post_res: Result<()> = async {
                let port = self.port(&newest_pending.bot_id)?;
                self.send_reply(&port, newest_pending, newest_turn, &merged_files)
                    .await
            }
            .await;
            if let Err(err) = post_res {
                eprintln!("p2 discord: posting a reply failed: {err}");
            } else if matches!(newest_turn, TurnState::Answered { .. }) {
                let mut posted = self.posted.lock().unwrap_or_else(|e| e.into_inner());
                posted.push_back(key.to_owned());
                if posted.len() > REMEMBERED_ANSWERS {
                    posted.pop_front();
                }
            }
        }
        let to_remove: Vec<String> = run.into_iter().map(|(p, _)| p.message_id).collect();
        let _ = self
            .state
            .change(Box::new(move |draft| {
                for id in to_remove {
                    draft.pending.remove(&id);
                }
            }))
            .await;
    }

    async fn send_reply(
        &self,
        port: &Arc<dyn DiscordPort>,
        pending: &Pending,
        turn: &TurnState,
        files: &[OfferedFile],
    ) -> Result<()> {
        let mut text = match turn {
            TurnState::Answered { text, .. } => text.clone(),
            TurnState::Unanswered { reason, .. } => format!("(No answer: {reason})"),
            _ => return Ok(()),
        };
        let limit = if files.is_empty() {
            0
        } else {
            port.upload_limit(pending.reply_channel_id.clone()).await?
        };
        let too_large: Vec<&OfferedFile> = files.iter().filter(|f| f.size > limit).collect();
        if !too_large.is_empty() {
            let names: Vec<String> = too_large
                .iter()
                .map(|f| format!("{} ({})", f.name, megabytes(f.size)))
                .collect();
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("Too large for Discord: ");
            text.push_str(&names.join(", "));
        }
        let fitting: Vec<OfferedFile> = files.iter().filter(|f| f.size <= limit).cloned().collect();
        let groups = attachment_groups(&fitting, limit);

        let mut posts: Vec<(String, Vec<OfferedFile>)> = chunk_text(&text, MESSAGE_LIMIT)
            .into_iter()
            .map(|c| (c, Vec::new()))
            .collect();
        for (i, group) in groups.into_iter().enumerate() {
            if i == 0
                && let Some(last) = posts.last_mut()
            {
                last.1 = group;
            } else {
                posts.push((String::new(), group));
            }
        }
        for (i, (post_text, post_files)) in posts.into_iter().enumerate() {
            let mut reply_files = Vec::new();
            for file in post_files {
                match self.read_file(&pending.session, &file).await {
                    Ok(data) => reply_files.push(ReplyFile {
                        name: file.name,
                        data,
                    }),
                    Err(err) => {
                        eprintln!("p2 discord: reading {} failed: {err}", file.name);
                    }
                }
            }
            if post_text.is_empty() && reply_files.is_empty() {
                continue;
            }
            let reply_to = (i == 0).then(|| pending.message_id.clone());
            port.reply(
                pending.reply_channel_id.clone(),
                reply_to,
                post_text,
                reply_files,
            )
            .await?;
        }
        Ok(())
    }

    async fn read_file(&self, session: &str, file: &OfferedFile) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(file.size as usize);
        let mut offset = 0u64;
        while offset < file.size {
            let len = READ_CHUNK.min((file.size - offset) as usize);
            let part = self
                .sessions
                .read(session.to_owned(), file.id.clone(), offset, len)
                .await?;
            if part.is_empty() {
                break;
            }
            offset += part.len() as u64;
            out.extend_from_slice(&part);
        }
        Ok(out)
    }

    pub async fn typing(&self) {
        let mut channels: BTreeMap<String, String> = BTreeMap::new();
        {
            let del = self.delivering.lock().unwrap_or_else(|e| e.into_inner());
            for (target, (bot, _)) in &*del {
                channels.insert(target.clone(), bot.clone());
            }
        }
        for pending in self.state.value().pending.values() {
            channels.insert(pending.reply_channel_id.clone(), pending.bot_id.clone());
        }
        for (channel, bot) in channels {
            if let Some(port) = (self.discord)(&bot) {
                let _ = port.typing(channel).await;
            }
        }
    }
}

async fn history(
    port: &Arc<dyn DiscordPort>,
    channel: &str,
    cursor: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<DiscordMessage>> {
    let mut after = cursor.unwrap_or("0").to_owned();
    let mut out = Vec::new();
    let until_num = until.map(snowflake_num);
    loop {
        let page = port
            .messages(channel.to_owned(), after.clone(), PAGE)
            .await?;
        let count = page.len();
        let last_id = page.last().map(|m| m.id.clone());
        for message in page {
            if let Some(u) = until_num
                && snowflake_num(&message.id) > u
            {
                return Ok(out);
            }
            out.push(message);
        }
        if count < PAGE {
            break;
        }
        let Some(next) = last_id else {
            break;
        };
        after = next;
    }
    Ok(out)
}

pub fn command_of(text: &str, bot_id: &str) -> Option<Command> {
    let stripped = text
        .replace(&format!("<@{bot_id}>"), "")
        .replace(&format!("<@!{bot_id}>"), "");
    let trimmed = stripped.trim().to_lowercase();
    let rest = trimmed.strip_prefix('/').unwrap_or(&trimmed);
    COMMANDS.into_iter().find(|cmd| {
        let name = match cmd {
            Command::Stop => "stop",
            Command::Status => "status",
            Command::Compact => "compact",
        };
        name == rest
    })
}

pub fn attachment_groups(files: &[OfferedFile], limit: u64) -> Vec<Vec<OfferedFile>> {
    let mut groups: Vec<Vec<OfferedFile>> = Vec::new();
    let mut bytes = 0u64;
    for file in files {
        let fits = groups
            .last()
            .is_some_and(|g| g.len() < MAX_ATTACHMENTS_PER_MESSAGE && bytes + file.size <= limit);
        if fits && let Some(last) = groups.last_mut() {
            last.push(file.clone());
            bytes += file.size;
        } else {
            groups.push(vec![file.clone()]);
            bytes = file.size;
        }
    }
    groups
}

fn megabytes(size: u64) -> String {
    format!("{:.1} MiB", size as f64 / (1024.0 * 1024.0))
}

pub fn may_wake(wake: &Wake, author: &str, roles: &[String]) -> bool {
    wake.anyone
        || wake.users.iter().any(|u| u == author)
        || roles.iter().any(|r| wake.roles.iter().any(|wr| wr == r))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Channel,
    Here,
}

struct BatchWindow {
    cap: usize,
    entries: VecDeque<(Part, DiscordMessage)>,
    channel_count: usize,
    here_count: usize,
}

struct BatchSection<'a> {
    title: Option<&'a str>,
    messages: Vec<DiscordMessage>,
}

impl BatchWindow {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: VecDeque::new(),
            channel_count: 0,
            here_count: 0,
        }
    }

    fn add(&mut self, part: Part, message: DiscordMessage) {
        match part {
            Part::Channel => self.channel_count += 1,
            Part::Here => self.here_count += 1,
        }
        self.entries.push_back((part, message));
        if self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    fn has(&self, id: &str) -> bool {
        self.entries.iter().any(|(_, m)| m.id == id)
    }

    fn count(&self, part: Part) -> usize {
        match part {
            Part::Channel => self.channel_count,
            Part::Here => self.here_count,
        }
    }

    fn total(&self) -> usize {
        self.channel_count + self.here_count
    }

    fn kept(&self) -> usize {
        self.entries.len()
    }

    fn omitted(&self) -> usize {
        self.total().saturating_sub(self.kept())
    }

    fn sections<'a>(
        &self,
        channel_title: Option<&'a str>,
        here_title: Option<&'a str>,
    ) -> Vec<BatchSection<'a>> {
        let mut grouped: Vec<(Part, Vec<DiscordMessage>)> = Vec::new();
        for (part, msg) in &self.entries {
            if let Some((last_part, list)) = grouped.last_mut()
                && last_part == part
            {
                list.push(msg.clone());
            } else {
                grouped.push((*part, vec![msg.clone()]));
            }
        }
        grouped
            .into_iter()
            .map(|(part, messages)| BatchSection {
                title: match part {
                    Part::Channel => channel_title,
                    Part::Here => here_title,
                },
                messages,
            })
            .collect()
    }
}

pub struct FormattedBatch {
    pub text: String,
    pub files: Vec<DiscordFile>,
}

fn batch_of(sections: &[BatchSection<'_>], bot_id: &str, header: &str) -> FormattedBatch {
    let mut files = Vec::new();
    let mut lines = vec![header.to_owned()];

    for section in sections {
        if let Some(title) = section.title {
            lines.push(title.to_owned());
        }
        for message in &section.messages {
            let text = message
                .text
                .replace(&format!("<@{bot_id}>"), "@Pi")
                .replace(&format!("<@!{bot_id}>"), "@Pi")
                .trim()
                .to_owned();
            let mut links = Vec::new();
            for (index, att) in message.attachments.iter().enumerate() {
                if att.size <= MAX_ATTACHMENT && files.len() < MAX_FILES {
                    files.push(DiscordFile {
                        url: att.url.clone(),
                        name: att.name.clone(),
                        size: att.size,
                        key: format!("discord:{}:{index}", message.id),
                        ref_id: message.id.clone(),
                    });
                } else {
                    links.push(format!(
                        "[attachment {} ({} bytes): {}]",
                        att.name, att.size, att.url
                    ));
                }
            }
            let who = format!(
                "{}<@{}>",
                if message.author.bot { "bot, " } else { "" },
                message.author.id
            );
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(text);
            }
            parts.extend(links);
            lines.push(format!(
                "{} ({who}): {}",
                message.author.name,
                parts.join("\n")
            ));
        }
    }

    FormattedBatch {
        text: lines.join("\n"),
        files,
    }
}

pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > limit {
        let byte_limit = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(idx, _)| idx);
        let half = limit / 2;
        let slice = &rest[..byte_limit];
        let cut = slice
            .rfind('\n')
            .filter(|&pos| rest[..pos].chars().count() >= half)
            .or_else(|| {
                slice
                    .rfind(' ')
                    .filter(|&pos| rest[..pos].chars().count() >= half)
            })
            .unwrap_or(byte_limit);
        chunks.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_owned());
    }
    chunks
}

fn snowflake_num(id: &str) -> u128 {
    id.parse().unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
