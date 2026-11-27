//! WASM extension adapter backed by `wasmtime::component::bindgen!` (`wit/extension.wit`).
//!
//! In-place JS mutations become explicit `option<patch>` return records across
//! the WIT boundary, every instance carries a generation token, and
//! invalidation drops the `wasmtime::Store` so guest memory and host resources
//! are reclaimed cleanly on `/reload`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use wasmtime::component::{Component, HasSelf, Linker, TypedFunc};
use wasmtime::{Engine, Store};

use crate::core::event_bus::ExtensionEvent;
use crate::core::tools::bash::{EXIT_STDIO_GRACE_MS, MAX_TIMEOUT_MS, NON_INTERACTIVE_ENV};

use super::types::{
    Extension, ExtensionInitContext, ExtensionRegistration, ExtensionToolContext, STALE_MESSAGE,
    ToolOutcome,
};

const EXEC_DEFAULT_TIMEOUT_MS: u64 = 30_000;
const EXEC_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub mod wit_bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "extension",
        imports: { default: async },
        exports: { default: async },
    });
}

/// Callback invoked when a WASM guest calls `pi:extension/host.execute-tool`.
pub type HostToolHandler = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// A WIT `option<patch>`: `None` means the guest did not touch the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionPatch<T> {
    pub value: Option<T>,
}

impl<T> Default for OptionPatch<T> {
    fn default() -> Self {
        Self::none()
    }
}

impl<T> OptionPatch<T> {
    pub fn none() -> Self {
        Self { value: None }
    }

    pub fn set(value: T) -> Self {
        Self { value: Some(value) }
    }

    pub fn changed(&self) -> bool {
        self.value.is_some()
    }
}

/// `before_provider_headers` patch. `null` in the set map means delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadersPatch {
    pub set: IndexMap<String, String>,
    pub delete: Vec<String>,
}

impl HeadersPatch {
    pub fn apply(&self, headers: &mut Value) {
        let Some(object) = headers.as_object_mut() else {
            return;
        };
        for name in &self.delete {
            object.shift_remove(name);
        }
        for (name, value) in &self.set {
            object.insert(name.clone(), json!(value));
        }
    }
}

/// `tool_call` patch. `input` is an explicit replacement because WASM receives
/// a value copy, not a mutable JS object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallPatch {
    pub block: OptionPatch<bool>,
    pub reason: OptionPatch<String>,
    pub input: OptionPatch<Value>,
    pub terminate: OptionPatch<bool>,
}

impl ToolCallPatch {
    pub fn apply(&self, event: &mut Value) {
        if let Some(block) = self.block.value {
            event["block"] = json!(block);
        }
        if let Some(reason) = &self.reason.value {
            event["reason"] = json!(reason);
        }
        if let Some(input) = &self.input.value {
            event["input"] = input.clone();
        }
        if let Some(terminate) = self.terminate.value {
            event["terminate"] = json!(terminate);
        }
    }
}

/// `tool_result` patch. Replacing `content` without `structuredContent` drops
/// the structured payload, matching the TypeScript host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResultPatch {
    pub content: OptionPatch<Value>,
    pub structured_content: OptionPatch<Value>,
    pub details: OptionPatch<Value>,
    pub is_error: OptionPatch<bool>,
    pub usage: OptionPatch<Value>,
}

impl ToolResultPatch {
    pub fn apply(&self, event: &mut Value) -> bool {
        let mut modified = false;
        if let Some(content) = &self.content.value {
            event["content"] = content.clone();
            if let Some(structured) = &self.structured_content.value {
                event["structuredContent"] = structured.clone();
            } else if let Some(object) = event.as_object_mut() {
                object.remove("structuredContent");
            }
            modified = true;
        }
        if let Some(structured) = &self.structured_content.value {
            event["structuredContent"] = structured.clone();
            modified = true;
        }
        if let Some(details) = &self.details.value {
            event["details"] = details.clone();
            modified = true;
        }
        if let Some(is_error) = self.is_error.value {
            event["isError"] = json!(is_error);
            modified = true;
        }
        if let Some(usage) = &self.usage.value {
            event["usage"] = usage.clone();
            modified = true;
        }
        modified
    }
}

/// `message_end` patch. A replacement with a different role is rejected.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageEndPatch {
    pub message: OptionPatch<Value>,
    pub system_prompt_options: OptionPatch<Value>,
}

impl MessageEndPatch {
    pub fn apply(&self, event: &mut Value) -> Result<bool, String> {
        let mut modified = false;
        if let Some(message) = &self.message.value {
            if event.get("role") != message.get("role") {
                return Err(
                    "message_end handlers must return a message with the same role".to_string(),
                );
            }
            event["message"] = message.clone();
            modified = true;
        }
        if let Some(options) = &self.system_prompt_options.value {
            event["systemPromptOptions"] = options.clone();
            modified = true;
        }
        Ok(modified)
    }
}

/// `cache_warming_decision` patch: last non-null action wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheWarmingPatch {
    pub action: OptionPatch<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationToken(u64);

impl GenerationToken {
    pub fn initial() -> Self {
        Self(1)
    }

    pub fn bump(&mut self) {
        self.0 += 1;
    }

    pub fn value(self) -> u64 {
        self.0
    }

    pub fn is_current(self, generation: u64) -> bool {
        self.0 == generation
    }
}

#[derive(Clone, Default)]
struct HostBridge {
    notifications: Arc<Mutex<Vec<(String, String)>>>,
    flags: Arc<Mutex<IndexMap<String, String>>>,
    tool_handler: Arc<Mutex<Option<HostToolHandler>>>,
}

#[derive(Default)]
struct GuestRegistrations {
    providers: Vec<(String, Value)>,
    virtual_models: Vec<(String, String, Value)>,
    mcp_servers: Vec<(String, Value)>,
    tools: Vec<super::types::ToolDefinition>,
    commands: Vec<(String, Option<String>)>,
    shortcuts: Vec<(String, Option<String>, String)>,
    flags: Vec<super::types::FlagDefinition>,
    subscriptions: Vec<ExtensionEvent>,
}

struct WasmState {
    host: Option<std::sync::Weak<super::types::ExtensionHost>>,
    registrations: Option<GuestRegistrations>,
    ui: Option<super::types::ExtensionUi>,
    cwd: String,
    generation: u64,
    stale: bool,
    bridge: HostBridge,
}

struct ChildTree(Option<u32>);

impl ChildTree {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take().and_then(|pid| i32::try_from(pid).ok()) {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(pid) = self.0.take() {
            let _ = crate::utils::child_process::kill_process_tree(pid);
        }
    }
}

impl Drop for ChildTree {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn read_capped_pipe<R: AsyncRead + Unpin>(pipe: Option<R>, limit: usize) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if bytes.len() < limit {
                    let take = (limit - bytes.len()).min(n);
                    bytes.extend_from_slice(&buf[..take]);
                }
            }
        }
    }
    bytes
}

async fn exec_host_command(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    default_cwd: &str,
    timeout_ms: Option<u64>,
) -> Result<wit_bindings::pi::extension::types::ExecResult, String> {
    if command.trim().is_empty() {
        return Err("exec command must not be empty".to_string());
    }
    let effective_timeout = timeout_ms.unwrap_or(EXEC_DEFAULT_TIMEOUT_MS);
    if effective_timeout == 0 {
        return Err("exec timeout_ms must be greater than 0".to_string());
    }
    if effective_timeout > MAX_TIMEOUT_MS {
        return Err(format!(
            "exec timeout_ms exceeds maximum ({MAX_TIMEOUT_MS})"
        ));
    }

    let work_dir = match cwd.filter(|value| !value.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None if !default_cwd.is_empty() => Some(PathBuf::from(default_cwd)),
        None => None,
    };
    if let Some(dir) = &work_dir
        && !dir.exists()
    {
        return Err(format!(
            "Working directory does not exist: {}",
            dir.display()
        ));
    }

    let mut process = Command::new(command);
    process
        .args(args)
        .envs(NON_INTERACTIVE_ENV.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = &work_dir {
        process.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.as_std_mut().process_group(0);
    }

    let mut child = process
        .spawn()
        .map_err(|error| format!("Failed to spawn \"{command}\": {error}"))?;
    let mut children = ChildTree(child.id());
    let mut stdout_task =
        tokio::spawn(read_capped_pipe(child.stdout.take(), EXEC_MAX_OUTPUT_BYTES));
    let mut stderr_task =
        tokio::spawn(read_capped_pipe(child.stderr.take(), EXEC_MAX_OUTPUT_BYTES));

    let mut timed_out = false;
    let status = match timeout(Duration::from_millis(effective_timeout), child.wait()).await {
        Ok(result) => {
            children.0 = None;
            Some(result.map_err(|error| format!("Failed to wait for \"{command}\": {error}"))?)
        }
        Err(_) => {
            timed_out = true;
            children.kill();
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };

    let drain = Duration::from_millis(EXIT_STDIO_GRACE_MS);
    let stdout_bytes = timeout(drain, &mut stdout_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stderr_bytes = timeout(drain, &mut stderr_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();

    Ok(wit_bindings::pi::extension::types::ExecResult {
        exit_code: status.and_then(|s| s.code()).unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
    })
}

impl wit_bindings::pi::extension::types::Host for WasmState {}

impl wit_bindings::pi::extension::host::Host for WasmState {
    async fn register_provider(&mut self, name: String, config_json: String) -> Result<(), String> {
        let config = serde_json::from_str(&config_json).map_err(|e| e.to_string())?;
        self.registrations
            .as_mut()
            .ok_or("Providers must be registered during init")?
            .providers
            .push((name, config));
        Ok(())
    }
    async fn register_virtual_model(
        &mut self,
        provider: String,
        id: String,
        definition_json: String,
    ) -> Result<(), String> {
        let definition = serde_json::from_str(&definition_json).map_err(|e| e.to_string())?;
        self.registrations
            .as_mut()
            .ok_or("Virtual models must be registered during init")?
            .virtual_models
            .push((provider, id, definition));
        Ok(())
    }
    async fn register_mcp_server(
        &mut self,
        name: String,
        config_json: String,
    ) -> Result<(), String> {
        let config = serde_json::from_str(&config_json).map_err(|e| e.to_string())?;
        self.registrations
            .as_mut()
            .ok_or("MCP servers must be registered during init")?
            .mcp_servers
            .push((name, config));
        Ok(())
    }
    async fn register_tool(
        &mut self,
        name: String,
        description: String,
        schema_json: String,
    ) -> Result<(), String> {
        let schema: Value = serde_json::from_str(&schema_json).map_err(|e| e.to_string())?;
        if name.is_empty() || !schema.is_object() {
            return Err("Expected a tool name and an object schema".into());
        }
        let mut tool = super::types::ToolDefinition::new(name, schema);
        tool.description = description;
        self.registrations
            .as_mut()
            .ok_or("Tools must be registered during init")?
            .tools
            .push(tool);
        Ok(())
    }
    async fn register_command(
        &mut self,
        name: String,
        description: Option<String>,
    ) -> Result<(), String> {
        self.registrations
            .as_mut()
            .ok_or("Commands must be registered during init")?
            .commands
            .push((name, description));
        Ok(())
    }
    async fn register_shortcut(
        &mut self,
        key: String,
        description: Option<String>,
        command: String,
    ) -> Result<(), String> {
        if key.trim().is_empty() || command.trim().is_empty() {
            return Err("Shortcut key and command name must be non-empty".to_string());
        }
        self.registrations
            .as_mut()
            .ok_or("Shortcuts must be registered during init")?
            .shortcuts
            .push((key, description, command));
        Ok(())
    }
    async fn register_flag(
        &mut self,
        name: String,
        description: Option<String>,
        boolean: bool,
        default_value: Option<String>,
    ) -> Result<(), String> {
        use super::types::{FlagDefinition, FlagKind, FlagValue};
        let (kind, default) = if boolean {
            (
                FlagKind::Boolean,
                default_value
                    .map(|s| s.parse::<bool>().map(FlagValue::Bool))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            )
        } else {
            (FlagKind::String, default_value.map(FlagValue::Str))
        };
        self.registrations
            .as_mut()
            .ok_or("Flags must be registered during init")?
            .flags
            .push(FlagDefinition {
                name,
                description,
                kind,
                default,
                extension_path: String::new(),
            });
        Ok(())
    }

    async fn subscribe(&mut self, event: String) -> Result<(), String> {
        let parsed = ExtensionEvent::parse(&event)?;
        let subscriptions = &mut self
            .registrations
            .as_mut()
            .ok_or("Events must be subscribed during init")?
            .subscriptions;
        if !subscriptions.contains(&parsed) {
            subscriptions.push(parsed);
        }
        Ok(())
    }

    async fn ui_select(&mut self, title: String, items: Vec<String>) -> Option<String> {
        self.ui
            .as_ref()?
            .select_with_options(title, items, Default::default())
            .await
    }

    async fn ui_confirm(&mut self, title: String, message: String) -> bool {
        match &self.ui {
            Some(ui) => {
                ui.confirm_with_options(title, message, Default::default())
                    .await
            }
            None => false,
        }
    }

    async fn ui_input(&mut self, title: String, placeholder: Option<String>) -> Option<String> {
        self.ui
            .as_ref()?
            .input_with_options(title, placeholder, Default::default())
            .await
    }

    async fn ui_editor(&mut self, title: String, prefill: Option<String>) -> Option<String> {
        self.ui.as_ref()?.editor(title, prefill).await
    }

    async fn ui_notify(&mut self, message: String, kind: Option<String>) {
        if let Some(ui) = &self.ui {
            ui.notify_with_type(message, kind.as_deref());
        }
    }

    async fn ui_set_status(&mut self, key: String, text: Option<String>) {
        if let Some(ui) = &self.ui {
            ui.set_status(key, text);
        }
    }

    async fn ui_set_widget(&mut self, key: String, lines: Option<Vec<String>>) {
        if let Some(ui) = &self.ui {
            ui.set_widget(key, lines, None);
        }
    }

    async fn ui_set_title(&mut self, title: String) {
        if let Some(ui) = &self.ui {
            ui.set_title(title);
        }
    }

    async fn ui_set_editor_text(&mut self, text: String) {
        if let Some(ui) = &self.ui {
            ui.set_editor_text(text);
        }
    }

    async fn notify(&mut self, level: String, message: String) {
        if self.stale {
            return;
        }
        self.bridge
            .notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((level, message));
    }

    async fn get_flag(&mut self, name: String) -> Option<String> {
        if self.stale {
            return None;
        }
        if let Some(host) = self.host.as_ref().and_then(std::sync::Weak::upgrade)
            && let Some(value) = host.get_flag(&name)
        {
            return Some(match value {
                super::types::FlagValue::Bool(value) => value.to_string(),
                super::types::FlagValue::Str(value) => value,
            });
        }
        self.bridge
            .flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&name)
            .cloned()
    }

    async fn get_cwd(&mut self) -> String {
        if self.stale {
            return String::new();
        }
        self.cwd.clone()
    }

    async fn get_model(&mut self) -> Option<String> {
        if self.stale {
            return None;
        }
        self.host
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .and_then(|host| host.get_model())
    }

    async fn get_active_tools(&mut self) -> Vec<String> {
        if self.stale {
            return Vec::new();
        }
        self.host
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .map(|host| host.get_active_tools())
            .unwrap_or_default()
    }

    async fn set_active_tools(&mut self, tools: Vec<String>) -> Result<(), String> {
        if self.stale {
            return Err(STALE_MESSAGE.to_string());
        }
        let host = self
            .host
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| STALE_MESSAGE.to_string())?;
        host.assert_active()?;
        host.set_active_tools(tools)
    }

    async fn send_message(
        &mut self,
        custom_type: String,
        content_json: String,
        trigger_turn: bool,
    ) -> Result<(), String> {
        if self.stale {
            return Err(STALE_MESSAGE.to_string());
        }
        let content: Value = serde_json::from_str(&content_json)
            .map_err(|error| format!("invalid send-message content JSON: {error}"))?;
        let host = self
            .host
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| STALE_MESSAGE.to_string())?;
        host.assert_active()?;
        host.send_message(custom_type, content, trigger_turn)
    }

    async fn append_entry(
        &mut self,
        custom_type: String,
        data_json: Option<String>,
    ) -> Result<String, String> {
        if self.stale {
            return Err(STALE_MESSAGE.to_string());
        }
        let data = data_json
            .map(|raw| {
                serde_json::from_str::<Value>(&raw)
                    .map_err(|error| format!("invalid append-entry data JSON: {error}"))
            })
            .transpose()?;
        let host = self
            .host
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| STALE_MESSAGE.to_string())?;
        host.assert_active()?;
        host.append_entry(custom_type, data)
    }

    async fn exec(
        &mut self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<wit_bindings::pi::extension::types::ExecResult, String> {
        if self.stale {
            return Err(STALE_MESSAGE.to_string());
        }
        if let Some(host) = self.host.as_ref().and_then(std::sync::Weak::upgrade) {
            host.assert_active()?;
        }
        exec_host_command(&command, &args, cwd.as_deref(), &self.cwd, timeout_ms).await
    }

    async fn execute_tool(&mut self, name: String, args_json: String) -> Result<String, String> {
        if self.stale {
            return Err(STALE_MESSAGE.to_string());
        }
        let handler = self
            .bridge
            .tool_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match handler {
            Some(handler) => handler(&name, &args_json),
            None => Err(format!("no host tool dispatcher configured for \"{name}\"")),
        }
    }
}

type OnEventFunc = TypedFunc<(String, String), (Result<Option<String>, String>,)>;

struct InstantiatedGuest {
    store: Store<WasmState>,
    instance: wit_bindings::Extension,
    on_event: Option<OnEventFunc>,
}

async fn run_guest_command(
    guest: &tokio::sync::Mutex<InstantiatedGuest>,
    name: &str,
    args: &str,
    ctx: &super::types::ExtensionCommandContext,
) -> Result<(), String> {
    let mut guard = guest.lock().await;
    let InstantiatedGuest {
        store, instance, ..
    } = &mut *guard;
    store.data_mut().ui = Some(ctx.base.ui.clone());
    store.data_mut().cwd.clone_from(&ctx.base.cwd);
    let result = instance.call_run_command(&mut *store, name, args).await;
    store.data_mut().ui = None;
    result.map_err(|e| format!("guest run-command trap: {e}"))?
}

async fn call_guest_event(
    guest: &tokio::sync::Mutex<InstantiatedGuest>,
    event_name: &str,
    payload: &Value,
) -> Result<Option<Value>, String> {
    let payload_json = serde_json::to_string(payload).map_err(|error| error.to_string())?;
    let mut guard = guest.lock().await;
    let InstantiatedGuest {
        store, on_event, ..
    } = &mut *guard;
    let Some(func) = on_event.as_ref() else {
        return Ok(None);
    };
    let (raw,) = func
        .call_async(&mut *store, (event_name.to_string(), payload_json))
        .await
        .map_err(|error| format!("guest on-event trap: {error}"))?;
    match raw? {
        Some(text) => serde_json::from_str::<Value>(&text)
            .map(Some)
            .map_err(|error| format!("invalid guest on-event JSON: {error}")),
        None => Ok(None),
    }
}

/// Owner of a single guest `Store`. Dropping the store on invalidation
/// reclaims guest memory and any host resources the guest created.
pub struct WasmStore {
    engine: Engine,
    component: Option<Component>,
    guest: Mutex<Option<Arc<tokio::sync::Mutex<InstantiatedGuest>>>>,
    bare_store: Mutex<Option<Store<WasmState>>>,
    bridge: HostBridge,
    generation: GenerationToken,
    instantiated: bool,
    invalidated: bool,
}

impl WasmStore {
    pub fn new() -> Result<Self, String> {
        let engine = Engine::default();
        Ok(Self {
            engine,
            component: None,
            guest: Mutex::new(None),
            bare_store: Mutex::new(None),
            bridge: HostBridge::default(),
            generation: GenerationToken::initial(),
            instantiated: false,
            invalidated: false,
        })
    }

    /// Validate and retain a component. The component is instantiated into a
    /// live async `wasmtime::Store` when `instantiate_async` or an async export
    /// runs.
    pub fn instantiate(&mut self, bytes: &[u8]) -> Result<(), String> {
        let component = Component::new(&self.engine, bytes)
            .map_err(|error| format!("invalid component: {error}"))?;
        let generation = self.generation.value();
        *self
            .bare_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Store::new(
            &self.engine,
            WasmState {
                host: None,
                ui: None,
                registrations: None,
                cwd: String::new(),
                generation,
                stale: false,
                bridge: self.bridge.clone(),
            },
        ));
        *self
            .guest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.component = Some(component);
        self.instantiated = true;
        Ok(())
    }

    fn prepare_instantiation(
        &self,
    ) -> Result<Option<(Engine, Component, u64, HostBridge)>, String> {
        self.assert_current()?;
        if self
            .guest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
        {
            return Ok(None);
        }
        let Some(component) = self.component.clone() else {
            return Ok(None);
        };
        Ok(Some((
            self.engine.clone(),
            component,
            self.generation.value(),
            self.bridge.clone(),
        )))
    }

    fn install_guest(
        &self,
        guest: Arc<tokio::sync::Mutex<InstantiatedGuest>>,
        expected_generation: u64,
    ) -> Result<Arc<tokio::sync::Mutex<InstantiatedGuest>>, String> {
        self.assert_current()?;
        if self.generation.value() != expected_generation {
            return Err(STALE_MESSAGE.to_string());
        }
        *self
            .bare_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *self
            .guest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guest.clone());
        Ok(guest)
    }

    fn live_guest(&self) -> Result<Option<Arc<tokio::sync::Mutex<InstantiatedGuest>>>, String> {
        self.assert_current()?;
        Ok(self
            .guest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    pub fn assert_current(&self) -> Result<(), String> {
        if self.invalidated {
            return Err(STALE_MESSAGE.to_string());
        }
        let store = self
            .bare_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(store) = store.as_ref() {
            let state = store.data();
            if state.stale || state.generation != self.generation.value() {
                return Err(STALE_MESSAGE.to_string());
            }
        }
        Ok(())
    }

    /// Invalidate the instance: bump the generation and drop the store.
    pub fn invalidate(&mut self) {
        self.generation.bump();
        *self
            .bare_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *self
            .guest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.component = None;
        self.instantiated = false;
        self.invalidated = true;
    }

    pub fn generation(&self) -> u64 {
        self.generation.value()
    }

    pub fn has_store(&self) -> bool {
        self.bare_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
            || self
                .guest
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
    }

    pub fn instantiated(&self) -> bool {
        self.instantiated
    }
}

/// Native extension whose calls are proxied through a WASM component.
pub struct WasmExtension {
    name: String,
    path: String,
    replaceable: bool,
    hidden: bool,
    generation: Arc<AtomicU64>,
    store: Mutex<WasmStore>,
    registration: ExtensionRegistration,
    last_patch: Mutex<Option<Value>>,
}

impl WasmExtension {
    pub fn new(
        name: impl Into<String>,
        bytes: Option<&[u8]>,
        registration: ExtensionRegistration,
        replaceable: bool,
        hidden: bool,
    ) -> Result<Self, String> {
        let name = name.into();
        let mut store = WasmStore::new()?;
        if let Some(bytes) = bytes {
            store.instantiate(bytes)?;
        }
        let generation = Arc::new(AtomicU64::new(store.generation()));
        Ok(Self {
            path: name.clone(),
            name,
            replaceable,
            hidden,
            generation,
            store: Mutex::new(store),
            registration,
            last_patch: Mutex::new(None),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn store(&self) -> &Mutex<WasmStore> {
        &self.store
    }

    pub fn last_patch(&self) -> Option<Value> {
        self.last_patch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Decode a `tool_call` return record and remember it for the host to
    /// apply. The explicit patch avoids relying on in-place guest mutation.
    pub fn decode_tool_call_patch(&self, patch: &ToolCallPatch) -> Result<(), String> {
        self.assert_current()?;
        let mut event = json!({});
        patch.apply(&mut event);
        *self
            .last_patch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event);
        Ok(())
    }

    pub fn decode_tool_result_patch(
        &self,
        event: &Value,
        patch: &ToolResultPatch,
    ) -> Result<Value, String> {
        self.assert_current()?;
        let mut event = event.clone();
        patch.apply(&mut event);
        Ok(event)
    }

    pub fn decode_message_end_patch(
        &self,
        event: &Value,
        patch: &MessageEndPatch,
    ) -> Result<Value, String> {
        self.assert_current()?;
        let mut event = event.clone();
        patch.apply(&mut event)?;
        Ok(event)
    }

    pub fn decode_headers_patch(&self, patch: &HeadersPatch) -> Result<Value, String> {
        self.assert_current()?;
        let mut headers = json!({});
        patch.apply(&mut headers);
        Ok(headers)
    }

    pub fn decode_cache_warming_patch(
        &self,
        patch: &CacheWarmingPatch,
    ) -> Result<Option<String>, String> {
        self.assert_current()?;
        Ok(patch.action.value.clone())
    }

    pub fn set_flag(&self, name: impl Into<String>, value: impl Into<String>) {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bridge
            .flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.into(), value.into());
    }

    pub fn set_tool_handler(&self, handler: HostToolHandler) {
        *self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bridge
            .tool_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handler);
    }

    pub fn notifications(&self) -> Vec<(String, String)> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bridge
            .notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn ensure_guest(
        &self,
    ) -> Result<Option<Arc<tokio::sync::Mutex<InstantiatedGuest>>>, String> {
        self.assert_current()?;
        let prepared = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = store.live_guest()? {
                return Ok(Some(existing));
            }
            store.prepare_instantiation()?
        };
        let Some((engine, component, generation, bridge)) = prepared else {
            return Ok(None);
        };
        let mut linker = Linker::new(&engine);
        wit_bindings::Extension::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|error| format!("link WIT host imports: {error}"))?;
        let mut wasm_store = Store::new(
            &engine,
            WasmState {
                host: None,
                ui: None,
                registrations: None,
                cwd: String::new(),
                generation,
                stale: false,
                bridge,
            },
        );
        let raw_instance = linker
            .instantiate_async(&mut wasm_store, &component)
            .await
            .map_err(|error| format!("instantiate WIT extension: {error}"))?;
        let instance = wit_bindings::Extension::new(&mut wasm_store, &raw_instance)
            .map_err(|error| format!("bind WIT extension exports: {error}"))?;
        let on_event = raw_instance
            .get_typed_func::<(String, String), (Result<Option<String>, String>,)>(
                &mut wasm_store,
                "on-event",
            )
            .ok();
        let guest = Arc::new(tokio::sync::Mutex::new(InstantiatedGuest {
            store: wasm_store,
            instance,
            on_event,
        }));
        let installed = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .install_guest(guest, generation)?;
        Ok(Some(installed))
    }

    pub async fn call_on_tool_call(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<ToolCallPatch>, String> {
        let Some(guest) = self.ensure_guest().await? else {
            return Ok(None);
        };
        let input_json = serde_json::to_string(input).map_err(|e| e.to_string())?;
        let raw = {
            let mut guard = guest.lock().await;
            let InstantiatedGuest {
                store, instance, ..
            } = &mut *guard;
            instance
                .call_on_tool_call(store, name, &input_json)
                .await
                .map_err(|error| format!("guest on-tool-call trap: {error}"))??
        };
        self.assert_current()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let parsed_input = match raw.input_json {
            Some(text) => OptionPatch::set(
                serde_json::from_str::<Value>(&text)
                    .map_err(|error| format!("invalid guest tool-call input JSON: {error}"))?,
            ),
            None => OptionPatch::none(),
        };
        let patch = ToolCallPatch {
            block: raw.block.map_or_else(OptionPatch::none, OptionPatch::set),
            reason: raw.reason.map_or_else(OptionPatch::none, OptionPatch::set),
            input: parsed_input,
            terminate: raw
                .terminate
                .map_or_else(OptionPatch::none, OptionPatch::set),
        };
        self.decode_tool_call_patch(&patch)?;
        Ok(Some(patch))
    }

    pub async fn call_on_before_headers(
        &self,
        headers: &Value,
    ) -> Result<Option<HeadersPatch>, String> {
        let Some(guest) = self.ensure_guest().await? else {
            return Ok(None);
        };
        let headers_json = serde_json::to_string(headers).map_err(|e| e.to_string())?;
        let raw = {
            let mut guard = guest.lock().await;
            let InstantiatedGuest {
                store, instance, ..
            } = &mut *guard;
            instance
                .call_on_before_headers(store, &headers_json)
                .await
                .map_err(|error| format!("guest on-before-headers trap: {error}"))??
        };
        self.assert_current()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let mut set = IndexMap::new();
        for entry in raw.set {
            set.insert(entry.name, entry.value);
        }
        Ok(Some(HeadersPatch {
            set,
            delete: raw.delete,
        }))
    }

    pub async fn call_on_event(
        &self,
        event: ExtensionEvent,
        payload: &Value,
    ) -> Result<Option<Value>, String> {
        let Some(guest) = self.ensure_guest().await? else {
            return Ok(None);
        };
        let result = call_guest_event(&guest, event.as_str(), payload).await?;
        self.assert_current()?;
        Ok(result)
    }

    fn assert_current(&self) -> Result<(), String> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .assert_current()?;
        let current = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation();
        if self.generation.load(Ordering::SeqCst) != current {
            return Err(STALE_MESSAGE.to_string());
        }
        Ok(())
    }
}

#[async_trait]
impl Extension for WasmExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn replaceable(&self) -> bool {
        self.replaceable
    }

    fn hidden(&self) -> bool {
        self.hidden
    }

    fn registration(&self) -> &ExtensionRegistration {
        &self.registration
    }

    fn set_registration(&mut self, registration: ExtensionRegistration) {
        self.registration = registration;
    }

    async fn init(&mut self, ctx: &mut ExtensionInitContext<'_>) -> Result<(), String> {
        self.assert_current()?;
        let Some(guest) = self.ensure_guest().await? else {
            return Ok(());
        };
        let result = {
            let mut guard = guest.lock().await;
            let InstantiatedGuest {
                store,
                instance,
                on_event,
            } = &mut *guard;
            let has_on_event = on_event.is_some();
            store.data_mut().host = Some(Arc::downgrade(ctx.host));
            store.data_mut().cwd = ctx.cwd.to_string();
            store.data_mut().registrations = Some(GuestRegistrations::default());
            instance
                .call_init(&mut *store)
                .await
                .map_err(|error| format!("guest init trap: {error}"))??;
            let registrations = store
                .data_mut()
                .registrations
                .take()
                .ok_or("Missing guest registration state")?;
            if !registrations.subscriptions.is_empty() && !has_on_event {
                return Err(
                    "WASM extension subscribed to events but does not export `on-event`"
                        .to_string(),
                );
            }
            for (name, config) in registrations.providers {
                ctx.register_provider(super::types::ProviderRegistration {
                    name,
                    config,
                    extension_path: ctx.registration.path.clone(),
                })?;
            }
            for (provider, id, definition) in registrations.virtual_models {
                ctx.register_virtual_model(super::types::VirtualModelRegistration {
                    provider,
                    id,
                    definition,
                    extension_path: ctx.registration.path.clone(),
                })?;
            }
            for (name, config) in registrations.mcp_servers {
                ctx.register_mcp_server(super::types::McpServerConfig {
                    name,
                    config,
                    extension_path: ctx.registration.path.clone(),
                })?;
            }
            for tool in registrations.tools {
                ctx.register_tool(tool)?;
            }
            for (name, description) in registrations.commands {
                let weak = Arc::downgrade(&guest);
                let command = name.clone();
                ctx.register_command(super::types::CommandDefinition {
                    name,
                    description,
                    handler: Arc::new(move |args, ctx| {
                        let guest = weak.upgrade();
                        let command = command.clone();
                        Box::pin(async move {
                            ctx.base.host.assert_active()?;
                            let guest = guest.ok_or(STALE_MESSAGE)?;
                            run_guest_command(&guest, &command, &args, &ctx).await
                        })
                    }),
                })?;
            }
            for (key, description, command) in registrations.shortcuts {
                let weak = Arc::downgrade(&guest);
                ctx.register_shortcut_handler(
                    super::types::ShortcutDefinition {
                        key,
                        description,
                        extension_path: ctx.registration.path.clone(),
                    },
                    Arc::new(move |_args, ctx| {
                        let guest = weak.upgrade();
                        let command = command.clone();
                        Box::pin(async move {
                            ctx.base.host.assert_active()?;
                            let guest = guest.ok_or(STALE_MESSAGE)?;
                            run_guest_command(&guest, &command, "", &ctx).await
                        })
                    }),
                )?;
            }
            for mut flag in registrations.flags {
                flag.extension_path.clone_from(&ctx.registration.path);
                ctx.register_flag(flag)?;
            }
            for event in registrations.subscriptions {
                let weak_guest = Arc::downgrade(&guest);
                let weak_host = Arc::downgrade(ctx.host);
                let event_name = event.as_str();
                ctx.on(
                    event,
                    Arc::new(move |payload| {
                        let guest = weak_guest.upgrade();
                        let host = weak_host.upgrade();
                        Box::pin(async move {
                            let host = host.ok_or_else(|| STALE_MESSAGE.to_string())?;
                            host.assert_active()?;
                            let guest = guest.ok_or_else(|| STALE_MESSAGE.to_string())?;
                            call_guest_event(&guest, event_name, &payload).await
                        })
                    }),
                )?;
            }
            Ok(())
        };
        self.assert_current()?;
        result
    }

    async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        self.assert_current()?;
        let Some(guest) = self.ensure_guest().await? else {
            return Ok(ToolOutcome::success(
                json!({ "type": "toolCall", "name": name }),
                json!({ "content": [] }),
            ));
        };
        let args_json = serde_json::to_string(&args).map_err(|e| e.to_string())?;
        let raw = {
            let mut guard = guest.lock().await;
            let InstantiatedGuest {
                store, instance, ..
            } = &mut *guard;
            store.data_mut().ui = Some(ctx.base.ui.clone());
            store.data_mut().cwd.clone_from(&ctx.base.cwd);
            let result = instance
                .call_execute_tool(&mut *store, name, &args_json)
                .await;
            store.data_mut().ui = None;
            result.map_err(|error| format!("guest execute-tool trap: {error}"))??
        };
        self.assert_current()?;
        let payload: Value = serde_json::from_str(&raw.payload_json)
            .map_err(|error| format!("invalid guest tool payload JSON: {error}"))?;
        if raw.ok {
            Ok(ToolOutcome::success(
                json!({ "type": "toolCall", "name": name }),
                payload,
            ))
        } else {
            let message = raw.error.unwrap_or_else(|| "guest tool failed".to_string());
            Ok(ToolOutcome::error("wasm-tool", name, &message))
        }
    }

    async fn run_command(
        &self,
        name: &str,
        args: &str,
        ctx: &super::types::ExtensionCommandContext,
    ) -> Result<(), String> {
        self.assert_current()?;
        let Some(guest) = self.ensure_guest().await? else {
            return Err(format!(
                "WASM extension \"{}\" has no command \"{name}\"",
                self.name
            ));
        };
        let result = run_guest_command(&guest, name, args, ctx).await;
        self.assert_current()?;
        result
    }

    fn invalidate(&self) {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.invalidate();
        self.generation.store(store.generation(), Ordering::SeqCst);
    }
}

/// Decode an `input` result. `handled` short-circuits; `transform` carries an
/// explicit replacement text rather than mutating a borrowed string.
#[derive(Debug, Clone, PartialEq)]
pub enum InputPatch {
    Continue,
    Transform { text: String, images: Option<Value> },
    Handled,
}

impl InputPatch {
    pub fn from_value(value: &Value) -> Self {
        match value.get("action").and_then(Value::as_str) {
            Some("handled") => Self::Handled,
            Some("transform") => Self::Transform {
                text: value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                images: value.get("images").cloned(),
            },
            _ => Self::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn guest_dialogs_receive_host_answers_and_deliver_notifications() {
        use super::wit_bindings::pi::extension::host::Host;
        use crate::core::extensions::types::{ExtensionMode, ExtensionUi};
        let mut ui = ExtensionUi::no_op(ExtensionMode::Tui);
        ui.select = Arc::new(|_, items| Box::pin(async move { items.into_iter().last() }));
        ui.confirm = Arc::new(|_, prompt| Box::pin(async move { prompt == "continue?" }));
        ui.input = Arc::new(|_, placeholder| Box::pin(async move { placeholder }));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let sink = notes.clone();
        ui.notify = Arc::new(move |message, level| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((message, level))
        });
        let mut state = WasmState {
            host: None,
            registrations: None,
            ui: Some(ui),
            cwd: "/work".to_string(),
            generation: 0,
            stale: false,
            bridge: HostBridge::default(),
        };
        assert_eq!(
            state
                .ui_select("select".into(), vec!["a".into(), "b".into()])
                .await
                .as_deref(),
            Some("b")
        );
        assert!(state.ui_confirm("title".into(), "continue?".into()).await);
        assert_eq!(
            state
                .ui_input("input".into(), Some("value".into()))
                .await
                .as_deref(),
            Some("value")
        );
        state
            .ui_notify("message".into(), Some("warning".into()))
            .await;
        assert_eq!(
            *notes.lock().unwrap_or_else(|e| e.into_inner()),
            vec![("message".to_owned(), "warning".to_owned())]
        );
    }

    use super::*;

    fn extension() -> WasmExtension {
        WasmExtension::new(
            "guest",
            None,
            ExtensionRegistration::new("guest"),
            false,
            false,
        )
        .expect("wasm extension")
    }

    #[test]
    fn option_patch_distinguishes_absent_and_set() {
        assert!(!OptionPatch::<String>::none().changed());
        let patch = OptionPatch::set("x".to_string());
        assert!(patch.changed());
        assert_eq!(patch.value.as_deref(), Some("x"));
    }

    #[test]
    fn headers_patch_sets_and_deletes() {
        let mut headers = json!({ "keep": "1", "drop": "2" });
        let patch = HeadersPatch {
            set: IndexMap::from([("added".to_string(), "3".to_string())]),
            delete: vec!["drop".to_string()],
        };
        patch.apply(&mut headers);
        assert_eq!(headers, json!({ "keep": "1", "added": "3" }));
    }

    #[test]
    fn tool_call_patch_carries_explicit_input() {
        let patch = ToolCallPatch {
            block: OptionPatch::set(true),
            reason: OptionPatch::set("denied".to_string()),
            input: OptionPatch::set(json!({"command":"ls"})),
            terminate: OptionPatch::none(),
        };
        let mut event = json!({});
        patch.apply(&mut event);
        assert_eq!(event["block"], true);
        assert_eq!(event["reason"], "denied");
        assert_eq!(event["input"], json!({"command":"ls"}));
        assert!(event.get("terminate").is_none());
    }

    #[test]
    fn tool_result_content_replacement_drops_structured_content() {
        let patch = ToolResultPatch {
            content: OptionPatch::set(json!([{"type":"text","text":"redacted"}])),
            ..ToolResultPatch::default()
        };
        let mut event = json!({"content": [], "structuredContent": {"secret": 1}});
        assert!(patch.apply(&mut event));
        assert!(event.get("structuredContent").is_none());
    }

    #[test]
    fn tool_result_structured_replacement_is_kept() {
        let patch = ToolResultPatch {
            content: OptionPatch::set(json!([])),
            structured_content: OptionPatch::set(json!({"ok": true})),
            ..ToolResultPatch::default()
        };
        let mut event = json!({"content": [], "structuredContent": {"old": true}});
        assert!(patch.apply(&mut event));
        assert_eq!(event["structuredContent"], json!({"ok": true}));
    }

    #[test]
    fn message_end_patch_rejects_role_change() {
        let patch = MessageEndPatch {
            message: OptionPatch::set(json!({"role":"user"})),
            ..MessageEndPatch::default()
        };
        let mut event = json!({"role":"assistant"});
        assert!(patch.apply(&mut event).is_err());
    }

    #[test]
    fn generation_token_bumps_on_invalidation() {
        let extension = extension();
        let before = extension.generation();
        assert_eq!(before, 1);
        extension.invalidate();
        assert_eq!(extension.generation(), before + 1);
        assert!(!extension.store().lock().expect("poisoned").has_store());
    }

    #[tokio::test]
    async fn invalidated_guest_calls_fail_with_stale_message() {
        let extension = extension();
        assert!(
            extension
                .decode_cache_warming_patch(&CacheWarmingPatch::default())
                .is_ok()
        );
        extension.invalidate();
        let error = extension
            .decode_cache_warming_patch(&CacheWarmingPatch {
                action: OptionPatch::set("warm".to_string()),
            })
            .expect_err("stale");
        assert!(error.contains("stale"));
    }

    #[tokio::test]
    async fn invalidated_extension_rejects_tool_and_command_calls() {
        let extension = extension();
        extension.invalidate();
        assert!(
            extension
                .decode_tool_call_patch(&ToolCallPatch::default())
                .is_err()
        );
    }

    #[test]
    fn input_patch_decodes_all_actions() {
        assert_eq!(InputPatch::from_value(&json!({})), InputPatch::Continue);
        assert_eq!(
            InputPatch::from_value(&json!({"action":"handled"})),
            InputPatch::Handled
        );
        assert_eq!(
            InputPatch::from_value(&json!({"action":"transform","text":"new"})),
            InputPatch::Transform {
                text: "new".to_string(),
                images: None
            }
        );
    }

    #[test]
    fn wasm_extension_is_a_native_extension_trait_object() {
        let extension: Box<dyn Extension> = Box::new(extension());
        assert_eq!(extension.name(), "guest");
        assert_eq!(extension.path(), "guest");
        assert!(!extension.replaceable());
        assert!(!extension.hidden());
    }

    #[tokio::test]
    async fn host_exec_runs_command_with_non_interactive_env_and_timeout() {
        use super::wit_bindings::pi::extension::host::Host;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = WasmState {
            host: None,
            registrations: None,
            ui: None,
            cwd: dir.path().to_string_lossy().into_owned(),
            generation: 1,
            stale: false,
            bridge: HostBridge::default(),
        };

        let ok = state
            .exec(
                "sh".into(),
                vec![
                    "-c".into(),
                    r#"printf '%s|%s' "$PAGER" "$GIT_TERMINAL_PROMPT""#.into(),
                ],
                None,
                Some(5_000),
            )
            .await
            .expect("exec should succeed");
        assert_eq!(ok.exit_code, 0);
        assert_eq!(ok.stdout, "cat|0");
        assert!(!ok.timed_out);

        let timed = state
            .exec(
                "sh".into(),
                vec!["-c".into(), "sleep 5".into()],
                None,
                Some(50),
            )
            .await
            .expect("timed-out exec returns structured result");
        assert!(timed.timed_out);
        assert_eq!(timed.exit_code, -1);

        state.stale = true;
        let stale_err = state
            .exec("sh".into(), vec!["-c".into(), "echo hi".into()], None, None)
            .await
            .expect_err("stale guest must not exec");
        assert!(stale_err.contains("stale"));
    }

    #[tokio::test]
    async fn host_exec_drains_large_output_without_sigpipe() {
        use super::wit_bindings::pi::extension::host::Host;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = WasmState {
            host: None,
            registrations: None,
            ui: None,
            cwd: dir.path().to_string_lossy().into_owned(),
            generation: 1,
            stale: false,
            bridge: HostBridge::default(),
        };

        let large = state
            .exec(
                "sh".into(),
                vec![
                    "-c".into(),
                    "head -c 2000000 /dev/zero | tr '\\0' 'a'".into(),
                ],
                None,
                Some(5_000),
            )
            .await
            .expect("large output command should succeed");
        assert_eq!(large.exit_code, 0);
        assert!(!large.timed_out);
        assert_eq!(large.stdout.len(), EXEC_MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn host_exec_honors_host_invalidation() {
        use super::wit_bindings::pi::extension::host::Host;
        use crate::core::event_bus::EventBus;
        use crate::core::extensions::types::ExtensionHost;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus));
        host.commit();

        let mut state = WasmState {
            host: Some(Arc::downgrade(&host)),
            registrations: None,
            ui: None,
            cwd: dir.path().to_string_lossy().into_owned(),
            generation: 1,
            stale: false,
            bridge: HostBridge::default(),
        };

        host.invalidate(None);
        let err = state
            .exec("sh".into(), vec!["-c".into(), "echo hi".into()], None, None)
            .await
            .expect_err("invalidated host must reject exec");
        assert!(err.contains("stale"));
    }

    #[tokio::test]
    async fn host_subscribe_and_session_actions_enforce_contracts() {
        use super::wit_bindings::pi::extension::host::Host;
        use crate::core::event_bus::EventBus;
        use crate::core::extensions::types::{ExtensionHost, ExtensionHostActions};

        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus));
        host.commit();

        let active_tools = Arc::new(Mutex::new(vec!["read".to_string(), "bash".to_string()]));
        let sent_messages = Arc::new(Mutex::new(Vec::<(String, Value, bool)>::new()));
        let appended = Arc::new(Mutex::new(Vec::<(String, Option<Value>)>::new()));

        let tools_get = active_tools.clone();
        let tools_set = active_tools.clone();
        let sent_ref = sent_messages.clone();
        let appended_ref = appended.clone();
        host.set_actions(ExtensionHostActions {
            get_model: Some(Arc::new(|| Some("anthropic/claude-sonnet-4".to_string()))),
            get_active_tools: Some(Arc::new(move || {
                tools_get.lock().unwrap_or_else(|e| e.into_inner()).clone()
            })),
            set_active_tools: Some(Arc::new(move |next| {
                *tools_set.lock().unwrap_or_else(|e| e.into_inner()) = next;
                Ok(())
            })),
            send_message: Some(Arc::new(move |kind, content, trigger| {
                sent_ref
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((kind, content, trigger));
                Ok(())
            })),
            append_entry: Some(Arc::new(move |kind, data| {
                appended_ref
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((kind, data));
                Ok("entry-42".to_string())
            })),
        });

        let mut state = WasmState {
            host: Some(Arc::downgrade(&host)),
            registrations: Some(GuestRegistrations::default()),
            ui: None,
            cwd: "/workspace".to_string(),
            generation: 1,
            stale: false,
            bridge: HostBridge::default(),
        };

        assert_eq!(state.get_cwd().await, "/workspace");
        assert_eq!(
            state.get_model().await.as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(state.get_active_tools().await, vec!["read", "bash"]);
        state
            .set_active_tools(vec!["read".to_string()])
            .await
            .expect("set_active_tools");
        assert_eq!(state.get_active_tools().await, vec!["read"]);

        state
            .send_message("self-update".into(), r#"{"version":"v1.7.0"}"#.into(), true)
            .await
            .expect("send_message");
        assert_eq!(
            *sent_messages.lock().unwrap_or_else(|e| e.into_inner()),
            vec![(
                "self-update".to_string(),
                json!({"version": "v1.7.0"}),
                true
            )]
        );

        let entry_id = state
            .append_entry("audit".into(), Some(r#"{"ok":true}"#.into()))
            .await
            .expect("append_entry");
        assert_eq!(entry_id, "entry-42");

        state
            .subscribe("tool_result".into())
            .await
            .expect("valid event");
        state
            .subscribe("tool_result".into())
            .await
            .expect("dedup event");
        assert!(state.subscribe("not_a_real_event".into()).await.is_err());
        state
            .register_shortcut("ctrl+u".into(), Some("Update".into()), "self-update".into())
            .await
            .expect("register_shortcut");
        state
            .register_mcp_server("docs".into(), r#"{"command":"docs-mcp"}"#.into())
            .await
            .expect("register_mcp_server");

        let regs = state.registrations.take().expect("registrations");
        assert_eq!(regs.subscriptions, vec![ExtensionEvent::ToolResult]);
        assert_eq!(regs.shortcuts.len(), 1);
        assert_eq!(regs.mcp_servers.len(), 1);
    }
}
