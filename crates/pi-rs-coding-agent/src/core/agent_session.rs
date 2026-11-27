//! `AgentSession`: the lifecycle and orchestration layer shared by every front door.
//!
//! Upstream's `packages/coding-agent/src/core/agent-session.ts` owns persistence, extension event
//! dispatch, auto-compaction, auto-retry, tool loadout, and session tree operations around the
//! core `Agent`. This module keeps the same responsibilities but talks to its collaborators
//! through small local protocols ([`SessionManager`], [`SettingsStore`], [`AuthStorage`],
//! [`ModelRegistry`], [`Compactor`], [`ExtensionRunner`]) so the session can be exercised offline
//! and the sibling modules being ported in parallel can be adapted at integration time.
//! Their JSON-shaped contracts use `serde_json::Value` where upstream uses extensible TS objects.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::{Display, Formatter},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use pi_rs_agent::{
    Agent, AgentError, AgentEvent, AgentListener, AgentMessage, CancellationToken, QueueMode,
    Subscription, ThinkingLevel,
};
use pi_rs_ai::{
    AssistantContent, ImageContent, InputContent, Model, StopReason, StringOrBlocks, SystemMessage,
    TextContent, UserMessage, UserRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::system_prompt::SystemPromptSections;

/// Errors produced by the session layer.
#[derive(Debug)]
pub enum SessionError {
    NoModel,
    NoAuth(String),
    StreamingBehaviorRequired,
    CompactionInProgress,
    CompactionFailed(String),
    InvalidEntryId(String),
    Disposed,
    Agent(AgentError),
    Extension(String),
}

impl Display for SessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoModel => formatter.write_str("No model selected"),
            Self::NoAuth(provider) => write!(formatter, "No API key for {provider}"),
            Self::StreamingBehaviorRequired => formatter.write_str(
                "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
            ),
            Self::CompactionInProgress => formatter.write_str(
                "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.",
            ),
            Self::CompactionFailed(message) => write!(formatter, "Compaction failed: {message}"),
            Self::InvalidEntryId(entry_id) => write!(formatter, "Invalid entry ID for forking: {entry_id}"),
            Self::Disposed => formatter.write_str("Session disposed"),
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Extension(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<AgentError> for SessionError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// Position of a fork relative to the selected entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkPosition {
    Before,
    At,
}

/// A persisted session entry, as exposed to the event stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// Local protocol for the JSONL v3 tree session manager.
pub trait SessionManager: Send + Sync {
    fn session_id(&self) -> String;
    fn session_file(&self) -> Option<String>;
    fn cwd(&self) -> String;
    fn messages(&self) -> Vec<AgentMessage>;
    fn append_message(&self, message: &AgentMessage) -> String;
    fn append_model_change(&self, provider: &str, model_id: &str);
    fn append_thinking_level_change(&self, level: ThinkingLevel);
    fn append_compaction(&self, summary: &str, first_kept_entry_id: Option<String>) -> String;
    fn entries(&self) -> Vec<SessionEntry>;
    fn navigate(&self, entry_id: &str) -> Result<()>;
    fn fork(&self, entry_id: &str, position: ForkPosition) -> Result<()>;
    fn stats(&self) -> SessionStatsSnapshot;
    fn disk(&self) -> Option<&super::agent_session_services::DiskSession> {
        None
    }
    fn try_append_message(&self, message: &AgentMessage) -> Result<String> {
        Ok(self.append_message(message))
    }
    fn try_append_compaction(&self, result: &CompactionResult) -> Result<String> {
        Ok(self.append_compaction(&result.summary, result.first_kept_entry_id.clone()))
    }
    fn append_usage(
        &self,
        _kind: &str,
        _provider: &str,
        _model: &str,
        _usage: &pi_rs_ai::Usage,
        _note: Option<&str>,
    ) {
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// Counts collected from every entry on the current branch, including compacted history.
#[derive(Clone, Debug, Default)]
pub struct SessionStatsSnapshot {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: TokenCounts,
    pub cost: f64,
}

#[derive(Clone, Debug)]
enum EntryKind {
    Message(AgentMessage),
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingChange {
        level: ThinkingLevel,
    },
    Compaction {
        summary: String,
        first_kept_entry_id: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct StoredEntry {
    id: String,
    parent: Option<String>,
    kind: EntryKind,
}

#[derive(Default)]
struct InMemoryStore {
    id: String,
    file: Option<String>,
    cwd: String,
    entries: Vec<StoredEntry>,
    next_id: usize,
    /// Index of the compaction boundary: messages before it are omitted from projection.
    kept_from: Option<usize>,
}

/// An offline session manager with the same tree operations as the JSONL v3 manager.
#[derive(Clone, Default)]
pub struct InMemorySessionManager {
    store: Arc<Mutex<InMemoryStore>>,
}

impl InMemorySessionManager {
    pub fn new(cwd: impl Into<String>) -> Self {
        let store = InMemoryStore {
            id: format!("session-{}", next_session_number()),
            cwd: cwd.into(),
            ..InMemoryStore::default()
        };
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn persisted(cwd: impl Into<String>, file: impl Into<String>) -> Self {
        let session = Self::new(cwd);
        if let Ok(mut store) = session.store.lock() {
            store.file = Some(file.into());
        }
        session
    }

    fn lock(&self) -> MutexGuard<'_, InMemoryStore> {
        self.store.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn allocate_id(store: &mut InMemoryStore) -> String {
        store.next_id += 1;
        format!("entry-{}", store.next_id)
    }
}

fn next_session_number() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl SessionManager for InMemorySessionManager {
    fn session_id(&self) -> String {
        self.lock().id.clone()
    }

    fn session_file(&self) -> Option<String> {
        self.lock().file.clone()
    }

    fn cwd(&self) -> String {
        self.lock().cwd.clone()
    }

    fn messages(&self) -> Vec<AgentMessage> {
        let store = self.lock();
        let kept_from = store.kept_from.unwrap_or(0);
        let mut messages = Vec::new();
        for (index, entry) in store.entries.iter().enumerate() {
            match &entry.kind {
                EntryKind::Message(message) if index >= kept_from => messages.push(message.clone()),
                EntryKind::Compaction { summary, .. } => {
                    messages.push(AgentMessage::user(format!(
                        "This session is being continued from a previous conversation. Summary:\n{summary}"
                    )));
                }
                _ => {}
            }
        }
        messages
    }

    fn append_message(&self, message: &AgentMessage) -> String {
        let mut store = self.lock();
        let id = Self::allocate_id(&mut store);
        let parent = store.entries.last().map(|entry| entry.id.clone());
        store.entries.push(StoredEntry {
            id: id.clone(),
            parent,
            kind: EntryKind::Message(message.clone()),
        });
        id
    }

    fn append_model_change(&self, provider: &str, model_id: &str) {
        let mut store = self.lock();
        let id = Self::allocate_id(&mut store);
        let parent = store.entries.last().map(|entry| entry.id.clone());
        store.entries.push(StoredEntry {
            id,
            parent,
            kind: EntryKind::ModelChange {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
        });
    }

    fn append_thinking_level_change(&self, level: ThinkingLevel) {
        let mut store = self.lock();
        let id = Self::allocate_id(&mut store);
        let parent = store.entries.last().map(|entry| entry.id.clone());
        store.entries.push(StoredEntry {
            id,
            parent,
            kind: EntryKind::ThinkingChange { level },
        });
    }

    fn append_compaction(&self, summary: &str, first_kept_entry_id: Option<String>) -> String {
        let mut store = self.lock();
        let id = Self::allocate_id(&mut store);
        let parent = store.entries.last().map(|entry| entry.id.clone());
        store.kept_from = Some(store.entries.len());
        store.entries.push(StoredEntry {
            id: id.clone(),
            parent,
            kind: EntryKind::Compaction {
                summary: summary.to_string(),
                first_kept_entry_id,
            },
        });
        id
    }

    fn entries(&self) -> Vec<SessionEntry> {
        self.lock()
            .entries
            .iter()
            .map(|entry| {
                let (kind, data) = match &entry.kind {
                    EntryKind::Message(message) => (
                        "message",
                        serde_json::to_value(message).unwrap_or(Value::Null),
                    ),
                    EntryKind::ModelChange { provider, model_id } => (
                        "model_change",
                        json!({ "provider": provider, "modelId": model_id }),
                    ),
                    EntryKind::ThinkingChange { level } => {
                        ("thinking_level_change", json!({ "level": level }))
                    }
                    EntryKind::Compaction {
                        summary,
                        first_kept_entry_id,
                    } => (
                        "compaction",
                        json!({ "summary": summary, "firstKeptEntryId": first_kept_entry_id }),
                    ),
                };
                SessionEntry {
                    id: entry.id.clone(),
                    kind: kind.to_string(),
                    data,
                }
            })
            .collect()
    }

    fn navigate(&self, entry_id: &str) -> Result<()> {
        let mut store = self.lock();
        let index = store
            .entries
            .iter()
            .position(|entry| entry.id == entry_id)
            .ok_or_else(|| SessionError::InvalidEntryId(entry_id.to_string()))?;
        store.entries.truncate(index + 1);
        store.kept_from = None;
        Ok(())
    }

    fn fork(&self, entry_id: &str, position: ForkPosition) -> Result<()> {
        let mut store = self.lock();
        let index = store
            .entries
            .iter()
            .position(|entry| entry.id == entry_id)
            .ok_or_else(|| SessionError::InvalidEntryId(entry_id.to_string()))?;
        let keep = match position {
            ForkPosition::At => index + 1,
            ForkPosition::Before => store.entries[index]
                .parent
                .as_ref()
                .and_then(|parent| store.entries.iter().position(|entry| &entry.id == parent))
                .map_or(0, |parent| parent + 1),
        };
        store.entries.truncate(keep);
        store.kept_from = None;
        store.id = format!("session-{}", next_session_number());
        Ok(())
    }

    fn stats(&self) -> SessionStatsSnapshot {
        let store = self.lock();
        let mut stats = SessionStatsSnapshot::default();
        for entry in &store.entries {
            let EntryKind::Message(message) = &entry.kind else {
                continue;
            };
            match message {
                AgentMessage::User(_) => stats.user_messages += 1,
                AgentMessage::Assistant(assistant) => {
                    stats.assistant_messages += 1;
                    stats.tool_calls += assistant
                        .content
                        .iter()
                        .filter(|block| matches!(block, AssistantContent::ToolCall(_)))
                        .count();
                    stats.tokens.input += assistant.usage.input;
                    stats.tokens.output += assistant.usage.output;
                    stats.tokens.cache_read += assistant.usage.cache_read;
                    stats.tokens.cache_write += assistant.usage.cache_write;
                    stats.tokens.total += assistant.usage.total_tokens;
                    stats.cost += assistant.usage.cost.total;
                }
                AgentMessage::ToolResult(tool) => {
                    stats.tool_results += 1;
                    if let Some(usage) = &tool.usage {
                        stats.tokens.input += usage.input;
                        stats.tokens.output += usage.output;
                        stats.tokens.cache_read += usage.cache_read;
                        stats.tokens.cache_write += usage.cache_write;
                        stats.tokens.total += usage.total_tokens;
                        stats.cost += usage.cost.total;
                    }
                }
                _ => {}
            }
            stats.total_messages += 1;
        }
        stats
    }
}

/// Local protocol for the settings that affect a run.
pub trait SettingsStore: Send + Sync {
    fn retry_enabled(&self) -> bool;
    fn max_retries(&self) -> usize;
    fn retry_base_delay_ms(&self) -> u64;
    fn compaction_enabled(&self) -> bool;
    fn compaction_threshold(&self) -> f64;
    fn steering_mode(&self) -> QueueMode;
    fn follow_up_mode(&self) -> QueueMode;
    fn set_retry_enabled(&self, _enabled: bool) {}
    fn set_compaction_enabled(&self, _enabled: bool) {}
    fn set_steering_mode(&self, _mode: QueueMode) {}
    fn set_follow_up_mode(&self, _mode: QueueMode) {}
    fn shell_path(&self) -> Option<String> {
        None
    }
    fn shell_command_prefix(&self) -> Option<String> {
        None
    }
    fn compaction_reserve_tokens(&self) -> Option<u64> {
        None
    }
    fn auto_resize_images(&self) -> bool {
        true
    }
    fn trusted_project(&self) -> Option<bool> {
        None
    }
    fn raw_value(&self) -> Value {
        Value::Null
    }
    fn cache_warming_mode(&self) -> super::cache_warmer::CacheWarmingMode {
        super::cache_warmer::CacheWarmingMode::Idle
    }
    fn compaction_reserve_tokens_for_model(&self, _provider: &str, _model_id: &str) -> Option<u64> {
        self.compaction_reserve_tokens()
    }
    fn branch_summary_reserve_tokens(&self) -> usize {
        16_384
    }
    fn branch_summary_skip_prompt(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct SettingsState {
    retry_enabled: bool,
    max_retries: usize,
    retry_base_delay_ms: u64,
    compaction_enabled: bool,
    compaction_threshold: f64,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

/// Offline settings implementation. The short default retry delay keeps tests deterministic;
/// upstream uses the provider retry settings from `settings.json`.
#[derive(Clone)]
pub struct InMemorySettings {
    state: Arc<Mutex<SettingsState>>,
}

impl Default for InMemorySettings {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(SettingsState {
                retry_enabled: true,
                max_retries: 2,
                retry_base_delay_ms: 5,
                compaction_enabled: true,
                compaction_threshold: 0.8,
                steering_mode: QueueMode::OneAtATime,
                follow_up_mode: QueueMode::OneAtATime,
            })),
        }
    }
}

impl InMemorySettings {
    fn lock(&self) -> MutexGuard<'_, SettingsState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    pub fn set_retry_enabled(&self, enabled: bool) {
        self.lock().retry_enabled = enabled;
    }
    pub fn set_max_retries(&self, retries: usize) {
        self.lock().max_retries = retries;
    }
    pub fn set_retry_base_delay_ms(&self, delay: u64) {
        self.lock().retry_base_delay_ms = delay;
    }
    pub fn set_compaction_enabled(&self, enabled: bool) {
        self.lock().compaction_enabled = enabled;
    }
    pub fn set_compaction_threshold(&self, threshold: f64) {
        self.lock().compaction_threshold = threshold;
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.lock().steering_mode = mode;
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.lock().follow_up_mode = mode;
    }
}

impl SettingsStore for InMemorySettings {
    fn set_retry_enabled(&self, enabled: bool) {
        InMemorySettings::set_retry_enabled(self, enabled);
    }
    fn set_compaction_enabled(&self, enabled: bool) {
        InMemorySettings::set_compaction_enabled(self, enabled);
    }
    fn set_steering_mode(&self, mode: QueueMode) {
        InMemorySettings::set_steering_mode(self, mode);
    }
    fn set_follow_up_mode(&self, mode: QueueMode) {
        InMemorySettings::set_follow_up_mode(self, mode);
    }
    fn retry_enabled(&self) -> bool {
        self.lock().retry_enabled
    }
    fn max_retries(&self) -> usize {
        self.lock().max_retries
    }
    fn retry_base_delay_ms(&self) -> u64 {
        self.lock().retry_base_delay_ms
    }
    fn compaction_enabled(&self) -> bool {
        self.lock().compaction_enabled
    }
    fn compaction_threshold(&self) -> f64 {
        self.lock().compaction_threshold
    }
    fn steering_mode(&self) -> QueueMode {
        self.lock().steering_mode
    }
    fn follow_up_mode(&self) -> QueueMode {
        self.lock().follow_up_mode
    }
}

/// Local protocol for configured provider credentials.
pub trait AuthStorage: Send + Sync {
    fn has_configured_auth(&self, provider: &str) -> bool;
}

/// In-memory credentials. `allow_all()` is intended for offline faux providers.
#[derive(Clone, Debug, Default)]
pub struct StaticAuth {
    providers: Arc<Mutex<HashSet<String>>>,
    allow_all: bool,
}

impl StaticAuth {
    pub fn allow_all() -> Self {
        Self {
            allow_all: true,
            ..Self::default()
        }
    }

    pub fn with_providers(providers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            providers: Arc::new(Mutex::new(providers.into_iter().map(Into::into).collect())),
            allow_all: false,
        }
    }

    pub fn add(&self, provider: impl Into<String>) {
        if let Ok(mut providers) = self.providers.lock() {
            providers.insert(provider.into());
        }
    }
}

impl AuthStorage for StaticAuth {
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.allow_all
            || self
                .providers
                .lock()
                .is_ok_and(|providers| providers.contains(provider))
    }
}

/// Local protocol for model resolution.
pub trait ModelRegistry: Send + Sync {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model>;
    fn models(&self) -> Vec<Model>;
    fn has_configured_auth(&self, provider: &str) -> bool;
    fn scoped_models(&self) -> Vec<(Model, Option<ThinkingLevel>)> {
        Vec::new()
    }
    fn model_catalog(&self) -> Option<pi_rs_ai::Models> {
        None
    }
    fn last_routed_model(&self) -> Option<(Model, Option<ThinkingLevel>)> {
        None
    }
    fn is_using_subscription(&self, _provider: &str) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct InMemoryModelRegistry {
    models: Vec<Model>,
    auth: Arc<dyn AuthStorage>,
}

impl InMemoryModelRegistry {
    pub fn new(models: Vec<Model>, auth: Arc<dyn AuthStorage>) -> Self {
        Self { models, auth }
    }
}

impl ModelRegistry for InMemoryModelRegistry {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.models
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
            .cloned()
    }

    fn models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        self.auth.has_configured_auth(provider)
    }
}

/// Outcome of a compaction request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    #[serde(skip)]
    pub estimated_tokens_after: u64,
}

/// Local protocol for summarization. The first kept entry is resolved by the session manager
/// when the summary is persisted.
pub trait Compactor: Send + Sync {
    fn compact<'a>(
        &'a self,
        model: Model,
        manager: &'a dyn SessionManager,
        instructions: &'a str,
        signal: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult>>;
}

/// Deterministic compactor used by tests and as a fallback when no summarization model is set.
#[derive(Clone, Copy, Debug)]
pub struct TailCompactor {
    pub keep_recent: usize,
}

impl Compactor for TailCompactor {
    fn compact<'a>(
        &'a self,
        _model: Model,
        manager: &'a dyn SessionManager,
        instructions: &'a str,
        _signal: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult>> {
        Box::pin(async move {
            let messages = manager.messages();
            let tokens_before = messages
                .iter()
                .map(|message| {
                    serde_json::to_string(message).map_or(0, |text| text.len() / 4) as u64
                })
                .sum();
            let mut summary = format!(
                "Summary of {} earlier messages. Kept {} recent messages.",
                messages.len().saturating_sub(self.keep_recent),
                self.keep_recent.min(messages.len())
            );
            if !instructions.trim().is_empty() {
                summary.push_str("\n\n");
                summary.push_str(instructions.trim());
            }
            let estimated_tokens_after = summary.len() as u64 / 4;
            Ok(CompactionResult {
                summary,
                first_kept_entry_id: None,
                tokens_before,
                estimated_tokens_after,
            })
        })
    }
}

/// Local protocol for extension event dispatch. The shared `ExtensionRunner` being ported in
/// parallel can implement this boundary without changing session orchestration.
pub trait ExtensionRunner: Send + Sync {
    fn emit(&self, event: &Value) -> Value;
    fn emit_async<'a>(&'a self, event: Value) -> futures_util::future::BoxFuture<'a, Value> {
        Box::pin(async move { self.emit(&event) })
    }
    fn has_handlers(&self, _event_type: &str) -> bool {
        false
    }
    fn native(&self) -> Option<&Arc<super::extensions::runner::ExtensionRunner>> {
        None
    }
    fn resources(&self) -> Option<&super::resource_loader::Resources> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopExtensionRunner;

impl ExtensionRunner for NoopExtensionRunner {
    fn emit(&self, _event: &Value) -> Value {
        Value::Null
    }
}

/// Records extension events for assertions and for the front door's diagnostics.
#[derive(Clone, Debug, Default)]
pub struct RecordingExtensionRunner {
    events: Arc<Mutex<Vec<Value>>>,
}

impl RecordingExtensionRunner {
    pub fn events(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl ExtensionRunner for RecordingExtensionRunner {
    fn emit(&self, event: &Value) -> Value {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
        Value::Null
    }

    fn has_handlers(&self, _event_type: &str) -> bool {
        true
    }
}

/// Why a compaction was requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// Why a session lifecycle event was emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionReason {
    Startup,
    New,
    Resume,
    Fork,
    Reload,
    Switch,
    Quit,
}

impl SessionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::New => "new",
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Reload => "reload",
            Self::Switch => "switch",
            Self::Quit => "quit",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StreamingBehavior {
    #[default]
    Steer,
    FollowUp,
}

#[derive(Clone, Debug, Default)]
pub struct PromptOptions {
    pub streaming_behavior: Option<StreamingBehavior>,
    pub expand_prompt_templates: bool,
    pub source: String,
    pub images: Vec<ImageContent>,
}

impl PromptOptions {
    pub fn source(&self) -> &str {
        if self.source.is_empty() {
            "interactive"
        } else {
            &self.source
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptDisposition {
    Handled,
    Queued,
    Started,
}

impl PromptDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Queued => "queued",
            Self::Started => "started",
        }
    }
}

/// Session-level events. Core agent events are re-emitted with session additions.
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentSessionEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
        will_retry: bool,
    },
    AgentSettled {
        aborted: bool,
    },
    TurnStart,
    EntryAppended {
        entry: super::session_manager::SessionEntry,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        aborted: bool,
        will_retry: bool,
    },
    AutoRetryStart {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    SessionStart {
        reason: SessionReason,
    },
    SessionShutdown {
        reason: SessionReason,
    },
    ModelSelect {
        provider: String,
        id: String,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    #[serde(rename = "tool_execution_start")]
    NestedToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        parent_tool_call_id: String,
    },
    #[serde(rename = "tool_execution_update")]
    NestedToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: pi_rs_agent::AgentToolResult,
        parent_tool_call_id: String,
    },
    #[serde(rename = "tool_execution_end")]
    NestedToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: pi_rs_agent::AgentToolResult,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        parent_tool_call_id: String,
    },
    #[serde(untagged)]
    Core(AgentEvent),
}

impl AgentSessionEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::AgentSettled { .. } => "agent_settled",
            Self::TurnStart => "turn_start",
            Self::Core(event) => match event {
                AgentEvent::AgentStart => "agent_start",
                AgentEvent::AgentEnd { .. } => "agent_end",
                AgentEvent::TurnStart => "turn_start",
                AgentEvent::TurnEnd { .. } => "turn_end",
                AgentEvent::MessageStart { .. } => "message_start",
                AgentEvent::MessageUpdate { .. } => "message_update",
                AgentEvent::MessageEnd { .. } => "message_end",
                AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
                AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
                AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            },
            Self::EntryAppended { .. } => "entry_appended",
            Self::QueueUpdate { .. } => "queue_update",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
            Self::AutoRetryStart { .. } => "auto_retry_start",
            Self::AutoRetryEnd { .. } => "auto_retry_end",
            Self::SessionStart { .. } => "session_start",
            Self::SessionShutdown { .. } => "session_shutdown",
            Self::ModelSelect { .. } => "model_select",
            Self::ThinkingLevelChanged { .. } => "thinking_level_changed",
            Self::SessionInfoChanged { .. } => "session_info_changed",
            Self::NestedToolExecutionStart { .. } => "tool_execution_start",
            Self::NestedToolExecutionUpdate { .. } => "tool_execution_update",
            Self::NestedToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}

/// Current context-window usage reported by `/session`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// Statistics reported by `/session`.
#[derive(Clone, Debug, Serialize)]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: TokenCounts,
    pub cost: f64,
    pub context_usage: Option<ContextUsage>,
}

pub type SessionEventListener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;
type OutputBackpressure = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub struct AgentSessionConfig {
    pub agent: Agent,
    pub session_manager: Arc<dyn SessionManager>,
    pub settings: Arc<dyn SettingsStore>,
    pub auth: Arc<dyn AuthStorage>,
    pub models: Arc<dyn ModelRegistry>,
    pub compactor: Arc<dyn Compactor>,
    pub extensions: Arc<dyn ExtensionRunner>,
    pub cwd: String,
    /// Message used to re-enter the loop after a retryable failure. Upstream sends the failed
    /// response as hidden request context; the local port uses a visible continuation prompt.
    pub retry_prompt: Option<String>,
}

struct Inner {
    agent: Agent,
    session_manager: Arc<dyn SessionManager>,
    settings: Arc<dyn SettingsStore>,
    auth: Arc<dyn AuthStorage>,
    models: Arc<dyn ModelRegistry>,
    compactor: Arc<dyn Compactor>,
    extensions: Arc<dyn ExtensionRunner>,
    cwd: String,
    system_prompt_sections: Option<SystemPromptSections>,
    retry_prompt: String,
    listeners: BTreeMap<usize, SessionEventListener>,
    next_listener: usize,
    agent_subscription: Option<Subscription>,
    output_backpressure: Option<OutputBackpressure>,
    steering: Vec<String>,
    follow_up: Vec<String>,
    pending_bash: Vec<AgentMessage>,
    retry_attempt: usize,
    retry_cancel: Option<CancellationToken>,
    run_settled: Option<CancellationToken>,
    run_aborted: bool,
    compaction_cancel: Option<CancellationToken>,
    is_streaming: bool,
    is_compacting: bool,
    disposed: bool,
    cache_warmer: Arc<super::cache_warmer::CacheWarmer>,
    nested_runner: Option<Arc<super::nested_tool_calls::NestedToolCallRunner>>,
}

#[derive(Clone, Debug)]
struct AssistantSnapshot {
    retryable: bool,
    error_message: Option<String>,
}

/// The central session object. Clones share one conversation and one event stream.
#[derive(Clone)]
pub struct AgentSession {
    inner: Arc<Mutex<Inner>>,
}

struct RunningSession<'a>(&'a AgentSession);

impl Drop for RunningSession<'_> {
    fn drop(&mut self) {
        self.0.finish_run();
    }
}

impl AgentSession {
    pub fn new(config: AgentSessionConfig) -> Result<Self> {
        let warmer_models = config.models.clone();
        let warmer_settings = config.settings.clone();
        let warmer_session = config.session_manager.clone();
        let warmer_extensions = config.extensions.clone();
        let warmer_stream: super::cache_warmer::WarmStreamFn =
            Arc::new(move |model, context, options| {
                let models = warmer_models.clone();
                Box::pin(async move {
                    let Some(catalog) = models.model_catalog() else {
                        return Err("Model catalog unavailable".to_owned());
                    };
                    let message = catalog
                        .complete_simple(model, context, options)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok((
                        message.response_model.clone(),
                        message.usage,
                        message.stop_reason,
                    ))
                })
            });
        let warmer_mode: super::cache_warmer::WarmModeFn =
            Arc::new(move || warmer_settings.cache_warming_mode());
        let warmer_tokens_session = warmer_session.clone();
        let warmer_tokens: super::cache_warmer::WarmPromptTokensFn = Arc::new(move || {
            warmer_tokens_session
                .messages()
                .iter()
                .rev()
                .find_map(|msg| match msg {
                    AgentMessage::Assistant(assistant) => Some(
                        assistant.usage.input
                            + assistant.usage.cache_read
                            + assistant.usage.cache_write,
                    ),
                    _ => None,
                })
                .unwrap_or(0.0)
        });
        let warmer_decide: super::cache_warmer::WarmDecideFn = Arc::new(move |decision| {
            let extensions = warmer_extensions.clone();
            Box::pin(async move {
                if !extensions.has_handlers("cache_warming_decision") {
                    return decision.action;
                }
                if let Some(runner) = extensions.native() {
                    let event = serde_json::json!({
                        "type": "cache_warming_decision",
                        "warmCost": decision.warm_cost,
                        "missCost": decision.miss_cost,
                        "continuationProbability": decision.continuation_probability,
                        "action": decision.action,
                    });
                    let result = runner.emit_cache_warming_decision(event).await;
                    if let Some(action) = result.get("action").and_then(Value::as_str) {
                        return match action {
                            "warm" => pi_rs_ai::cache_warming::CacheWarmingAction::Warm,
                            "stop" => pi_rs_ai::cache_warming::CacheWarmingAction::Stop,
                            _ => decision.action,
                        };
                    }
                }
                decision.action
            })
        });
        let warmer_record_session = warmer_session;
        let warmer_record: super::cache_warmer::WarmRecordUsageFn =
            Arc::new(move |provider, model, usage, note| {
                warmer_record_session.append_usage(
                    "cache_warm",
                    &provider,
                    &model,
                    &usage,
                    note.as_deref(),
                );
            });
        let cache_warmer = Arc::new(super::cache_warmer::CacheWarmer::new(
            warmer_stream,
            warmer_mode,
            warmer_tokens,
            warmer_decide,
            warmer_record,
        ));

        let inner = Arc::new(Mutex::new(Inner {
            agent: config.agent,
            session_manager: config.session_manager,
            settings: config.settings,
            auth: config.auth,
            models: config.models,
            compactor: config.compactor,
            extensions: config.extensions,
            cwd: config.cwd,
            system_prompt_sections: None,
            retry_prompt: config
                .retry_prompt
                .unwrap_or_else(|| "Continue from the last response.".to_string()),
            listeners: BTreeMap::new(),
            next_listener: 0,
            agent_subscription: None,
            output_backpressure: None,
            steering: Vec::new(),
            follow_up: Vec::new(),
            pending_bash: Vec::new(),
            retry_attempt: 0,
            retry_cancel: None,
            run_settled: None,
            run_aborted: false,
            compaction_cancel: None,
            is_streaming: false,
            is_compacting: false,
            disposed: false,
            cache_warmer,
            nested_runner: None,
        }));
        let weak = Arc::downgrade(&inner);
        let listener: Arc<AgentListener> = Arc::new(move |event, _signal| {
            let session = weak.upgrade().map(|inner| Self { inner });
            Box::pin(async move {
                if let Some(session) = session {
                    session
                        .handle_agent_event(event)
                        .map_err(|error| AgentError::Callback(error.to_string()))?;
                    let backpressure = session.lock().output_backpressure.clone();
                    if let Some(drain) = backpressure {
                        drain().await;
                    }
                }
                Ok(())
            })
        });
        let subscription = {
            let guard = inner.lock().unwrap_or_else(|error| error.into_inner());
            guard.agent.subscribe(listener)
        };
        {
            let mut guard = inner.lock().unwrap_or_else(|error| error.into_inner());
            guard.agent_subscription = Some(subscription);
        }
        let session = Self { inner };
        {
            let guard = session.lock();
            guard
                .agent
                .set_steering_mode(guard.settings.steering_mode());
            guard
                .agent
                .set_follow_up_mode(guard.settings.follow_up_mode());
        }
        session.emit(AgentSessionEvent::SessionStart {
            reason: SessionReason::Startup,
        });
        Ok(session)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Subscribe to session events. Dropping the returned id does not unsubscribe.
    pub fn subscribe(&self, listener: SessionEventListener) -> usize {
        let mut inner = self.lock();
        let id = inner.next_listener;
        inner.next_listener += 1;
        inner.listeners.insert(id, listener);
        id
    }

    pub fn unsubscribe(&self, id: usize) -> bool {
        self.lock().listeners.remove(&id).is_some()
    }

    /// Forward a raw extension event from the cwd-bound runtime.
    pub fn emit_runtime_event(&self, event: Value) {
        let runner = self.lock().extensions.clone();
        runner.emit(&event);
    }

    pub(crate) fn set_output_backpressure(&self, drain: OutputBackpressure) {
        self.lock().output_backpressure = Some(drain);
    }

    fn emit(&self, event: AgentSessionEvent) {
        let listeners: Vec<SessionEventListener> =
            self.lock().listeners.values().cloned().collect();
        let extension_event = match &event {
            AgentSessionEvent::SessionStart { reason } => {
                Some(json!({ "type": "session_start", "reason": reason.as_str() }))
            }
            AgentSessionEvent::SessionShutdown { reason } => {
                let value = match reason {
                    SessionReason::Reload => "reload",
                    SessionReason::Quit => "quit",
                    _ => "other",
                };
                Some(json!({ "type": "session_shutdown", "reason": value }))
            }
            AgentSessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => Some(json!({
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorMessage": error_message,
            })),
            AgentSessionEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => Some(json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
                "finalError": final_error,
            })),
            _ => None,
        };
        if let Some(extension_event) = extension_event {
            let runner = self.lock().extensions.clone();
            runner.emit(&extension_event);
        }
        for listener in listeners {
            listener(&event);
        }
    }

    fn handle_agent_event(&self, event: &AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AgentStart => self.emit(AgentSessionEvent::AgentStart),
            AgentEvent::AgentEnd { messages } => {
                if let Some(runner) = self.lock().nested_runner.clone() {
                    runner.clear();
                }
                self.emit(AgentSessionEvent::AgentEnd {
                    messages: messages.to_vec(),
                    will_retry: false,
                });
            }
            AgentEvent::TurnStart => self.emit(AgentSessionEvent::TurnStart),
            AgentEvent::MessageStart { message } => {
                let changed = self.dequeue_started_message(message);
                if changed {
                    self.emit_queue_update();
                }
                self.emit(AgentSessionEvent::Core(event.clone()));
            }
            AgentEvent::MessageEnd { message } => {
                let message_to_emit = if let AgentMessage::ToolResult(tr) = message
                    && let Some(nested_runner) = self.lock().nested_runner.clone()
                    && let Some(summary) = nested_runner.take_record(&tr.tool_call_id)
                    && (summary.calls.is_some() || summary.usage.is_some())
                {
                    let mut updated = (**tr).clone();
                    if let Some(calls) = summary.calls {
                        updated.nested_calls = Some(calls);
                    }
                    if let Some(nested_usage) = summary.usage {
                        updated.usage = Some(match updated.usage {
                            Some(existing) => {
                                super::usage_totals::combine_usage(&existing, &nested_usage)
                            }
                            None => nested_usage,
                        });
                    }
                    AgentMessage::ToolResult(Arc::new(updated))
                } else {
                    message.clone()
                };
                self.emit(AgentSessionEvent::Core(AgentEvent::MessageEnd {
                    message: message_to_emit.clone(),
                }));
                self.session_manager()
                    .try_append_message(&message_to_emit)?;
                if let AgentMessage::Assistant(assistant) = &message_to_emit {
                    if assistant.stop_reason != StopReason::Error {
                        let mut inner = self.lock();
                        if inner.retry_attempt > 0 {
                            inner.retry_attempt = 0;
                            drop(inner);
                            self.emit(AgentSessionEvent::AutoRetryEnd {
                                success: true,
                                attempt: 0,
                                final_error: None,
                            });
                        }
                    }
                    if !assistant.stop_reason.failed() {
                        let (warmer, agent, session_mgr, session_id) = {
                            let inner = self.lock();
                            (
                                inner.cache_warmer.clone(),
                                inner.agent.clone(),
                                inner.session_manager.clone(),
                                inner.session_manager.session_id(),
                            )
                        };
                        let model = agent.model();
                        let expected_len = session_mgr.messages().len();
                        let llm_messages = agent
                            .messages()
                            .iter()
                            .filter_map(AgentMessage::to_llm)
                            .collect::<Vec<_>>();
                        let options = pi_rs_ai::StreamOptions {
                            session_id: Some(session_id),
                            ..pi_rs_ai::StreamOptions::default()
                        };
                        let check_model = model.clone();
                        let is_current: super::cache_warmer::WarmIsCurrentFn =
                            Arc::new(move || {
                                let cur = agent.model();
                                cur.provider == check_model.provider
                                    && cur.id == check_model.id
                                    && session_mgr.messages().len() == expected_len
                            });
                        warmer.start(
                            super::cache_warmer::CacheWarmRequest {
                                model,
                                context: pi_rs_ai::Context {
                                    messages: llm_messages,
                                    ..pi_rs_ai::Context::default()
                                },
                                options,
                            },
                            is_current,
                        );
                    }
                }
            }
            _ => self.emit(AgentSessionEvent::Core(event.clone())),
        }
        Ok(())
    }

    fn dequeue_started_message(&self, message: &AgentMessage) -> bool {
        let AgentMessage::User(user) = message else {
            return false;
        };
        let mut inner = self.lock();
        if inner.steering.is_empty() && inner.follow_up.is_empty() {
            return false;
        }
        let text = match &user.content {
            StringOrBlocks::String(text) => std::borrow::Cow::Borrowed(text.as_str()),
            StringOrBlocks::Blocks(blocks) => {
                std::borrow::Cow::Owned(pi_rs_ai::transcript::input_text(blocks, " "))
            }
        };
        if let Some(index) = inner.steering.iter().position(|queued| queued == &*text) {
            inner.steering.remove(index);
        } else if let Some(index) = inner.follow_up.iter().position(|queued| queued == &*text) {
            inner.follow_up.remove(index);
        } else {
            return false;
        }
        true
    }

    fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let inner = self.lock();
            (inner.steering.clone(), inner.follow_up.clone())
        };
        self.emit(AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        });
    }

    /// Drop queued steering and follow-up messages.
    pub fn clear_queue(&self) {
        {
            let mut inner = self.lock();
            inner.steering.clear();
            inner.follow_up.clear();
            inner.agent.clear_all_queues();
        }
        self.emit_queue_update();
    }

    pub fn peek_steering(&self) -> Vec<String> {
        self.lock().steering.clone()
    }

    pub fn peek_follow_up(&self) -> Vec<String> {
        self.lock().follow_up.clone()
    }

    /// Send a prompt. While streaming, `options.streaming_behavior` decides whether the message
    /// steers the current turn or follows it.
    pub async fn prompt(
        &self,
        text: impl Into<String>,
        options: PromptOptions,
    ) -> Result<PromptDisposition> {
        self.prompt_with_preflight(text, options, |_| {}).await
    }

    /// Report acceptance before starting the turn so RPC can correlate the following events.
    pub async fn prompt_with_preflight(
        &self,
        text: impl Into<String>,
        options: PromptOptions,
        accepted: impl FnOnce(PromptDisposition),
    ) -> Result<PromptDisposition> {
        let text = text.into();
        if self.lock().disposed {
            return Err(SessionError::Disposed);
        }
        if self.lock().is_compacting {
            return Err(SessionError::CompactionInProgress);
        }
        if self.is_streaming() {
            match options.streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.steer_with_images(text, options.images).await?
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.follow_up_with_images(text, options.images).await?
                }
                None => return Err(SessionError::StreamingBehaviorRequired),
            };
            accepted(PromptDisposition::Queued);
            return Ok(PromptDisposition::Queued);
        }
        let (extensions, before_system_prompt, before_sections) = {
            let inner = self.lock();
            if options.expand_prompt_templates && text.starts_with('/') {
                let event = json!({ "type": "command", "text": text, "source": options.source() });
                let result = inner.extensions.emit(&event);
                if result
                    .get("handled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    accepted(PromptDisposition::Handled);
                    return Ok(PromptDisposition::Handled);
                }
            }
            let model = inner.agent.model();
            if !inner.auth.has_configured_auth(&model.provider) {
                return Err(SessionError::NoAuth(model.provider.clone()));
            }
            let sections_json = inner.system_prompt_sections.as_ref().map(|sections| {
                sections
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect::<serde_json::Map<String, Value>>()
            });
            let system_prompt = if let Some(sections) = &inner.system_prompt_sections {
                pi_rs_ai::transcript::system_message_text(&SystemMessage {
                    sections: Some(
                        sections
                            .iter()
                            .map(|(name, content)| (name.clone(), Some(content.clone())))
                            .collect(),
                    ),
                    ..SystemMessage::default()
                })
            } else {
                inner.agent.system_prompt()
            };
            (inner.extensions.clone(), system_prompt, sections_json)
        };
        let mut before_event = json!({
            "type": "before_agent_start",
            "prompt": text,
            "source": options.source(),
            "systemPrompt": before_system_prompt,
        });
        if let Some(sections) = &before_sections {
            before_event["sections"] = Value::Object(sections.clone());
        }
        let before_result = extensions.emit_async(before_event).await;
        let injected_messages =
            self.apply_before_agent_start_result(&before_result, &before_system_prompt);
        let _run = self.begin_run()?;
        accepted(PromptDisposition::Started);
        let (text, images) = self.normalize_images(text, options.images);
        let agent = self.agent();
        let user = user_input(text, images);
        let mut prompt_messages = Vec::new();
        if let Some(system) = self.pending_system_message(&agent) {
            prompt_messages.push(system.into());
        }
        for injected in injected_messages {
            let _ = self.session_manager().try_append_message(&injected);
            prompt_messages.push(injected);
        }
        prompt_messages.push(user);
        let result = agent.prompt(prompt_messages).await;
        let outcome = match result {
            Ok(()) => self.after_run().await,
            Err(error) => Err(SessionError::Agent(error)),
        };
        outcome?;
        Ok(PromptDisposition::Started)
    }

    fn apply_before_agent_start_result(
        &self,
        result: &Value,
        previous_system_prompt: &str,
    ) -> Vec<AgentMessage> {
        let mut injected = Vec::new();
        if !result.is_object() {
            return injected;
        }
        let mut sections_updated = false;
        if let Some(sections_obj) = result.get("sections").and_then(Value::as_object) {
            let mut inner = self.lock();
            let target = inner
                .system_prompt_sections
                .get_or_insert_with(SystemPromptSections::new);
            let mut changed = false;
            for (name, value) in sections_obj {
                match value {
                    Value::Null => {
                        if target.shift_remove(name).is_some() {
                            changed = true;
                        }
                    }
                    Value::String(content)
                        if target.get(name).map(String::as_str) != Some(content.as_str()) =>
                    {
                        target.insert(name.clone(), content.clone());
                        changed = true;
                    }
                    _ => {}
                }
            }
            sections_updated = changed;
        }
        if let Some(new_system_prompt) = result.get("systemPrompt").and_then(Value::as_str) {
            let rendered_after_sections = self.system_prompt();
            if new_system_prompt != previous_system_prompt
                && (!sections_updated || new_system_prompt != rendered_after_sections)
            {
                let mut inner = self.lock();
                let mut forced = SystemPromptSections::new();
                forced.insert("preamble".to_owned(), new_system_prompt.to_owned());
                inner.system_prompt_sections = Some(forced);
            }
        }
        if let Some(items) = result.get("messages").and_then(Value::as_array) {
            for item in items {
                if let Some(message) = before_agent_start_message(item) {
                    injected.push(message);
                }
            }
        }
        injected
    }

    pub(super) fn with_system_prompt_sections(self, sections: SystemPromptSections) -> Self {
        self.lock().system_prompt_sections = Some(sections);
        self
    }

    pub fn with_nested_runner(
        self,
        runner: Arc<super::nested_tool_calls::NestedToolCallRunner>,
    ) -> Self {
        let session = self.clone();
        runner.set_event_sink(Arc::new(move |ev| {
            session.emit_nested_tool_event(ev);
        }));
        self.lock().nested_runner = Some(runner);
        self
    }

    pub fn emit_nested_tool_event(
        &self,
        event: super::nested_tool_calls::NestedToolExecutionEvent,
    ) {
        use super::nested_tool_calls::NestedToolExecutionEvent::*;
        let session_event = match event {
            Start {
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id,
            } => AgentSessionEvent::NestedToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id,
            },
            Update {
                tool_call_id,
                tool_name,
                args,
                partial_result,
                parent_tool_call_id,
            } => AgentSessionEvent::NestedToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
                parent_tool_call_id,
            },
            End {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
                parent_tool_call_id,
            } => AgentSessionEvent::NestedToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
                parent_tool_call_id,
            },
        };
        self.emit(session_event);
    }

    fn pending_system_message(&self, agent: &Agent) -> Option<SystemMessage> {
        let inner = self.lock();
        let desired = inner.system_prompt_sections.as_ref()?;
        let messages = agent.messages();
        let mut previous = IndexMap::new();
        for message in &messages {
            if let AgentMessage::System(message) = message {
                for (name, content) in message.sections.iter().flatten() {
                    if let Some(content) = content {
                        previous.insert(name.as_str(), content.as_str());
                    } else {
                        previous.shift_remove(name.as_str());
                    }
                }
            }
        }
        let mut sections = IndexMap::new();
        for (name, content) in desired {
            if previous.shift_remove(name.as_str()) != Some(content.as_str()) {
                sections.insert(name.clone(), Some(content.clone()));
            }
        }
        sections.extend(previous.into_keys().map(|name| (name.to_owned(), None)));
        (!sections.is_empty()).then(|| SystemMessage {
            content: StringOrBlocks::String(String::new()),
            sections: Some(sections),
            timestamp: chrono::Utc::now().timestamp_millis().unsigned_abs(),
            ..SystemMessage::default()
        })
    }
}

fn before_agent_start_message(value: &Value) -> Option<AgentMessage> {
    if value.get("role").is_some() {
        let mut candidate = value.clone();
        if let Some(obj) = candidate.as_object_mut()
            && !obj.contains_key("timestamp")
        {
            obj.insert(
                "timestamp".to_owned(),
                Value::from(chrono::Utc::now().timestamp_millis().unsigned_abs()),
            );
        }
        if let Ok(message) = serde_json::from_value::<AgentMessage>(candidate) {
            return Some(message);
        }
    }
    let obj = value.as_object()?;
    let custom_type = obj.get("customType")?.as_str()?.to_owned();
    let mut fields = serde_json::Map::new();
    fields.insert("customType".to_owned(), Value::String(custom_type));
    fields.insert(
        "content".to_owned(),
        obj.get("content")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    fields.insert(
        "display".to_owned(),
        Value::Bool(obj.get("display").and_then(Value::as_bool).unwrap_or(true)),
    );
    if let Some(details) = obj.get("details") {
        fields.insert("details".to_owned(), details.clone());
    }
    let timestamp = obj
        .get("timestamp")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().unsigned_abs());
    fields.insert("timestamp".to_owned(), Value::from(timestamp));
    AgentMessage::custom("custom", fields).ok()
}

impl AgentSession {
    /// Continue the conversation without adding a new user message.
    pub async fn continue_(&self) -> Result<()> {
        if self.lock().disposed {
            return Err(SessionError::Disposed);
        }
        if self.is_streaming() {
            return Err(SessionError::Agent(AgentError::ContinueInProgress));
        }
        let _run = self.begin_run()?;
        let result = self.agent().continue_run().await;
        match result {
            Ok(()) => self.after_run().await,
            Err(error) => Err(SessionError::Agent(error)),
        }
    }

    /// Queue a message to be delivered after the current assistant turn finishes its tools.
    pub async fn steer(&self, text: impl Into<String>) -> Result<PromptDisposition> {
        self.steer_with_images(text.into(), Vec::new()).await
    }

    pub async fn steer_with_images(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Result<PromptDisposition> {
        let (text, images) = self.normalize_images(text, images);
        let message = user_input(text.clone(), images);
        self.lock().steering.push(text);
        self.agent().steer(message);
        self.emit_queue_update();
        Ok(PromptDisposition::Queued)
    }

    /// Queue a message to be delivered once the agent would otherwise stop.
    pub async fn follow_up(&self, text: impl Into<String>) -> Result<PromptDisposition> {
        self.follow_up_with_images(text.into(), Vec::new()).await
    }

    pub async fn follow_up_with_images(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Result<PromptDisposition> {
        let (text, images) = self.normalize_images(text, images);
        let message = user_input(text.clone(), images);
        self.lock().follow_up.push(text);
        self.agent().follow_up(message);
        self.emit_queue_update();
        Ok(PromptDisposition::Queued)
    }

    fn normalize_images(
        &self,
        mut text: String,
        images: Vec<ImageContent>,
    ) -> (String, Vec<ImageContent>) {
        use crate::utils::{
            image_process::{ProcessImageOptions, ProcessImageResult, process_image},
            tool_result_images::NormalizeToolResultImagesOptions,
        };
        if images.is_empty() {
            return (text, images);
        }
        let auto_resize = self.settings().auto_resize_images();
        let model = self.model();
        let resize_options = model
            .input_limits
            .as_ref()
            .and_then(|limits| limits.images.as_ref())
            .and_then(|images| images.resize.as_ref())
            .and_then(|options| {
                NormalizeToolResultImagesOptions::from_model_options(options, auto_resize)
                    .resize_options
            });
        let options = ProcessImageOptions {
            auto_resize_images: Some(auto_resize),
            resize_options,
        };
        let mut normalized = Vec::with_capacity(images.len());
        let mut hints = Vec::new();
        for mut image in images {
            let bytes = STANDARD.decode(&image.data).unwrap_or_default();
            match process_image(&bytes, &image.mime_type, options.clone()) {
                ProcessImageResult::Ok {
                    data,
                    mime_type,
                    hints: image_hints,
                } => {
                    image.data = data;
                    image.mime_type = mime_type;
                    normalized.push(image);
                    hints.extend(image_hints);
                }
                ProcessImageResult::Err { message } => hints.push(message),
            }
        }
        if !hints.is_empty() {
            text.push_str("\n\n");
            text.push_str(&hints.join("\n"));
        }
        (text, normalized)
    }

    /// A bash result cannot be inserted between an assistant tool call and its result.
    pub fn record_bash_message(&self, message: AgentMessage) -> Result<()> {
        let mut inner = self.lock();
        if inner.is_streaming {
            inner.pending_bash.push(message);
        } else {
            inner.session_manager.try_append_message(&message)?;
            inner.agent.append_message(message);
        }
        Ok(())
    }

    fn begin_run(&self) -> Result<RunningSession<'_>> {
        let mut inner = self.lock();
        if inner.is_streaming {
            return Err(SessionError::StreamingBehaviorRequired);
        }
        inner.is_streaming = true;
        inner.run_settled = Some(CancellationToken::new());
        Ok(RunningSession(self))
    }

    fn finish_run(&self) {
        let (aborted, retry_attempt, settled, warmer) = {
            let mut inner = self.lock();
            inner.is_streaming = false;
            for message in std::mem::take(&mut inner.pending_bash) {
                match inner.session_manager.try_append_message(&message) {
                    Ok(_) => inner.agent.append_message(message),
                    Err(error) => eprintln!("{error}"),
                }
            }
            let aborted = std::mem::take(&mut inner.run_aborted);
            let retry_attempt = if aborted {
                std::mem::take(&mut inner.retry_attempt)
            } else {
                0
            };
            inner.retry_cancel = None;
            (
                aborted,
                retry_attempt,
                inner.run_settled.take(),
                inner.cache_warmer.clone(),
            )
        };
        warmer.on_agent_settled();
        self.emit(AgentSessionEvent::AgentSettled { aborted });
        if retry_attempt != 0 {
            self.emit(AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt: retry_attempt,
                final_error: Some("retry aborted".to_string()),
            });
        }
        if let Some(settled) = settled {
            settled.cancel();
        }
    }

    fn agent(&self) -> Agent {
        self.lock().agent.clone()
    }

    fn last_assistant_snapshot(&self) -> Option<AssistantSnapshot> {
        self.lock()
            .session_manager
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant(assistant) => Some(AssistantSnapshot {
                    retryable: assistant.stop_reason == StopReason::Error,
                    error_message: assistant.error_message.clone(),
                }),
                _ => None,
            })
    }

    async fn after_run(&self) -> Result<()> {
        if self.lock().run_aborted {
            return Ok(());
        }
        let mut retry_rounds = 0usize;
        while let Some(snapshot) = self.last_assistant_snapshot() {
            if !snapshot.retryable || self.lock().run_aborted {
                break;
            }
            if !self.prepare_retry(snapshot).await? {
                break;
            }
            retry_rounds += 1;
            if retry_rounds > self.lock().settings.max_retries() + 1 {
                break;
            }
            let prompt = self.lock().retry_prompt.clone();
            self.agent().prompt(prompt).await?;
        }
        if self.needs_compaction() {
            self.compact_with_reason(CompactionReason::Threshold, None)
                .await?;
        }
        Ok(())
    }

    async fn prepare_retry(&self, snapshot: AssistantSnapshot) -> Result<bool> {
        let (enabled, max_retries, base_delay) = {
            let inner = self.lock();
            (
                inner.settings.retry_enabled(),
                inner.settings.max_retries(),
                inner.settings.retry_base_delay_ms(),
            )
        };
        if !enabled || self.lock().retry_attempt >= max_retries {
            return Ok(false);
        }
        let attempt = {
            let mut inner = self.lock();
            inner.retry_attempt += 1;
            inner.retry_attempt
        };
        let delay_ms = base_delay.saturating_mul(1u64 << attempt.saturating_sub(1).min(20));
        self.emit(AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: max_retries,
            delay_ms,
            error_message: snapshot
                .error_message
                .unwrap_or_else(|| "Unknown error".to_string()),
        });
        let cancel = CancellationToken::new();
        {
            let mut inner = self.lock();
            inner.retry_cancel = Some(cancel.clone());
        }
        let cancelled = tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay_ms)) => false,
            () = cancel.cancelled() => true,
        };
        {
            let mut inner = self.lock();
            inner.retry_cancel = None;
        }
        if cancelled {
            self.emit(AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: None,
            });
            self.lock().retry_attempt = 0;
            return Ok(false);
        }
        Ok(true)
    }

    fn estimated_tokens(&self) -> u64 {
        self.lock()
            .session_manager
            .messages()
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |text| text.len() / 4) as u64)
            .sum()
    }

    fn needs_compaction(&self) -> bool {
        let compacting = {
            let inner = self.lock();
            inner.settings.compaction_enabled() && !inner.is_compacting
        };
        if !compacting {
            return false;
        }
        let model = self.agent().model();
        let window = model.context_window;
        let settings = self.settings();
        let limit = settings
            .compaction_reserve_tokens_for_model(&model.provider, &model.id)
            .map_or_else(
                || (window as f64 * settings.compaction_threshold()) as u64,
                |reserve| window.saturating_sub(reserve),
            );
        limit > 0 && self.estimated_tokens() >= limit
    }

    /// Compact the active context. Upstream supports manual, threshold, and overflow reasons.
    pub async fn compact(&self, custom_instructions: Option<&str>) -> Result<CompactionResult> {
        self.compact_with_reason(CompactionReason::Manual, custom_instructions)
            .await
    }

    async fn compact_with_reason(
        &self,
        reason: CompactionReason,
        instructions: Option<&str>,
    ) -> Result<CompactionResult> {
        let signal = CancellationToken::new();
        {
            let mut inner = self.lock();
            if inner.is_compacting {
                return Err(SessionError::CompactionInProgress);
            }
            inner.is_compacting = true;
            inner.compaction_cancel = Some(signal.clone());
        }
        self.emit(AgentSessionEvent::CompactionStart { reason });
        let (manager, compactor) = {
            let inner = self.lock();
            (inner.session_manager.clone(), inner.compactor.clone())
        };
        let result = compactor
            .compact(
                self.model(),
                manager.as_ref(),
                instructions.unwrap_or_default(),
                &signal,
            )
            .await
            .and_then(|compaction| {
                manager.try_append_compaction(&compaction)?;
                Ok(compaction)
            });
        let outcome = match result {
            Ok(compaction) => {
                let messages = manager.messages();
                self.agent().set_messages(messages);
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason,
                    result: Some(compaction.clone()),
                    error_message: None,
                    aborted: false,
                    will_retry: false,
                });
                Ok(compaction)
            }
            Err(error) => {
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    error_message: (!signal.is_cancelled()).then(|| error.to_string()),
                    aborted: signal.is_cancelled(),
                    will_retry: false,
                });
                Err(SessionError::CompactionFailed(error.to_string()))
            }
        };
        let mut inner = self.lock();
        inner.is_compacting = false;
        inner.compaction_cancel = None;
        outcome
    }

    /// Replace the current session manager and rebind the agent transcript.
    pub async fn switch_session(&self, manager: Arc<dyn SessionManager>) -> Result<()> {
        self.abort().await;
        self.emit(AgentSessionEvent::SessionShutdown {
            reason: SessionReason::Switch,
        });
        let messages = manager.messages();
        {
            let mut inner = self.lock();
            inner.session_manager = manager;
            inner.steering.clear();
            inner.follow_up.clear();
            inner.retry_attempt = 0;
            inner.agent.set_messages(messages);
            inner.agent.clear_all_queues();
        }
        self.emit_queue_update();
        self.emit(AgentSessionEvent::SessionStart {
            reason: SessionReason::Resume,
        });
        Ok(())
    }

    /// Start a fresh in-memory session, keeping the current cwd.
    pub async fn new_session(&self) -> Result<Arc<dyn SessionManager>> {
        let cwd = self.lock().cwd.clone();
        let manager: Arc<dyn SessionManager> = Arc::new(InMemorySessionManager::new(cwd));
        self.switch_session(manager.clone()).await?;
        self.emit(AgentSessionEvent::SessionStart {
            reason: SessionReason::New,
        });
        Ok(manager)
    }

    /// Fork the branch at an entry.
    pub async fn fork(&self, entry_id: &str, position: ForkPosition) -> Result<()> {
        {
            let inner = self.lock();
            if inner
                .session_manager
                .entries()
                .iter()
                .all(|entry| entry.id != entry_id)
            {
                return Err(SessionError::InvalidEntryId(entry_id.to_string()));
            }
        }
        self.abort().await;
        self.emit(AgentSessionEvent::SessionShutdown {
            reason: SessionReason::Fork,
        });
        {
            let inner = self.lock();
            inner.session_manager.fork(entry_id, position)?;
        }
        let messages = self.lock().session_manager.messages();
        self.agent().set_messages(messages);
        self.emit(AgentSessionEvent::SessionStart {
            reason: SessionReason::Fork,
        });
        Ok(())
    }

    /// Navigate to another entry of the session tree.
    pub async fn navigate_tree(&self, target_id: &str) -> Result<()> {
        {
            let inner = self.lock();
            inner.session_manager.navigate(target_id)?;
        }
        let messages = self.lock().session_manager.messages();
        self.agent().set_messages(messages);
        Ok(())
    }

    /// Navigate to another entry of the session tree, optionally summarizing abandoned messages
    /// using `branchSummary.reserveTokens`.
    pub async fn navigate_tree_with_summary(
        &self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<Option<super::compaction::BranchSummaryResult>> {
        if !summarize {
            self.navigate_tree(target_id).await?;
            return Ok(None);
        }
        let reserve_tokens = self.settings().branch_summary_reserve_tokens();
        let model = self.model();
        let mut options = super::compaction::BranchSummaryOptions::for_model(model.clone());
        options.reserve_tokens = reserve_tokens;
        options.custom_instructions = custom_instructions.map(str::to_owned);
        options.replace_instructions = custom_instructions.is_some();

        let messages = {
            let inner = self.lock();
            inner
                .session_manager
                .messages()
                .into_iter()
                .filter_map(|msg| serde_json::to_value(msg).ok())
                .collect::<Vec<_>>()
        };
        let summary_result = super::compaction::generate_branch_summary_with_options(
            &messages,
            &options,
            |_branch_messages| async move {
                // Delegate to session manager navigation + optional summary
                Ok::<_, String>(String::new())
            },
        )
        .await;
        self.navigate_tree(target_id).await?;
        Ok(Some(summary_result))
    }

    pub fn last_routed_model(&self) -> Option<(Model, Option<ThinkingLevel>)> {
        self.lock().models.last_routed_model()
    }

    pub fn is_using_subscription(&self, provider: &str) -> bool {
        self.lock().models.is_using_subscription(provider)
    }

    /// Set the model and persist a `model_change` entry.
    pub async fn set_model(&self, model: Model) -> Result<()> {
        let previous = self.agent().model();
        {
            let inner = self.lock();
            inner
                .session_manager
                .append_model_change(&model.provider, &model.id);
            inner.agent.set_model(model.clone());
            if !inner.auth.has_configured_auth(&model.provider) {
                return Err(SessionError::NoAuth(model.provider.clone()));
            }
        }
        self.emit(AgentSessionEvent::ModelSelect {
            provider: model.provider.clone(),
            id: model.id.clone(),
        });
        if previous.provider != model.provider || previous.id != model.id {
            let level = self.agent().thinking_level();
            self.set_thinking_level(level);
        }
        Ok(())
    }

    /// Set the thinking level and persist a `thinking_level_change` entry.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        {
            let inner = self.lock();
            inner.agent.set_thinking_level(level);
            inner.session_manager.append_thinking_level_change(level);
        }
        self.emit(AgentSessionEvent::ThinkingLevelChanged { level });
    }

    /// Reload settings and extensions for the current session.
    pub async fn reload(&self) -> Result<()> {
        self.abort().await;
        self.emit(AgentSessionEvent::SessionShutdown {
            reason: SessionReason::Reload,
        });
        {
            let inner = self.lock();
            inner
                .agent
                .set_steering_mode(inner.settings.steering_mode());
            inner
                .agent
                .set_follow_up_mode(inner.settings.follow_up_mode());
        }
        self.emit(AgentSessionEvent::SessionStart {
            reason: SessionReason::Reload,
        });
        Ok(())
    }

    /// Abort the active run, retry wait, and any in-flight compaction.
    pub async fn abort(&self) {
        {
            let mut inner = self.lock();
            inner.run_aborted = inner.is_streaming;
            if let Some(cancel) = &inner.retry_cancel {
                cancel.cancel();
            }
            if let Some(cancel) = &inner.compaction_cancel {
                cancel.cancel();
            }
            inner.agent.abort();
        }
        self.wait_for_idle().await;
    }

    pub fn abort_retry(&self) {
        if let Some(cancel) = &self.lock().retry_cancel {
            cancel.cancel();
        }
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        let inner = self.lock();
        inner.settings.set_steering_mode(mode);
        inner.agent.set_steering_mode(mode);
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        let inner = self.lock();
        inner.settings.set_follow_up_mode(mode);
        inner.agent.set_follow_up_mode(mode);
    }

    pub async fn wait_for_idle(&self) {
        let settled = self.lock().run_settled.clone();
        if let Some(settled) = settled {
            settled.cancelled().await;
            return;
        }
        let agent = self.agent();
        agent.wait_for_idle().await;
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        let settings = self.lock().settings.clone();
        settings.set_compaction_enabled(enabled);
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        let settings = self.lock().settings.clone();
        settings.set_retry_enabled(enabled);
    }

    pub fn is_streaming(&self) -> bool {
        self.lock().is_streaming
    }

    pub fn is_idle(&self) -> bool {
        let inner = self.lock();
        !inner.is_streaming && !inner.is_compacting && inner.retry_cancel.is_none()
    }

    pub fn is_compacting(&self) -> bool {
        self.lock().is_compacting
    }

    pub fn is_retrying(&self) -> bool {
        self.lock().retry_cancel.is_some()
    }

    pub fn retry_attempt(&self) -> usize {
        self.lock().retry_attempt
    }

    pub fn model(&self) -> Model {
        self.agent().model()
    }

    pub fn available_models(&self) -> Vec<Model> {
        self.lock().models.models()
    }

    pub fn scoped_models(&self) -> Vec<(Model, Option<ThinkingLevel>)> {
        self.lock().models.scoped_models()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.agent().thinking_level()
    }

    pub fn session_id(&self) -> String {
        self.lock().session_manager.session_id()
    }

    pub fn session_file(&self) -> Option<String> {
        self.lock().session_manager.session_file()
    }

    pub fn cwd(&self) -> String {
        self.lock().cwd.clone()
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.lock().session_manager.messages()
    }

    pub fn system_prompt(&self) -> String {
        let inner = self.lock();
        if let Some(sections) = &inner.system_prompt_sections {
            return pi_rs_ai::transcript::system_message_text(&SystemMessage {
                sections: Some(
                    sections
                        .iter()
                        .map(|(name, content)| (name.clone(), Some(content.clone())))
                        .collect(),
                ),
                ..SystemMessage::default()
            });
        }
        inner.agent.system_prompt()
    }

    pub fn session_manager(&self) -> Arc<dyn SessionManager> {
        self.lock().session_manager.clone()
    }

    pub fn settings(&self) -> Arc<dyn SettingsStore> {
        self.lock().settings.clone()
    }

    pub(crate) fn interactive_tools(&self) -> Vec<pi_rs_ai::Tool> {
        self.agent()
            .tools()
            .iter()
            .map(|tool| tool.declaration().clone())
            .collect()
    }

    pub(crate) fn interactive_resources(&self) -> Option<super::resource_loader::Resources> {
        self.lock().extensions.resources().cloned()
    }

    /// Session statistics including context usage and all-history token totals.
    pub fn stats(&self) -> SessionStats {
        let inner = self.lock();
        let counters = inner.session_manager.stats();
        let window = inner.agent.model().context_window;
        let latest_assistant = inner
            .session_manager
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            });
        let (tokens, percent) = match &latest_assistant {
            Some(assistant) if assistant.usage.total_tokens > 0.0 => {
                let tokens = assistant.usage.total_tokens as u64;
                (Some(tokens), Some(tokens as f64 / window as f64 * 100.0))
            }
            _ => (None, None),
        };
        SessionStats {
            session_file: inner.session_manager.session_file(),
            session_id: inner.session_manager.session_id(),
            user_messages: counters.user_messages,
            assistant_messages: counters.assistant_messages,
            tool_calls: counters.tool_calls,
            tool_results: counters.tool_results,
            total_messages: counters.total_messages,
            tokens: counters.tokens,
            cost: counters.cost,
            context_usage: Some(ContextUsage {
                tokens,
                context_window: window,
                percent,
            }),
        }
    }

    pub fn cache_warming_status(&self) -> super::cache_warmer::CacheWarmingStatus {
        self.lock().cache_warmer.status()
    }

    pub fn cache_warming_mode(&self) -> super::cache_warmer::CacheWarmingMode {
        self.lock().settings.cache_warming_mode()
    }

    pub fn on_cache_warming_mode_changed(&self) {
        self.lock().cache_warmer.on_mode_changed();
    }

    pub fn usage_cost_breakdown(&self) -> Vec<super::usage_totals::UsageCostBreakdownEntry> {
        let entries = self.lock().session_manager.entries();
        super::usage_totals::get_usage_cost_breakdown_from_entries(&entries)
    }

    pub fn cache_waste(&self) -> super::cache_stats::CacheWasteTotals {
        let (entries, models) = {
            let inner = self.lock();
            (inner.session_manager.entries(), inner.models.clone())
        };
        let prices = super::cache_stats::RegistryPriceAdapter(models.as_ref());
        super::cache_stats::compute_cache_waste(&entries, &prices)
    }

    pub fn detect_latest_cache_miss(
        &self,
        message: &pi_rs_ai::AssistantMessage,
    ) -> Option<super::cache_stats::CacheMiss> {
        let (entries, models) = {
            let inner = self.lock();
            (inner.session_manager.entries(), inner.models.clone())
        };
        let prices = super::cache_stats::RegistryPriceAdapter(models.as_ref());
        super::cache_stats::detect_cache_miss(&entries, message, &prices)
    }

    /// Release agent listeners and mark the session unusable.
    pub fn dispose(&self) {
        let mut inner = self.lock();
        if inner.disposed {
            return;
        }
        inner.disposed = true;
        inner.cache_warmer.cancel("session disposed");
        inner.agent_subscription.take();
        inner.listeners.clear();
        inner.agent.abort();
    }
}

fn user_input(text: String, images: Vec<ImageContent>) -> AgentMessage {
    let mut blocks = Vec::with_capacity(1 + images.len());
    blocks.push(InputContent::Text(TextContent {
        text,
        ..TextContent::default()
    }));
    blocks.extend(images.into_iter().map(InputContent::Image));
    UserMessage {
        role: UserRole::User,
        content: StringOrBlocks::Blocks(blocks),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            }),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_rs_agent::AgentInitialState;
    use pi_rs_ai::{
        Models, ModelsOptions, StopReason,
        faux::{
            FauxHandle, FauxOptions, FauxStep, faux_assistant_message, faux_provider, faux_text,
        },
    };

    use super::*;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    struct Fixture {
        session: AgentSession,
        faux: FauxHandle,
        settings: Arc<InMemorySettings>,
        manager: Arc<InMemorySessionManager>,
        extensions: Arc<RecordingExtensionRunner>,
        events: Arc<Mutex<Vec<AgentSessionEvent>>>,
        calls: Arc<AtomicUsize>,
    }

    fn fixture() -> TestResult<Fixture> {
        let faux = faux_provider(FauxOptions {
            api: Some("faux-session".into()),
            ..FauxOptions::default()
        });
        let model = faux.get_model(None).ok_or("faux model missing")?.clone();
        let models = Models::new(ModelsOptions::default());
        models.set_provider(faux.provider.clone());
        let stream_fn = pi_rs_agent::models_stream_fn(models);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let counting_stream: pi_rs_agent::StreamFn = Arc::new(move |model, context, options| {
            counted.fetch_add(1, Ordering::Relaxed);
            stream_fn(model, context, options)
        });
        let agent = Agent::new(pi_rs_agent::AgentOptions {
            initial_state: AgentInitialState {
                model: Some(model.clone()),
                system_prompt: Some("test".into()),
                thinking_level: Some(ThinkingLevel::Medium),
                ..AgentInitialState::default()
            },
            stream_fn: Some(counting_stream),
            ..pi_rs_agent::AgentOptions::default()
        })?;
        let settings = Arc::new(InMemorySettings::default());
        let manager = Arc::new(InMemorySessionManager::new("/tmp/pi-session-test"));
        let auth: Arc<dyn AuthStorage> = Arc::new(StaticAuth::allow_all());
        let registry: Arc<dyn ModelRegistry> =
            Arc::new(InMemoryModelRegistry::new(vec![model], auth.clone()));
        let extensions = Arc::new(RecordingExtensionRunner::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let session = AgentSession::new(AgentSessionConfig {
            agent,
            session_manager: manager.clone(),
            settings: settings.clone(),
            auth,
            models: registry,
            compactor: Arc::new(TailCompactor { keep_recent: 1 }),
            extensions: extensions.clone(),
            cwd: "/tmp/pi-session-test".into(),
            retry_prompt: None,
        })?;
        let sink = events.clone();
        session.subscribe(Arc::new(move |event| {
            if let Ok(mut sink) = sink.lock() {
                sink.push(event.clone());
            }
        }));
        Ok(Fixture {
            session,
            faux,
            settings,
            manager,
            extensions,
            events,
            calls,
        })
    }

    fn answer(text: &str) -> FauxStep {
        let mut message = faux_assistant_message(vec![faux_text(text)]);
        message.stop_reason = StopReason::Stop;
        message.into()
    }

    fn failed(text: &str) -> FauxStep {
        let mut message = faux_assistant_message(vec![faux_text(text)]);
        message.stop_reason = StopReason::Error;
        message.error_message = Some("provider error".to_string());
        message.into()
    }

    fn event_kinds(fixture: &Fixture) -> Vec<&'static str> {
        fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(AgentSessionEvent::kind)
            .collect()
    }

    #[tokio::test]
    async fn prompt_runs_the_agent_and_emits_lifecycle_events() -> TestResult {
        let fixture = fixture()?;
        fixture.faux.set_responses([answer("hello")]);
        assert_eq!(
            fixture
                .session
                .prompt("hi", PromptOptions::default())
                .await?,
            PromptDisposition::Started
        );
        assert!(event_kinds(&fixture).contains(&"agent_start"));
        assert!(event_kinds(&fixture).contains(&"agent_end"));
        assert!(event_kinds(&fixture).contains(&"agent_settled"));
        assert_eq!(fixture.faux.state.call_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn message_end_is_persisted_to_the_session_manager() -> TestResult {
        let fixture = fixture()?;
        fixture.faux.set_responses([answer("persisted")]);
        fixture
            .session
            .prompt("store this", PromptOptions::default())
            .await?;
        let messages = fixture.manager.messages();
        assert!(messages.iter().any(|message| message.role() == "user"));
        assert!(messages.iter().any(|message| match message {
            AgentMessage::Assistant(assistant) => assistant.content == [faux_text("persisted")],
            _ => false,
        }));
        Ok(())
    }

    #[tokio::test]
    async fn steering_queue_is_reported_and_drained() -> TestResult {
        let fixture = fixture()?;
        fixture
            .session
            .steer("interrupt")
            .await
            .expect("steer succeeds");
        let updates: Vec<_> = fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|event| matches!(event, AgentSessionEvent::QueueUpdate { .. }))
            .cloned()
            .collect();
        assert!(matches!(
            updates.last(),
            Some(AgentSessionEvent::QueueUpdate { steering, .. }) if steering == &["interrupt"]
        ));
        fixture.session.clear_queue();
        assert!(fixture.session.peek_steering().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn follow_up_queue_is_reported() -> TestResult {
        let fixture = fixture()?;
        fixture.session.follow_up("later").await?;
        assert_eq!(fixture.session.peek_follow_up(), ["later"]);
        Ok(())
    }

    #[tokio::test]
    async fn prompt_requires_streaming_behavior_while_streaming() -> TestResult {
        let fixture = fixture()?;
        fixture.session.lock().is_streaming = true;
        let error = fixture
            .session
            .prompt("second", PromptOptions::default())
            .await;
        assert!(matches!(
            error,
            Err(SessionError::StreamingBehaviorRequired)
        ));
        assert_eq!(
            fixture
                .session
                .prompt(
                    "queued",
                    PromptOptions {
                        streaming_behavior: Some(StreamingBehavior::FollowUp),
                        ..PromptOptions::default()
                    }
                )
                .await?,
            PromptDisposition::Queued
        );
        fixture.session.lock().is_streaming = false;
        Ok(())
    }

    #[tokio::test]
    async fn auto_retry_continues_after_error() -> TestResult {
        let fixture = fixture()?;
        fixture
            .faux
            .set_responses([failed("bad"), answer("recovered")]);
        fixture
            .session
            .prompt("try", PromptOptions::default())
            .await?;
        assert!(event_kinds(&fixture).contains(&"auto_retry_start"));
        assert!(event_kinds(&fixture).contains(&"auto_retry_end"));
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
        assert_eq!(fixture.session.retry_attempt(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn auto_retry_stops_at_max_attempts() -> TestResult {
        let fixture = fixture()?;
        fixture.settings.set_max_retries(1);
        fixture
            .faux
            .set_responses([failed("bad"), failed("still bad")]);
        fixture
            .session
            .prompt("try", PromptOptions::default())
            .await?;
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
        let kinds = event_kinds(&fixture);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "auto_retry_start")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn retry_can_be_disabled() -> TestResult {
        let fixture = fixture()?;
        fixture.settings.set_retry_enabled(false);
        fixture.faux.set_responses([failed("bad")]);
        fixture
            .session
            .prompt("try", PromptOptions::default())
            .await?;
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
        assert!(!event_kinds(&fixture).contains(&"auto_retry_start"));
        Ok(())
    }

    #[tokio::test]
    async fn auto_compaction_truncates_context_and_updates_stats() -> TestResult {
        let fixture = fixture()?;
        fixture.settings.set_compaction_threshold(0.00001);
        fixture
            .faux
            .set_responses([answer("a fairly long response for compaction")]);
        fixture
            .session
            .prompt("first", PromptOptions::default())
            .await?;
        assert!(event_kinds(&fixture).contains(&"compaction_start"));
        assert!(event_kinds(&fixture).contains(&"compaction_end"));
        let messages = fixture.session.messages();
        assert!(messages.iter().any(|message| match message {
            AgentMessage::User(user) => match &user.content {
                pi_rs_ai::StringOrBlocks::String(text) => text.contains("Summary"),
                _ => false,
            },
            _ => false,
        }));
        assert!(fixture.session.stats().context_usage.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn manual_compaction_reports_reason_and_summary() -> TestResult {
        let fixture = fixture()?;
        fixture.faux.set_responses([answer("one")]);
        fixture
            .session
            .prompt("first", PromptOptions::default())
            .await?;
        let result = fixture
            .session
            .compact(Some("focus on decisions"))
            .await
            .expect("compaction succeeds");
        assert!(result.summary.contains("focus on decisions"));
        assert!(
            fixture
                .events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .any(|event| matches!(
                    event,
                    AgentSessionEvent::CompactionStart {
                        reason: CompactionReason::Manual
                    }
                ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn set_model_and_thinking_level_are_recorded() -> TestResult {
        let fixture = fixture()?;
        let model = fixture.faux.get_model(None).ok_or("no model")?.clone();
        fixture.session.set_model(model.clone()).await?;
        fixture.session.set_thinking_level(ThinkingLevel::High);
        assert_eq!(fixture.session.thinking_level(), ThinkingLevel::High);
        assert!(
            fixture
                .manager
                .entries()
                .iter()
                .any(|entry| entry.kind == "model_change")
        );
        assert!(
            fixture
                .manager
                .entries()
                .iter()
                .any(|entry| entry.kind == "thinking_level_change")
        );
        Ok(())
    }

    #[tokio::test]
    async fn stats_reports_message_counts() -> TestResult {
        let fixture = fixture()?;
        fixture.faux.set_responses([answer("stats")]);
        fixture
            .session
            .prompt("count me", PromptOptions::default())
            .await?;
        let stats = fixture.session.stats();
        assert_eq!(stats.session_id, fixture.manager.session_id());
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.total_messages, 2);
        assert!(stats.context_usage.expect("usage").tokens.unwrap_or(0) > 0);
        Ok(())
    }

    #[tokio::test]
    async fn switch_session_rebinds_the_transcript() -> TestResult {
        let fixture = fixture()?;
        fixture.manager.append_message(&AgentMessage::user("old"));
        let replacement: Arc<dyn SessionManager> =
            Arc::new(InMemorySessionManager::new("/tmp/pi-session-test"));
        replacement.append_message(&AgentMessage::user("new"));
        fixture.session.switch_session(replacement).await?;
        assert_eq!(fixture.session.messages().len(), 1);
        assert!(event_kinds(&fixture).contains(&"session_shutdown"));
        Ok(())
    }

    #[tokio::test]
    async fn fork_and_navigate_tree_restore_a_branch() -> TestResult {
        let fixture = fixture()?;
        let first = fixture.manager.append_message(&AgentMessage::user("first"));
        let second = fixture
            .manager
            .append_message(&AgentMessage::user("second"));
        fixture.session.navigate_tree(&first).await?;
        assert_eq!(fixture.session.messages().len(), 1);
        let third = fixture.manager.append_message(&AgentMessage::user("third"));
        assert!(
            fixture
                .session
                .fork(&second, ForkPosition::At)
                .await
                .is_err()
        );
        fixture.session.fork(&third, ForkPosition::At).await?;
        assert!(matches!(
            fixture.session.fork("missing", ForkPosition::At).await,
            Err(SessionError::InvalidEntryId(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn dispose_is_idempotent_and_blocks_prompts() -> TestResult {
        let fixture = fixture()?;
        fixture.session.dispose();
        fixture.session.dispose();
        assert!(matches!(
            fixture.session.prompt("no", PromptOptions::default()).await,
            Err(SessionError::Disposed)
        ));
        Ok(())
    }

    #[test]
    fn extension_events_are_forwarded() -> TestResult {
        let fixture = fixture()?;
        let kinds: Vec<String> = fixture
            .extensions
            .events()
            .iter()
            .filter_map(|event| {
                event
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert!(kinds.contains(&"session_start".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn before_agent_start_async_updates_system_prompt_and_injects_message() -> TestResult {
        struct BeforeStartRunner;
        impl ExtensionRunner for BeforeStartRunner {
            fn emit(&self, _event: &Value) -> Value {
                Value::Null
            }
            fn emit_async<'a>(
                &'a self,
                event: Value,
            ) -> futures_util::future::BoxFuture<'a, Value> {
                Box::pin(async move {
                    if event["type"].as_str() == Some("before_agent_start") {
                        json!({
                            "sections": { "customSection": "from-async-ext" },
                            "systemPrompt": "Override System Prompt",
                            "messages": [{
                                "role": "user",
                                "content": "injected-by-hook"
                            }]
                        })
                    } else {
                        Value::Null
                    }
                })
            }
            fn has_handlers(&self, event_type: &str) -> bool {
                event_type == "before_agent_start"
            }
        }

        let faux = faux_provider(FauxOptions {
            api: Some("faux-before-start".into()),
            ..FauxOptions::default()
        });
        faux.set_responses([answer("done")]);
        let model = faux.get_model(None).ok_or("faux model missing")?.clone();
        let models = Models::new(ModelsOptions::default());
        models.set_provider(faux.provider.clone());
        let stream_fn = pi_rs_agent::models_stream_fn(models);
        let agent = Agent::new(pi_rs_agent::AgentOptions {
            initial_state: AgentInitialState {
                model: Some(model.clone()),
                system_prompt: Some("Original System".into()),
                ..AgentInitialState::default()
            },
            stream_fn: Some(stream_fn),
            ..pi_rs_agent::AgentOptions::default()
        })?;
        let auth: Arc<dyn AuthStorage> = Arc::new(StaticAuth::allow_all());
        let registry: Arc<dyn ModelRegistry> =
            Arc::new(InMemoryModelRegistry::new(vec![model], auth.clone()));
        let session = AgentSession::new(AgentSessionConfig {
            agent,
            session_manager: Arc::new(InMemorySessionManager::new("/tmp/pi-before-start")),
            settings: Arc::new(InMemorySettings::default()),
            auth,
            models: registry,
            compactor: Arc::new(TailCompactor { keep_recent: 1 }),
            extensions: Arc::new(BeforeStartRunner),
            cwd: "/tmp/pi-before-start".into(),
            retry_prompt: None,
        })?;
        session.prompt("hello", PromptOptions::default()).await?;
        assert!(session.system_prompt().contains("Override System Prompt"));
        let msgs = session.messages();
        assert!(msgs.iter().any(|m| {
            serde_json::to_string(m)
                .unwrap_or_default()
                .contains("injected-by-hook")
        }));
        Ok(())
    }
}
