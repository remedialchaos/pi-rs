//! Rust port of `packages/coding-agent/src/core/event-bus.ts` plus the typed
//! extension event contract described in `rust/research/wasm-extensions.md`.
//!
//! The bus is deliberately transport agnostic: payloads are `serde_json::Value`
//! so native Rust extensions and WASM guests share one wire format. Handlers are
//! async and return `Option<Value>` (an extension handler's patch/result).

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{Value, json};

/// Result of one extension event handler.
///
/// `Ok(None)` means the handler declined to change anything (`undefined` in the
/// TypeScript host); `Err` is a handler exception and is reported through the
/// bus error reporter, never propagated through dispatch.
pub type HandlerResult = Result<Option<Value>, String>;

/// Async event handler. Boxed so it is object-safe and can be stored behind an
/// `Arc` and cloned for each dispatch snapshot.
pub type EventHandler = Arc<dyn Fn(Value) -> BoxFuture<'static, HandlerResult> + Send + Sync>;

/// Every event name in the extension contract, verbatim. The order groups the
/// events the way the research note does; `as_str` is the external identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExtensionEvent {
    // Startup / resources (3)
    ProjectTrust,
    ResourcesDiscover,
    McpServersChange,
    // Session (10)
    SessionStart,
    SessionInfoChanged,
    SessionBeforeSwitch,
    SessionBeforeFork,
    SessionBeforeCompact,
    SessionCompact,
    SessionCompactFailed,
    SessionShutdown,
    SessionBeforeTree,
    SessionTree,
    // Agent / turn / provider / message (20)
    Context,
    ContextWithSystem,
    CacheWarmingDecision,
    BeforeProviderHeaders,
    BeforeProviderRequest,
    AfterProviderResponse,
    ProviderStreamEvent,
    BeforeAgentStart,
    AgentStart,
    AgentEnd,
    AgentBeforeSettle,
    AgentSettled,
    UiPromptStart,
    UiPromptEnd,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
    // Model / tool / input / UI (8)
    ModelSelect,
    ThinkingLevelSelect,
    UserBash,
    Input,
    ToolCall,
    ToolResult,
}

impl ExtensionEvent {
    /// The exact event name used by the upstream TypeScript host and WIT
    /// boundary. There are 41 variants and 41 distinct names.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectTrust => "project_trust",
            Self::ResourcesDiscover => "resources_discover",
            Self::McpServersChange => "mcp_servers_change",
            Self::SessionStart => "session_start",
            Self::SessionInfoChanged => "session_info_changed",
            Self::SessionBeforeSwitch => "session_before_switch",
            Self::SessionBeforeFork => "session_before_fork",
            Self::SessionBeforeCompact => "session_before_compact",
            Self::SessionCompact => "session_compact",
            Self::SessionCompactFailed => "session_compact_failed",
            Self::SessionShutdown => "session_shutdown",
            Self::SessionBeforeTree => "session_before_tree",
            Self::SessionTree => "session_tree",
            Self::Context => "context",
            Self::ContextWithSystem => "context_with_system",
            Self::CacheWarmingDecision => "cache_warming_decision",
            Self::BeforeProviderHeaders => "before_provider_headers",
            Self::BeforeProviderRequest => "before_provider_request",
            Self::AfterProviderResponse => "after_provider_response",
            Self::ProviderStreamEvent => "provider_stream_event",
            Self::BeforeAgentStart => "before_agent_start",
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::AgentBeforeSettle => "agent_before_settle",
            Self::AgentSettled => "agent_settled",
            Self::UiPromptStart => "ui_prompt_start",
            Self::UiPromptEnd => "ui_prompt_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolExecutionStart => "tool_execution_start",
            Self::ToolExecutionUpdate => "tool_execution_update",
            Self::ToolExecutionEnd => "tool_execution_end",
            Self::ModelSelect => "model_select",
            Self::ThinkingLevelSelect => "thinking_level_select",
            Self::UserBash => "user_bash",
            Self::Input => "input",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    /// Parse an event name. Unknown names are rejected; `ExtensionEvent` is a
    /// closed set so a typo in a WIT boundary cannot introduce a phantom event.
    pub fn parse(name: &str) -> Result<Self, String> {
        for event in Self::all() {
            if event.as_str() == name {
                return Ok(event);
            }
        }
        Err(format!("unknown extension event: {name}"))
    }

    /// All 41 events in canonical order.
    pub fn all() -> [Self; 41] {
        [
            Self::ProjectTrust,
            Self::ResourcesDiscover,
            Self::McpServersChange,
            Self::SessionStart,
            Self::SessionInfoChanged,
            Self::SessionBeforeSwitch,
            Self::SessionBeforeFork,
            Self::SessionBeforeCompact,
            Self::SessionCompact,
            Self::SessionCompactFailed,
            Self::SessionShutdown,
            Self::SessionBeforeTree,
            Self::SessionTree,
            Self::Context,
            Self::ContextWithSystem,
            Self::CacheWarmingDecision,
            Self::BeforeProviderHeaders,
            Self::BeforeProviderRequest,
            Self::AfterProviderResponse,
            Self::ProviderStreamEvent,
            Self::BeforeAgentStart,
            Self::AgentStart,
            Self::AgentEnd,
            Self::AgentBeforeSettle,
            Self::AgentSettled,
            Self::UiPromptStart,
            Self::UiPromptEnd,
            Self::TurnStart,
            Self::TurnEnd,
            Self::MessageStart,
            Self::MessageUpdate,
            Self::MessageEnd,
            Self::ToolExecutionStart,
            Self::ToolExecutionUpdate,
            Self::ToolExecutionEnd,
            Self::ModelSelect,
            Self::ThinkingLevelSelect,
            Self::UserBash,
            Self::Input,
            Self::ToolCall,
            Self::ToolResult,
        ]
    }
}

impl std::fmt::Display for ExtensionEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error reporter installed by the runner. Upstream logs with `console.error`;
/// here the host decides where the message goes (TUI, RPC, or stderr).
pub type ErrorReporter = Arc<dyn Fn(String) + Send + Sync>;

struct Listener {
    id: usize,
    event: ExtensionEvent,
    handler: EventHandler,
}

/// Async listener registry with ordered dispatch and a pre-load transaction.
///
/// Dispatch always snapshots the listener list first. Handlers that remove or
/// add listeners during a dispatch therefore do not affect the current pass,
/// matching the Node `EventEmitter` behavior the upstream relies on.
pub struct EventBus {
    listeners: RwLock<Vec<Listener>>,
    pending: Mutex<Option<Vec<Listener>>>,
    next_id: AtomicUsize,
    error_reporter: Mutex<Option<ErrorReporter>>,
    ui_prompt_depth: AtomicUsize,
    active_ui_prompt: Mutex<Option<(String, Option<String>)>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            listeners: RwLock::new(Vec::new()),
            pending: Mutex::new(None),
            next_id: AtomicUsize::new(1),
            error_reporter: Mutex::new(None),
            ui_prompt_depth: AtomicUsize::new(0),
            active_ui_prompt: Mutex::new(None),
        }
    }

    pub fn set_error_reporter(&self, reporter: ErrorReporter) {
        *self
            .error_reporter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reporter);
    }

    fn report_error(&self, event: ExtensionEvent, error: &str) {
        let message = format!("Event handler error ({event}): {error}");
        if let Some(reporter) = self
            .error_reporter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            reporter(message.clone());
        }
    }

    /// Register a handler and return an unsubscribe token.
    ///
    /// While a load transaction is open the listener is staged and only becomes
    /// visible after `commit`. A failed load calls `rollback`, which drops every
    /// staged listener (the upstream `trackEventBusSubscription` behavior).
    pub fn on<F, Fut>(&self, event: ExtensionEvent, handler: F) -> usize
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let listener = Listener {
            id,
            event,
            handler: Arc::new(move |data| Box::pin(handler(data))),
        };
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(staged) = pending.as_mut() {
            staged.push(listener);
        } else {
            drop(pending);
            self.listeners
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(listener);
        }
        id
    }

    /// Register a pre-built handler (used by the extension registration layer).
    pub fn on_handler(&self, event: ExtensionEvent, handler: EventHandler) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let listener = Listener { id, event, handler };
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(staged) = pending.as_mut() {
            staged.push(listener);
        } else {
            drop(pending);
            self.listeners
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(listener);
        }
        id
    }

    /// Remove a listener by token. Idempotent.
    pub fn off(&self, id: usize) {
        if let Some(staged) = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            staged.retain(|listener| listener.id != id);
        }
        self.listeners
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|listener| listener.id != id);
    }

    pub fn begin_transaction(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.is_none() {
            *pending = Some(Vec::new());
        }
    }

    pub fn commit(&self) {
        let staged = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_default();
        if !staged.is_empty() {
            self.listeners
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend(staged);
        }
    }

    pub fn rollback(&self) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub fn listener_count(&self) -> usize {
        self.listeners
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn clear(&self) {
        self.listeners
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn snapshot(&self, event: ExtensionEvent) -> Vec<EventHandler> {
        self.listeners
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|listener| listener.event == event)
            .map(|listener| Arc::clone(&listener.handler))
            .collect()
    }

    /// An ordered snapshot of the handlers for `event`. The runner uses this
    /// for dispatch modes that need host-side validation (role checks,
    /// `structuredContent` invalidation, input `handled` short-circuiting).
    pub fn handlers(&self, event: ExtensionEvent) -> Vec<EventHandler> {
        self.snapshot(event)
    }

    async fn invoke(
        &self,
        event: ExtensionEvent,
        handler: &EventHandler,
        data: Value,
    ) -> Option<Value> {
        match handler(data).await {
            Ok(result) => result,
            Err(error) => {
                self.report_error(event, &error);
                None
            }
        }
    }

    /// Call one snapshotted handler with error reporting. Used by the runner
    /// for dispatch modes that need host-side validation between handlers.
    pub async fn call(
        &self,
        event: ExtensionEvent,
        handler: &EventHandler,
        data: Value,
    ) -> Option<Value> {
        self.invoke(event, handler, data).await
    }

    /// Fire-and-forget dispatch. Returns the error messages observed, if any.
    /// Handler exceptions are isolated: one failing handler never blocks the
    /// rest.
    pub async fn emit(&self, event: ExtensionEvent, data: Value) -> Vec<String> {
        let handlers = self.snapshot(event);
        let mut errors = Vec::new();
        for handler in handlers {
            match handler(data.clone()).await {
                Ok(_) => {}
                Err(error) => {
                    self.report_error(event, &error);
                    errors.push(error);
                }
            }
        }
        errors
    }

    /// Short-circuit first-winner dispatch. The first handler result satisfying
    /// `decisive` wins and later handlers are not called.
    ///
    /// Used for `project_trust`, `session_before_switch` (cancel), `tool_call`
    /// (block), and `user_bash`.
    pub async fn emit_first(
        &self,
        event: ExtensionEvent,
        data: Value,
        decisive: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        for handler in self.snapshot(event) {
            if let Some(result) = self.invoke(event, &handler, data.clone()).await
                && decisive(&result)
            {
                return Some(result);
            }
        }
        None
    }

    /// Chained transform dispatch. Each handler result is folded into `state`
    /// by `apply`; later handlers see earlier changes.
    ///
    /// Used for `before_provider_request`, `message_end`, `tool_result`, and
    /// `input` (transform phase).
    pub async fn emit_chain(
        &self,
        event: ExtensionEvent,
        data: Value,
        apply: impl Fn(&mut Value, &Value),
    ) -> Value {
        let mut state = data;
        for handler in self.snapshot(event) {
            if let Some(result) = self.invoke(event, &handler, state.clone()).await {
                apply(&mut state, &result);
            }
        }
        state
    }

    /// Chained transform with an early stop. `apply` returns `true` to stop;
    /// used by `input`, where `action: "handled"` wins immediately after any
    /// preceding transforms have been folded in.
    pub async fn emit_chain_until(
        &self,
        event: ExtensionEvent,
        data: Value,
        apply: impl Fn(&mut Value, &Value) -> bool,
    ) -> (Value, bool) {
        let mut state = data;
        for handler in self.snapshot(event) {
            if let Some(result) = self.invoke(event, &handler, state.clone()).await
                && apply(&mut state, &result)
            {
                return (state, true);
            }
        }
        (state, false)
    }

    /// Last-override-wins dispatch. Every handler is called; the last non-null
    /// extracted value is returned.
    ///
    /// Used for `cache_warming_decision`.
    pub async fn emit_last(
        &self,
        event: ExtensionEvent,
        data: Value,
        extract: impl Fn(&Value) -> Option<Value>,
    ) -> Option<Value> {
        let mut winner = None;
        for handler in self.snapshot(event) {
            if let Some(result) = self.invoke(event, &handler, data.clone()).await
                && let Some(value) = extract(&result)
            {
                winner = Some(value);
            }
        }
        winner
    }

    /// Coalesced blocking-UI lifecycle. The first nested prompt emits
    /// `ui_prompt_start`, the last one to finish emits `ui_prompt_end`, with the
    /// outermost prompt's kind and title. Microtask coalescing in the upstream
    /// host is represented here by the depth counter.
    pub async fn with_ui_prompt<T, Fut>(&self, kind: &str, title: Option<String>, run: Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let outer = self.ui_prompt_depth.fetch_add(1, Ordering::SeqCst) == 0;
        if outer {
            *self
                .active_ui_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some((kind.to_string(), title.clone()));
            let mut data = json!({ "reason": "ui_prompt", "kind": kind });
            if let Some(title) = &title {
                data["title"] = json!(title);
            }
            self.emit(ExtensionEvent::UiPromptStart, data).await;
        }

        let output = run.await;

        if self.ui_prompt_depth.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.ui_prompt_depth.store(0, Ordering::SeqCst);
            let active = self
                .active_ui_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .unwrap_or_else(|| (kind.to_string(), title.clone()));
            let (kind, title) = active;
            let mut data = json!({ "reason": "ui_prompt", "kind": kind });
            if let Some(title) = title {
                data["title"] = json!(title);
            }
            self.emit(ExtensionEvent::UiPromptEnd, data).await;
        }
        output
    }

    pub fn ui_prompt_depth(&self) -> usize {
        self.ui_prompt_depth.load(Ordering::SeqCst)
    }

    /// Whether any handler is registered for `event` (including staged ones).
    pub fn has_handlers(&self, event: ExtensionEvent) -> bool {
        if self
            .listeners
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|listener| listener.event == event)
        {
            return true;
        }
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|staged| staged.iter().any(|listener| listener.event == event))
    }

    pub fn handler_names() -> Vec<&'static str> {
        ExtensionEvent::all()
            .into_iter()
            .map(ExtensionEvent::as_str)
            .collect()
    }
}

/// Convenience for collecting the 41 event names into an insertion-ordered map.
pub fn event_name_index() -> IndexMap<&'static str, ExtensionEvent> {
    ExtensionEvent::all()
        .into_iter()
        .map(|event| (event.as_str(), event))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn handler<T: Send + Sync + 'static>(
        seen: Arc<Mutex<Vec<Value>>>,
        value: T,
        to_value: impl Fn(&T) -> Value + Send + Sync + 'static,
    ) -> EventHandler {
        Arc::new(move |data| {
            let seen = Arc::clone(&seen);
            let output = to_value(&value);
            Box::pin(async move {
                seen.lock().expect("seen poisoned").push(data);
                Ok(Some(output))
            })
        })
    }

    #[test]
    fn all_events_match_the_contract() {
        let names = EventBus::handler_names();
        assert_eq!(names.len(), 41);
        assert_eq!(names[0], "project_trust");
        assert_eq!(names[1], "resources_discover");
        assert_eq!(names[2], "mcp_servers_change");
        assert_eq!(names.last().copied(), Some("tool_result"));
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), 41);
        assert_eq!(
            ExtensionEvent::parse("ui_prompt_end"),
            Ok(ExtensionEvent::UiPromptEnd)
        );
        assert!(ExtensionEvent::parse("not_an_event").is_err());
    }

    #[tokio::test]
    async fn emit_reports_handler_errors_but_continues() {
        let bus = EventBus::new();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&errors);
        bus.set_error_reporter(Arc::new(move |message| {
            recorder.lock().expect("errors poisoned").push(message);
        }));
        let seen = Arc::new(Mutex::new(Vec::new()));
        bus.on_handler(
            ExtensionEvent::AgentStart,
            Arc::new(|_| Box::pin(async { Err("boom".to_string()) })),
        );
        bus.on_handler(
            ExtensionEvent::AgentStart,
            handler(Arc::clone(&seen), json!({"ok": true}), |v| v.clone()),
        );
        let reported = bus.emit(ExtensionEvent::AgentStart, json!({})).await;
        assert_eq!(reported, vec!["boom".to_string()]);
        assert_eq!(seen.lock().expect("seen poisoned").len(), 1);
        assert!(errors.lock().expect("errors poisoned")[0].contains("agent_start"));
    }

    #[tokio::test]
    async fn first_winner_short_circuits() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for decision in ["undecided", "yes", "no"] {
            let calls = Arc::clone(&calls);
            bus.on_handler(
                ExtensionEvent::ProjectTrust,
                Arc::new(move |_| {
                    let calls = Arc::clone(&calls);
                    let decision = decision.to_string();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(Some(json!({ "trusted": decision })))
                    })
                }),
            );
        }
        let winner = bus
            .emit_first(ExtensionEvent::ProjectTrust, json!({}), |value| {
                value["trusted"] != "undecided"
            })
            .await;
        assert_eq!(winner, Some(json!({ "trusted": "yes" })));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chain_folds_in_order() {
        let bus = EventBus::new();
        for suffix in ["a", "b"] {
            bus.on_handler(
                ExtensionEvent::BeforeProviderRequest,
                Arc::new(move |state| {
                    let suffix = suffix.to_string();
                    Box::pin(async move {
                        let mut state = state;
                        state["text"] = json!(format!(
                            "{}{}",
                            state["text"].as_str().unwrap_or(""),
                            suffix
                        ));
                        Ok(Some(state))
                    })
                }),
            );
        }
        let final_state = bus
            .emit_chain(
                ExtensionEvent::BeforeProviderRequest,
                json!({ "text": "" }),
                |state, result| {
                    if let Some(text) = result["text"].as_str() {
                        state["text"] = json!(text);
                    }
                },
            )
            .await;
        assert_eq!(final_state["text"], "ab");
    }

    #[tokio::test]
    async fn last_override_wins() {
        let bus = EventBus::new();
        for action in ["warm", "skip", "cancel"] {
            bus.on_handler(
                ExtensionEvent::CacheWarmingDecision,
                Arc::new(move |_| {
                    let action = action.to_string();
                    Box::pin(async move { Ok(Some(json!({ "action": action }))) })
                }),
            );
        }
        let winner = bus
            .emit_last(ExtensionEvent::CacheWarmingDecision, json!({}), |value| {
                value.get("action").cloned()
            })
            .await;
        assert_eq!(winner, Some(json!("cancel")));
    }

    #[tokio::test]
    async fn ui_prompt_depth_is_coalesced() {
        let bus = EventBus::new();
        let starts = Arc::new(AtomicUsize::new(0));
        let ends = Arc::new(AtomicUsize::new(0));
        let start_counter = Arc::clone(&starts);
        let end_counter = Arc::clone(&ends);
        bus.on_handler(
            ExtensionEvent::UiPromptStart,
            Arc::new(move |_| {
                let counter = Arc::clone(&start_counter);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            }),
        );
        bus.on_handler(
            ExtensionEvent::UiPromptEnd,
            Arc::new(move |_| {
                let counter = Arc::clone(&end_counter);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            }),
        );
        bus.with_ui_prompt("select", Some("outer".to_string()), async {
            assert_eq!(bus.ui_prompt_depth(), 1);
            bus.with_ui_prompt("confirm", Some("inner".to_string()), async {
                assert_eq!(bus.ui_prompt_depth(), 2);
            })
            .await;
            assert_eq!(bus.ui_prompt_depth(), 1);
        })
        .await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(ends.load(Ordering::SeqCst), 1);
        assert_eq!(bus.ui_prompt_depth(), 0);
    }

    #[tokio::test]
    async fn transaction_commits_and_rolls_back_listeners() {
        let bus = EventBus::new();
        bus.begin_transaction();
        bus.on(ExtensionEvent::SessionStart, |_| {
            Box::pin(async { Ok(None) })
        });
        assert_eq!(bus.listener_count(), 0);
        bus.rollback();
        assert_eq!(bus.listener_count(), 0);

        bus.begin_transaction();
        let id = bus.on(ExtensionEvent::SessionStart, |_| {
            Box::pin(async { Ok(None) })
        });
        bus.commit();
        assert_eq!(bus.listener_count(), 1);
        bus.off(id);
        assert_eq!(bus.listener_count(), 0);
    }

    #[tokio::test]
    async fn dispatch_snapshots_ignore_registrations_during_emit() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let bus_for_handler = Arc::new(EventBus::new());
        let inner = Arc::clone(&bus_for_handler);
        bus.on_handler(
            ExtensionEvent::TurnStart,
            Arc::new(move |_| {
                let counter = Arc::clone(&counter);
                let inner = Arc::clone(&inner);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    inner.on(ExtensionEvent::TurnStart, |_| Box::pin(async { Ok(None) }));
                    Ok(None)
                })
            }),
        );
        bus.emit(ExtensionEvent::TurnStart, json!({})).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(bus_for_handler.listener_count(), 1);
    }
}
