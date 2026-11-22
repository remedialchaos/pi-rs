//! Context hierarchy implementations. Every UI entry point is injectable so
//! the whole extension host is testable without a terminal or RPC transport.

use std::future::Future;
use std::sync::Arc;

use serde_json::Value;

use super::types::{
    AsyncFn, ConfirmFn, ContextUsage, ExecuteToolFn, ExtensionCommandContext, ExtensionContext,
    ExtensionMode, ExtensionToolContext, ExtensionUi, InputFn, NewSessionFn, NotifyFn, ReloadFn,
    SelectFn, SimpleSessionFn, ToolDefinition,
};

/// Host-provided UI callbacks. `select`, `confirm`, and `input` are async;
/// `notify` is fire-and-forget. Tests supply in-memory implementations.
pub struct UiInject {
    pub select: SelectFn,
    pub confirm: ConfirmFn,
    pub input: InputFn,
    pub notify: NotifyFn,
}

impl UiInject {
    /// An implementation where every dialog returns a fixed answer.
    pub fn fixed(selection: Option<String>, confirmation: bool, input: Option<String>) -> Self {
        Self {
            select: Arc::new(move |_, _| {
                let selection = selection.clone();
                Box::pin(async move { selection })
            }),
            confirm: Arc::new(move |_, _| {
                let confirmation = confirmation;
                Box::pin(async move { confirmation })
            }),
            input: Arc::new(move |_, _| {
                let input = input.clone();
                Box::pin(async move { input })
            }),
            notify: Arc::new(|_, _| {}),
        }
    }
}

impl Default for UiInject {
    fn default() -> Self {
        Self::fixed(None, false, None)
    }
}

impl ExtensionUi {
    /// Build a UI context for the given mode.
    ///
    /// - `tui`: full dialog-capable UI.
    /// - `rpc`: dialogs and notifications use the injected JSONL callbacks;
    ///   component factories are ignored (there is no terminal to draw on).
    /// - `json` / `print`: no-op UI, `has_ui == false`.
    pub fn from_inject(mode: ExtensionMode, inject: UiInject) -> Self {
        if mode.has_ui() {
            Self {
                has_ui: true,
                mode,
                select: inject.select,
                confirm: inject.confirm,
                input: inject.input,
                notify: inject.notify,
            }
        } else {
            Self::no_op(mode)
        }
    }

    pub fn no_op(mode: ExtensionMode) -> Self {
        Self {
            has_ui: false,
            mode,
            select: Arc::new(|_, _| Box::pin(async { None })),
            confirm: Arc::new(|_, _| Box::pin(async { false })),
            input: Arc::new(|_, _| Box::pin(async { None })),
            notify: Arc::new(|_, _| {}),
        }
    }

    pub async fn select(&self, title: impl Into<String>, options: Vec<String>) -> Option<String> {
        (self.select)(title.into(), options).await
    }

    pub async fn confirm(&self, title: impl Into<String>, message: impl Into<String>) -> bool {
        (self.confirm)(title.into(), message.into()).await
    }

    pub async fn input(
        &self,
        title: impl Into<String>,
        placeholder: Option<String>,
    ) -> Option<String> {
        (self.input)(title.into(), placeholder).await
    }

    pub fn notify(&self, message: impl Into<String>, kind: &str) {
        (self.notify)(message.into(), kind.to_string());
    }
}

impl ExtensionContext {
    /// Fail fast with the stale-context message after a reload or session
    /// replacement. Called at re-entrancy boundaries such as `execute_tool`.
    pub fn check_active(&self) -> Result<(), String> {
        self.host.assert_active()
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn mode(&self) -> ExtensionMode {
        self.mode
    }

    pub fn has_ui(&self) -> bool {
        self.ui.has_ui
    }

    pub fn is_idle(&self) -> bool {
        (self.is_idle)()
    }

    pub fn is_project_trusted(&self) -> bool {
        (self.is_project_trusted)()
    }

    pub fn has_pending_messages(&self) -> bool {
        (self.has_pending_messages)()
    }

    pub fn get_context_usage(&self) -> Option<ContextUsage> {
        (self.get_context_usage)()
    }

    pub fn get_system_prompt(&self) -> String {
        (self.get_system_prompt)()
    }

    pub fn abort(&self) {
        (self.abort)();
    }

    pub fn shutdown(&self) {
        (self.shutdown)();
    }

    pub fn compact(&self, options: Option<Value>) {
        (self.compact)(options);
    }

    pub fn signal(&self) -> Option<Value> {
        None
    }

    /// Blocking UI call wrapped in the coalesced `ui_prompt_start/end`
    /// lifecycle. Nested prompts increment a depth counter so only the
    /// outermost prompt emits the lifecycle events.
    pub async fn select_prompt(
        &self,
        title: impl Into<String>,
        options: Vec<String>,
    ) -> Option<String> {
        let title = title.into();
        let call = (self.ui.select)(title.clone(), options);
        self.prompt("select", Some(title), call).await
    }

    pub async fn confirm_prompt(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> bool {
        let title = title.into();
        let call = (self.ui.confirm)(title.clone(), message.into());
        self.prompt("confirm", Some(title), call).await
    }

    pub async fn input_prompt(
        &self,
        title: impl Into<String>,
        placeholder: Option<String>,
    ) -> Option<String> {
        let title = title.into();
        let call = (self.ui.input)(title.clone(), placeholder);
        self.prompt("input", Some(title), call).await
    }

    async fn prompt<T, Fut>(&self, kind: &str, title: Option<String>, call: Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        match &self.bus {
            Some(bus) => bus.with_ui_prompt(kind, title, call).await,
            None => call.await,
        }
    }
}

impl ExtensionToolContext {
    /// Replace the nested tool executor. Used by the runner when binding the
    /// session's tool dispatcher.
    pub fn with_execute_tool(mut self, execute: ExecuteToolFn) -> Self {
        self.execute_tool_fn = Some(execute);
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    /// The id assigned to the next nested call, without consuming the counter.
    pub fn next_nested_id(&self) -> String {
        format!("{}/{}", self.tool_call_id, 1)
    }
}

impl ExtensionCommandContext {
    pub async fn wait_for_idle(&self) {
        (self.wait_for_idle)().await
    }

    pub async fn new_session(&self, options: Option<Value>) -> Result<Value, String> {
        self.base.check_active()?;
        (self.new_session)(options).await
    }

    pub async fn fork(&self, entry_id: &str, options: Option<Value>) -> Result<Value, String> {
        self.base.check_active()?;
        (self.fork)(entry_id.to_string(), options).await
    }

    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: Option<Value>,
    ) -> Result<Value, String> {
        self.base.check_active()?;
        (self.navigate_tree)(target_id.to_string(), options).await
    }

    pub async fn switch_session(
        &self,
        session_path: &str,
        options: Option<Value>,
    ) -> Result<Value, String> {
        self.base.check_active()?;
        (self.switch_session)(session_path.to_string(), options).await
    }

    pub async fn reload(&self) -> Result<(), String> {
        self.base.check_active()?;
        (self.reload)().await
    }
}

/// Build the no-op session-control functions used when no mode has bound them.
pub fn default_command_actions() -> (
    AsyncFn<()>,
    NewSessionFn,
    SimpleSessionFn,
    SimpleSessionFn,
    SimpleSessionFn,
    ReloadFn,
) {
    (
        Arc::new(|| Box::pin(async {})),
        Arc::new(|_| Box::pin(async { Ok(serde_json::json!({ "cancelled": false })) })),
        Arc::new(|_, _| Box::pin(async { Ok(serde_json::json!({ "cancelled": false })) })),
        Arc::new(|_, _| Box::pin(async { Ok(serde_json::json!({ "cancelled": false })) })),
        Arc::new(|_, _| Box::pin(async { Ok(serde_json::json!({ "cancelled": false })) })),
        Arc::new(|| Box::pin(async { Ok(()) })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event_bus::EventBus;
    use crate::core::extensions::types::{ExecuteToolOptions, ExtensionHost, ToolOutcome};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn base_context(bus: Option<Arc<EventBus>>) -> ExtensionContext {
        let host = Arc::new(ExtensionHost::new(
            bus.clone().unwrap_or_else(|| Arc::new(EventBus::new())),
        ));
        ExtensionContext {
            host,
            cwd: "/work".to_string(),
            mode: ExtensionMode::Tui,
            ui: ExtensionUi::no_op(ExtensionMode::Tui),
            bus,
            is_idle: Arc::new(|| true),
            is_project_trusted: Arc::new(|| true),
            has_pending_messages: Arc::new(|| false),
            get_context_usage: Arc::new(|| None),
            get_system_prompt: Arc::new(|| "prompt".to_string()),
            abort: Arc::new(|| {}),
            shutdown: Arc::new(|| {}),
            compact: Arc::new(|_| {}),
        }
    }

    #[tokio::test]
    async fn json_and_print_modes_disable_ui() {
        for mode in [ExtensionMode::Json, ExtensionMode::Print] {
            let ui = ExtensionUi::from_inject(
                mode,
                UiInject::fixed(Some("x".into()), true, Some("y".into())),
            );
            assert!(!ui.has_ui);
            assert_eq!(ui.select("t", vec![]).await, None);
            assert!(!ui.confirm("t", "m").await);
            assert_eq!(ui.input("t", None).await, None);
        }
    }

    #[tokio::test]
    async fn tui_and_rpc_modes_use_injected_dialogs() {
        for mode in [ExtensionMode::Tui, ExtensionMode::Rpc] {
            let ui = ExtensionUi::from_inject(
                mode,
                UiInject::fixed(Some("x".into()), true, Some("y".into())),
            );
            assert!(ui.has_ui);
            assert_eq!(ui.select("t", vec![]).await.as_deref(), Some("x"));
            assert!(ui.confirm("t", "m").await);
            assert_eq!(ui.input("t", None).await.as_deref(), Some("y"));
        }
    }

    #[tokio::test]
    async fn nested_execute_tool_generates_parent_child_ids() {
        let bus = Arc::new(EventBus::new());
        let context = base_context(Some(bus));
        let tool_context = ExtensionToolContext::new(context, "call-42");
        let executor: ExecuteToolFn = Arc::new(move |id, name, _, _| {
            Box::pin(
                async move { ToolOutcome::success(json!({ "id": id }), json!({ "name": name })) },
            )
        });
        let tool_context = tool_context.with_execute_tool(executor);
        let first = tool_context
            .execute_tool("read", json!({}), ExecuteToolOptions::default())
            .await;
        let second = tool_context
            .execute_tool("bash", json!({}), ExecuteToolOptions::default())
            .await;
        assert_eq!(first.tool_call["id"], "call-42/1");
        assert_eq!(second.tool_call["id"], "call-42/2");
        assert!(!first.is_error);
    }

    #[tokio::test]
    async fn nested_execute_tool_without_executor_is_an_error_value() {
        let context = base_context(None);
        let tool_context = ExtensionToolContext::new(context, "root");
        let outcome = tool_context
            .execute_tool("unknown", json!({}), ExecuteToolOptions::default())
            .await;
        assert!(outcome.is_error);
        assert_eq!(outcome.tool_call["id"], "root/0");
    }

    #[tokio::test]
    async fn stale_context_rejects_nested_calls() {
        let bus = Arc::new(EventBus::new());
        let context = base_context(Some(bus));
        let host = Arc::clone(&context.host);
        let tool_context = ExtensionToolContext::new(context, "root").with_execute_tool(Arc::new(
            |id, name, _, _| {
                Box::pin(
                    async move { ToolOutcome::success(json!({"id": id}), json!({"name": name})) },
                )
            },
        ));
        host.invalidate(None);
        let outcome = tool_context
            .execute_tool("read", json!({}), ExecuteToolOptions::default())
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("stale"))
        );
    }

    #[tokio::test]
    async fn ui_prompt_events_are_emitted_around_blocking_calls() {
        let bus = Arc::new(EventBus::new());
        let starts = Arc::new(AtomicUsize::new(0));
        let ends = Arc::new(AtomicUsize::new(0));
        let start_counter = Arc::clone(&starts);
        let end_counter = Arc::clone(&ends);
        bus.on(
            crate::core::event_bus::ExtensionEvent::UiPromptStart,
            move |_| {
                let counter = Arc::clone(&start_counter);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            },
        );
        bus.on(
            crate::core::event_bus::ExtensionEvent::UiPromptEnd,
            move |_| {
                let counter = Arc::clone(&end_counter);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            },
        );
        let mut context = base_context(Some(Arc::clone(&bus)));
        context.ui = ExtensionUi::from_inject(
            ExtensionMode::Tui,
            UiInject {
                select: Arc::new(|_, _| Box::pin(async { Some("choice".to_string()) })),
                ..UiInject::default()
            },
        );
        assert_eq!(
            context
                .select_prompt("Pick", vec!["choice".into()])
                .await
                .as_deref(),
            Some("choice")
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(ends.load(Ordering::SeqCst), 1);
    }
}
