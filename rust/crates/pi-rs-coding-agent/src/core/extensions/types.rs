//! Shared types for the extension host: registrations, the `Extension` trait,
//! the runtime shared state (`ExtensionHost`), and the context value types.
//! Implementations of the context hierarchy live in `super::context`.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::core::event_bus::{EventBus, EventHandler, ExtensionEvent};
pub use crate::core::skills::ResourceDiagnostic;
pub use crate::core::skills::ResourceDiagnostic as Diagnostic;

/// Handler map keyed by event, in registration order within each event.
pub type HandlerMap = IndexMap<ExtensionEvent, Vec<EventHandler>>;

/// Factory for a fresh extension instance. Every reload calls factories again
/// so no state is shared with the previous generation.
pub type ExtensionFactory = Arc<dyn Fn() -> Result<Box<dyn Extension>, String> + Send + Sync>;

/// Declarative tool renderers. Live TUI component factories stay native; WASM
/// extensions use these string-based descriptions at the boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRenderers {
    pub render_shell: Option<String>,
    pub call: Option<String>,
}

pub type ToolRenderer = Arc<dyn Fn(&str) -> Option<ToolRenderers> + Send + Sync>;

/// Injectable UI callbacks. Keeping these as named aliases avoids leaking
/// deeply nested `Arc<dyn Fn>` types through the public context structs.
pub type SelectFn =
    Arc<dyn Fn(String, Vec<String>) -> BoxFuture<'static, Option<String>> + Send + Sync>;
pub type ConfirmFn = Arc<dyn Fn(String, String) -> BoxFuture<'static, bool> + Send + Sync>;
pub type InputFn =
    Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, Option<String>> + Send + Sync>;
pub type NotifyFn = Arc<dyn Fn(String, String) + Send + Sync>;

/// Error listener and extension setup callback aliases.
pub type ErrorListener = Arc<dyn Fn(ExtensionError) + Send + Sync>;
pub type ExtensionSetup =
    Arc<dyn Fn(&mut ExtensionInitContext<'_>) -> Result<(), String> + Send + Sync>;

/// Async factory used by command / UI injection points.
pub type AsyncFn<T> = Arc<dyn Fn() -> BoxFuture<'static, T> + Send + Sync>;

/// How a run was started. `tui` and `rpc` have dialog-capable UI; `json` and
/// `print` have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Rpc => "rpc",
            Self::Json => "json",
            Self::Print => "print",
        }
    }

    pub fn has_ui(self) -> bool {
        matches!(self, Self::Tui | Self::Rpc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Boolean,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    Bool(bool),
    Str(String),
}

impl FlagValue {
    pub fn as_json(&self) -> Value {
        match self {
            Self::Bool(value) => json!(value),
            Self::Str(value) => json!(value),
        }
    }

    pub fn matches_kind(&self, kind: FlagKind) -> bool {
        matches!(
            (self, kind),
            (Self::Bool(_), FlagKind::Boolean) | (Self::Str(_), FlagKind::String)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExposure {
    #[default]
    Direct,
    ModelOnly,
    Codemode,
    Deferred,
    Hidden,
}

impl ToolExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ModelOnly => "model-only",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON Schema object. Must be a JSON object.
    pub parameters: Value,
    pub exposure: ToolExposure,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            label: String::new(),
            description: String::new(),
            parameters,
            exposure: ToolExposure::Direct,
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
        }
    }
}

pub type CommandHandler = Arc<
    dyn Fn(String, ExtensionCommandContext) -> BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

#[derive(Clone)]
pub struct CommandDefinition {
    pub name: String,
    pub description: Option<String>,
    pub handler: CommandHandler,
}

#[derive(Debug, Clone)]
pub struct ShortcutDefinition {
    pub key: String,
    pub description: Option<String>,
    pub extension_path: String,
}

#[derive(Debug, Clone)]
pub struct FlagDefinition {
    pub name: String,
    pub description: Option<String>,
    pub kind: FlagKind,
    pub default: Option<FlagValue>,
    pub extension_path: String,
}

#[derive(Debug, Clone)]
pub struct ProviderRegistration {
    pub name: String,
    pub config: Value,
    pub extension_path: String,
}

#[derive(Debug, Clone)]
pub struct VirtualModelRegistration {
    pub provider: String,
    pub id: String,
    pub definition: Value,
    pub extension_path: String,
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub config: Value,
    pub extension_path: String,
}

/// Compute the namespace of an MCP server name. Upstream `mcpNamespace`
/// collapses `-` and `_`, so `docs_api` and `docs-api` would clash.
pub fn mcp_namespace(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch == '-' || ch == '_' {
            if !result.ends_with('-') {
                result.push('-');
            }
        } else {
            result.extend(ch.to_lowercase());
        }
    }
    result.trim_matches('-').to_string()
}

#[derive(Clone)]
pub struct MarkdownTransformerDefinition {
    pub message_types: Vec<String>,
    pub transform: Arc<dyn Fn(String, Value) -> String + Send + Sync>,
}

/// Everything one extension registered during `init`. Kept separate from the
/// `Extension` trait so a WASM guest and a native built-in share one shape.
#[derive(Default)]
pub struct ExtensionRegistration {
    pub path: String,
    pub replaceable: bool,
    pub hidden: bool,
    pub handlers: HandlerMap,
    pub tools: IndexMap<String, ToolDefinition>,
    pub commands: IndexMap<String, CommandDefinition>,
    pub shortcuts: IndexMap<String, ShortcutDefinition>,
    pub flags: IndexMap<String, FlagDefinition>,
    pub providers: Vec<ProviderRegistration>,
    pub virtual_models: Vec<VirtualModelRegistration>,
    pub mcp_servers: IndexMap<String, McpServerConfig>,
    pub markdown_transformer: Option<MarkdownTransformerDefinition>,
    pub message_renderers: Vec<String>,
    pub entry_renderers: Vec<String>,
    pub tool_renderers: Vec<ToolRenderer>,
}

impl ExtensionRegistration {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    /// Names used by `omitReplacedExtensions`: `tool:<name>`, `command:<name>`,
    /// `flag:<name>` in that order.
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        names.extend(self.tools.keys().map(|name| format!("tool:{name}")));
        names.extend(self.commands.keys().map(|name| format!("command:{name}")));
        names.extend(self.flags.keys().map(|name| format!("flag:{name}")));
        names
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
    pub stack: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPhase {
    Loading,
    Active,
    Failed,
}

/// Shared runtime state. All extension APIs reference one `ExtensionHost`, so
/// flags, MCP servers, provider queues, and invalidation behave identically for
/// native and WASM extensions.
pub struct ExtensionHost {
    pub generation: AtomicU64,
    phase: Mutex<LoadPhase>,
    flag_values: Mutex<IndexMap<String, FlagValue>>,
    pending_flags: Mutex<IndexMap<String, FlagValue>>,
    mcp_servers: Mutex<IndexMap<String, McpServerConfig>>,
    pending_mcp: Mutex<Vec<String>>,
    pending_providers: Mutex<Vec<ProviderRegistration>>,
    pending_virtual_models: Mutex<Vec<VirtualModelRegistration>>,
    applied_providers: Mutex<Vec<ProviderRegistration>>,
    applied_virtual_models: Mutex<Vec<VirtualModelRegistration>>,
    stale_message: Mutex<Option<String>>,
    error_listeners: Mutex<Vec<(usize, ErrorListener)>>,
    bus_unsubscribers: Mutex<Vec<usize>>,
    refresh_tools: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub event_bus: Arc<EventBus>,
}

impl ExtensionHost {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            generation: AtomicU64::new(1),
            phase: Mutex::new(LoadPhase::Loading),
            flag_values: Mutex::new(IndexMap::new()),
            pending_flags: Mutex::new(IndexMap::new()),
            mcp_servers: Mutex::new(IndexMap::new()),
            pending_mcp: Mutex::new(Vec::new()),
            pending_providers: Mutex::new(Vec::new()),
            pending_virtual_models: Mutex::new(Vec::new()),
            applied_providers: Mutex::new(Vec::new()),
            applied_virtual_models: Mutex::new(Vec::new()),
            stale_message: Mutex::new(None),
            error_listeners: Mutex::new(Vec::new()),
            bus_unsubscribers: Mutex::new(Vec::new()),
            refresh_tools: Mutex::new(None),
            event_bus,
        }
    }

    pub fn phase(&self) -> LoadPhase {
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn begin_loading(&self) {
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = LoadPhase::Loading;
        self.pending_flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_mcp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Atomic commit: queued flag defaults become visible and the load phase
    /// becomes `Active`. The event bus transaction is committed by the runner.
    pub fn commit(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending_flags
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut values = self
            .flag_values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (name, value) in pending {
            values.entry(name).or_insert(value);
        }
        drop(values);
        self.pending_mcp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = LoadPhase::Active;
    }

    /// Rollback: discard every queued registration and unsubscribe listeners
    /// staged during the failed load.
    pub fn rollback(&self) {
        self.pending_flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_virtual_models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let names = std::mem::take(
            &mut *self
                .pending_mcp
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut servers = self
            .mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for name in names {
            servers.shift_remove(&name);
        }
        drop(servers);
        let mut unsubscribers = self
            .bus_unsubscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in unsubscribers.drain(..) {
            self.event_bus.off(id);
        }
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = LoadPhase::Failed;
    }

    pub fn register_flag_default(&self, name: &str, value: FlagValue) -> Result<(), String> {
        if self.phase() == LoadPhase::Loading {
            let mut pending = self
                .pending_flags
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.entry(name.to_string()).or_insert(value);
        } else {
            let mut values = self
                .flag_values
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            values.entry(name.to_string()).or_insert(value);
        }
        Ok(())
    }

    pub fn get_flag(&self, name: &str) -> Option<FlagValue> {
        self.flag_values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
            .or_else(|| {
                self.pending_flags
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(name)
                    .cloned()
            })
    }

    pub fn set_flag_value(&self, name: &str, value: FlagValue) {
        self.flag_values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_string(), value);
    }

    pub fn flag_values(&self) -> IndexMap<String, FlagValue> {
        self.flag_values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn restore_flag_values(&self, values: &IndexMap<String, FlagValue>) {
        *self
            .flag_values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = values.clone();
    }

    pub fn register_mcp_server(&self, server: McpServerConfig) -> Result<(), String> {
        let mut servers = self
            .mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(owner) = servers
            .get(&server.name)
            .map(|existing| existing.extension_path.clone())
            && owner != server.extension_path
        {
            return Err(format!(
                "MCP server \"{}\" is already registered by extension \"{owner}\"",
                server.name
            ));
        }
        let namespace = mcp_namespace(&server.name);
        if let Some(clash) = servers
            .iter()
            .find(|(name, _existing)| **name != server.name && mcp_namespace(name) == namespace)
            .map(|(_, existing)| existing.name.clone())
        {
            return Err(format!(
                "MCP server \"{}\" conflicts with registered server \"{clash}\"",
                server.name
            ));
        }
        if self.phase() == LoadPhase::Loading {
            self.pending_mcp
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(server.name.clone());
        }
        servers.insert(server.name.clone(), server);
        Ok(())
    }

    pub fn unregister_mcp_server(&self, name: &str, owner: &str) {
        let mut servers = self
            .mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if servers
            .get(name)
            .is_some_and(|server| server.extension_path == owner)
        {
            servers.shift_remove(name);
        }
    }

    pub fn mcp_servers(&self) -> Vec<McpServerConfig> {
        self.mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn register_provider(&self, registration: ProviderRegistration) {
        if self.phase() == LoadPhase::Loading {
            self.pending_providers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(registration);
        } else {
            self.applied_providers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(registration);
        }
    }

    pub fn unregister_provider(&self, name: &str) {
        for collection in [&self.pending_providers, &self.applied_providers] {
            collection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|registration| registration.name != name);
        }
    }

    pub fn register_virtual_model(&self, registration: VirtualModelRegistration) {
        if self.phase() == LoadPhase::Loading {
            self.pending_virtual_models
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(registration);
        } else {
            self.applied_virtual_models
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(registration);
        }
    }

    pub fn unregister_virtual_model(&self, provider: &str, id: &str) {
        for collection in [&self.pending_virtual_models, &self.applied_virtual_models] {
            collection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|registration| registration.provider != provider || registration.id != id);
        }
    }

    /// Take the providers queued during load. `bind_core` flushes these once
    /// the model registry is available.
    pub fn take_pending_providers(&self) -> Vec<ProviderRegistration> {
        std::mem::take(
            &mut *self
                .pending_providers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn take_pending_virtual_models(&self) -> Vec<VirtualModelRegistration> {
        std::mem::take(
            &mut *self
                .pending_virtual_models
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn applied_providers(&self) -> Vec<ProviderRegistration> {
        self.applied_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn applied_virtual_models(&self) -> Vec<VirtualModelRegistration> {
        self.applied_virtual_models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set_refresh_tools(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self
            .refresh_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(callback);
    }

    pub fn refresh_tools(&self) {
        if let Some(callback) = self
            .refresh_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            callback();
        }
    }

    pub fn track_unsubscribe(&self, id: usize) {
        self.bus_unsubscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(id);
    }

    pub fn on_error(&self, listener: ErrorListener) -> usize {
        let mut listeners = self
            .error_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = listeners.len() + 1;
        listeners.push((id, listener));
        id
    }

    pub fn report_error(&self, error: ExtensionError) {
        let listeners = self
            .error_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for (_, listener) in listeners {
            listener(error.clone());
        }
    }

    /// Mark this runtime stale. All captured contexts and WASM instances must
    /// fail subsequent calls with the returned message.
    pub fn invalidate(&self, message: Option<&str>) {
        let mut stale = self
            .stale_message
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stale.is_some() {
            return;
        }
        *stale = Some(message.unwrap_or(STALE_MESSAGE).to_string());
        drop(stale);
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut unsubscribers = self
            .bus_unsubscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in unsubscribers.drain(..) {
            self.event_bus.off(id);
        }
    }

    pub fn stale_message(&self) -> Option<String> {
        self.stale_message
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn assert_active(&self) -> Result<(), String> {
        self.stale_message().map_or(Ok(()), Err)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn clear_pending(&self) {
        self.pending_flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_mcp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.pending_virtual_models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

pub const STALE_MESSAGE: &str = "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For reload, do not use the old ctx after await ctx.reload().";

/// Registration API exposed to `Extension::init`.
pub struct ExtensionInitContext<'a> {
    pub registration: &'a mut ExtensionRegistration,
    pub host: &'a Arc<ExtensionHost>,
    pub bus: &'a Arc<EventBus>,
    pub cwd: &'a str,
}

impl ExtensionInitContext<'_> {
    pub fn assert_active(&self) -> Result<(), String> {
        self.host.assert_active()
    }

    /// Subscribe an extension handler. The handler is recorded both on the
    /// registration (for snapshots) and on the event bus (for dispatch).
    pub fn on(&mut self, event: ExtensionEvent, handler: EventHandler) -> Result<usize, String> {
        self.assert_active()?;
        let id = self.bus.on_handler(event, Arc::clone(&handler));
        self.host.track_unsubscribe(id);
        self.registration
            .handlers
            .entry(event)
            .or_default()
            .push(handler);
        Ok(id)
    }

    pub fn register_tool(&mut self, definition: ToolDefinition) -> Result<(), String> {
        self.assert_active()?;
        if !definition.parameters.is_object() {
            return Err(format!(
                "Tool \"{}\" registered by extension \"{}\" must define an object parameter schema.",
                definition.name, self.registration.path
            ));
        }
        self.registration
            .tools
            .insert(definition.name.clone(), definition);
        self.host.refresh_tools();
        Ok(())
    }

    pub fn register_command(&mut self, definition: CommandDefinition) -> Result<(), String> {
        self.assert_active()?;
        if definition.name.is_empty() {
            return Err(format!(
                "Command registered by extension \"{}\" must have a non-empty string name.",
                self.registration.path
            ));
        }
        self.registration
            .commands
            .insert(definition.name.clone(), definition);
        Ok(())
    }

    pub fn register_shortcut(&mut self, definition: ShortcutDefinition) -> Result<(), String> {
        self.assert_active()?;
        self.registration
            .shortcuts
            .insert(definition.key.to_lowercase(), definition);
        Ok(())
    }

    pub fn register_flag(&mut self, definition: FlagDefinition) -> Result<(), String> {
        self.assert_active()?;
        if let Some(default) = &definition.default
            && !default.matches_kind(definition.kind)
        {
            return Err(format!(
                "Invalid default for flag \"{}\": expected {:?}, got {:?}",
                definition.name, definition.kind, default
            ));
        }
        if let Some(default) = &definition.default {
            self.host
                .register_flag_default(&definition.name, default.clone())?;
        }
        self.registration
            .flags
            .insert(definition.name.clone(), definition);
        Ok(())
    }

    pub fn get_flag(&self, name: &str) -> Option<FlagValue> {
        if self.registration.flags.contains_key(name) {
            self.host.get_flag(name)
        } else {
            None
        }
    }

    pub fn register_provider(&mut self, registration: ProviderRegistration) -> Result<(), String> {
        self.assert_active()?;
        self.registration.providers.push(registration.clone());
        self.host.register_provider(registration);
        Ok(())
    }

    pub fn register_virtual_model(
        &mut self,
        registration: VirtualModelRegistration,
    ) -> Result<(), String> {
        self.assert_active()?;
        self.registration.virtual_models.push(registration.clone());
        self.host.register_virtual_model(registration);
        Ok(())
    }

    pub fn register_mcp_server(&mut self, server: McpServerConfig) -> Result<(), String> {
        self.assert_active()?;
        self.host.register_mcp_server(server.clone())?;
        self.registration
            .mcp_servers
            .insert(server.name.clone(), server);
        Ok(())
    }

    pub fn register_markdown_transformer(
        &mut self,
        transformer: MarkdownTransformerDefinition,
    ) -> Result<(), String> {
        self.assert_active()?;
        self.registration.markdown_transformer = Some(transformer);
        Ok(())
    }

    pub fn register_tool_renderer(&mut self, renderer: ToolRenderer) -> Result<(), String> {
        self.assert_active()?;
        self.registration.tool_renderers.push(renderer);
        Ok(())
    }
}

/// Agent context usage snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsage {
    pub tokens: Option<i64>,
    pub context_window: i64,
    pub percent: Option<f64>,
}

/// Options for a nested tool call through `ExtensionToolContext::execute_tool`.
#[derive(Debug, Clone, Default)]
pub struct ExecuteToolOptions {
    pub deliver_as_update: bool,
}

/// Outcome of a tool call. Tool failures are values, not errors, matching the
/// upstream contract: nested `execute_tool` never rejects.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub tool_call: Value,
    pub result: Value,
    pub is_error: bool,
    pub nested_calls: Vec<Value>,
}

impl ToolOutcome {
    pub fn error(tool_call_id: &str, name: &str, message: &str) -> Self {
        Self {
            tool_call: json!({ "type": "toolCall", "id": tool_call_id, "name": name, "arguments": {} }),
            result: json!({
                "content": [{ "type": "text", "text": message }],
                "details": {}
            }),
            is_error: true,
            nested_calls: Vec::new(),
        }
    }

    pub fn success(tool_call: Value, result: Value) -> Self {
        Self {
            tool_call,
            result,
            is_error: false,
            nested_calls: Vec::new(),
        }
    }
}

/// Base context passed to event handlers and tool execution.
pub struct ExtensionContext {
    pub host: Arc<ExtensionHost>,
    pub cwd: String,
    pub mode: ExtensionMode,
    pub ui: ExtensionUi,
    pub bus: Option<Arc<EventBus>>,
    pub is_idle: Arc<dyn Fn() -> bool + Send + Sync>,
    pub is_project_trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    pub has_pending_messages: Arc<dyn Fn() -> bool + Send + Sync>,
    pub get_context_usage: Arc<dyn Fn() -> Option<ContextUsage> + Send + Sync>,
    pub get_system_prompt: Arc<dyn Fn() -> String + Send + Sync>,
    pub abort: Arc<dyn Fn() + Send + Sync>,
    pub shutdown: Arc<dyn Fn() + Send + Sync>,
    pub compact: Arc<dyn Fn(Option<Value>) + Send + Sync>,
}

/// Injectable UI primitives. All functions are host-provided so tests are
/// fully offline: no terminal, no sockets, no dialogs.
pub struct ExtensionUi {
    pub has_ui: bool,
    pub mode: ExtensionMode,
    pub select: SelectFn,
    pub confirm: ConfirmFn,
    pub input: InputFn,
    pub notify: NotifyFn,
}

pub type ExecuteToolFn = Arc<
    dyn Fn(String, String, Value, ExecuteToolOptions) -> BoxFuture<'static, ToolOutcome>
        + Send
        + Sync,
>;

/// Context passed to tool `execute()`. Adds `tools` and nested
/// `execute_tool()` with ids of the form `<parentId>/<n>`.
pub struct ExtensionToolContext {
    pub base: ExtensionContext,
    pub tool_call_id: String,
    pub tools: Vec<ToolDefinition>,
    nested_counter: Arc<std::sync::atomic::AtomicUsize>,
    pub execute_tool_fn: Option<ExecuteToolFn>,
}

impl ExtensionToolContext {
    pub fn new(base: ExtensionContext, tool_call_id: impl Into<String>) -> Self {
        Self {
            base,
            tool_call_id: tool_call_id.into(),
            tools: Vec::new(),
            nested_counter: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            execute_tool_fn: None,
        }
    }

    /// Never rejects: unknown tools, blocked calls, and thrown errors all come
    /// back as `is_error: true`.
    pub async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        options: ExecuteToolOptions,
    ) -> ToolOutcome {
        let Some(execute) = &self.execute_tool_fn else {
            return ToolOutcome::error(
                &format!("{}/0", self.tool_call_id),
                name,
                "Nested tool calls are not available in this context",
            );
        };
        let index = self.nested_counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/{}", self.tool_call_id, index);
        let nested_calls = self.base.host.assert_active().err();
        if let Some(message) = nested_calls {
            return ToolOutcome::error(&id, name, &message);
        }
        execute(id, name.to_string(), args, options).await
    }
}

pub type NewSessionFn =
    Arc<dyn Fn(Option<Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
pub type SimpleSessionFn =
    Arc<dyn Fn(String, Option<Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
pub type ReloadFn = Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Context passed to command handlers. Adds session-control operations that
/// are only safe for user-initiated commands.
pub struct ExtensionCommandContext {
    pub base: ExtensionContext,
    pub wait_for_idle: AsyncFn<()>,
    pub new_session: NewSessionFn,
    pub fork: SimpleSessionFn,
    pub navigate_tree: SimpleSessionFn,
    pub switch_session: SimpleSessionFn,
    pub reload: ReloadFn,
}

/// The unified extension trait implemented by native built-ins and by the
/// WASM adapter.
#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn path(&self) -> &str {
        self.name()
    }
    fn replaceable(&self) -> bool {
        false
    }
    fn hidden(&self) -> bool {
        false
    }
    fn registration(&self) -> &ExtensionRegistration;

    /// Attach the registration assembled from `init`. The runner owns the
    /// transaction and hands the finished registration back on success.
    fn set_registration(&mut self, registration: ExtensionRegistration);

    fn handlers(&self) -> &HandlerMap {
        &self.registration().handlers
    }

    async fn init(&mut self, _ctx: &mut ExtensionInitContext<'_>) -> Result<(), String> {
        Ok(())
    }

    async fn execute_tool(
        &self,
        name: &str,
        _args: Value,
        _ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        Err(format!(
            "Extension \"{}\" does not provide tool \"{name}\"",
            self.name()
        ))
    }

    async fn run_command(
        &self,
        name: &str,
        _args: &str,
        _ctx: &ExtensionCommandContext,
    ) -> Result<(), String> {
        Err(format!(
            "Extension \"{}\" does not provide command \"{name}\"",
            self.name()
        ))
    }

    /// Tear down the instance. Native extensions are no-ops; the WASM adapter
    /// drops its `Store` and bumps the generation token.
    fn invalidate(&self) {}
}

/// Result of loading a set of extensions.
#[derive(Default)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<Arc<dyn Extension>>,
    pub errors: Vec<ExtensionError>,
    pub warnings: Vec<ResourceDiagnostic>,
}

impl Clone for LoadExtensionsResult {
    fn clone(&self) -> Self {
        Self {
            extensions: self.extensions.clone(),
            errors: self.errors.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

/// A minimal native extension used by built-ins and tests. Registration is
/// filled in by the runner during the pre-load transaction.
pub struct SimpleExtension {
    name: String,
    registration: ExtensionRegistration,
}

impl SimpleExtension {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            registration: ExtensionRegistration::new(name.clone()),
            name,
        }
    }
}

#[async_trait]
impl Extension for SimpleExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn registration(&self) -> &ExtensionRegistration {
        &self.registration
    }

    fn set_registration(&mut self, registration: ExtensionRegistration) {
        self.registration = registration;
    }
}

/// A helper for building a handler closure that returns a fixed JSON result.
pub fn constant_handler(result: Value) -> EventHandler {
    Arc::new(move |_| {
        let result = result.clone();
        Box::pin(async move { Ok(Some(result)) })
    })
}

/// A helper for building a handler from a synchronous function.
pub fn sync_handler<F>(function: F) -> EventHandler
where
    F: Fn(Value) -> HandlerResultValue + Send + Sync + 'static,
{
    Arc::new(move |data| {
        let output = function(data);
        Box::pin(async move { output })
    })
}

pub type HandlerResultValue = Result<Option<Value>, String>;

/// Await a boxed future produced by one of the injectable UI closures.
pub async fn run_async<T, F>(function: F) -> T
where
    F: Future<Output = T>,
{
    function.await
}
