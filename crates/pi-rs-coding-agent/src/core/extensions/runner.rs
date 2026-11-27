//! Extension runner: registration rules, dispatch modes, and the pre-load
//! transaction. Mirrors `packages/coding-agent/src/core/extensions/runner.ts`.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::core::event_bus::{EventBus, EventHandler, ExtensionEvent};
use crate::core::skills::ResourceDiagnostic;

use super::types::{
    CommandDefinition, CommandHandler, Extension, ExtensionError, ExtensionFactory, ExtensionHost,
    ExtensionInitContext, ExtensionMode, ExtensionRegistration, FlagDefinition, FlagValue,
    LoadExtensionsResult, MarkdownTransformerDefinition, McpServerConfig, ProviderRegistration,
    ShortcutDefinition, ToolDefinition, ToolRenderers, VirtualModelRegistration,
};

use std::collections::BTreeSet;

/// Canonical keybinding ids that extensions may never override. From
/// `runner.ts#L98-L117`.
pub const RESERVED_KEYBINDINGS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

#[derive(Clone)]
pub struct ResolvedCommand {
    pub name: String,
    pub invocation_name: String,
    pub definition: CommandDefinition,
    pub extension_path: String,
}

/// Owns loaded extensions and applies the upstream registration rules.
pub struct ExtensionRunner {
    extensions: Mutex<Vec<Arc<dyn Extension>>>,
    late_tools: Mutex<Vec<ToolDefinition>>,
    pub host: Arc<ExtensionHost>,
    pub bus: Arc<EventBus>,
    pub cwd: String,
    mode: Mutex<ExtensionMode>,
    has_ui: Mutex<bool>,
    shortcut_diagnostics: Mutex<Vec<ResourceDiagnostic>>,
    command_diagnostics: Mutex<Vec<ResourceDiagnostic>>,
    reported_mcp_servers: Mutex<Vec<String>>,
}

impl ExtensionRunner {
    pub fn new(
        extensions: Vec<Arc<dyn Extension>>,
        host: Arc<ExtensionHost>,
        bus: Arc<EventBus>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            extensions: Mutex::new(extensions),
            late_tools: Mutex::new(Vec::new()),
            host,
            bus,
            cwd: cwd.into(),
            mode: Mutex::new(ExtensionMode::Print),
            has_ui: Mutex::new(false),
            shortcut_diagnostics: Mutex::new(Vec::new()),
            command_diagnostics: Mutex::new(Vec::new()),
            reported_mcp_servers: Mutex::new(Vec::new()),
        }
    }

    pub fn set_ui(&self, mode: ExtensionMode, has_ui: bool) {
        *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
        *self
            .has_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = has_ui;
    }

    pub fn mode(&self) -> ExtensionMode {
        *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn has_ui(&self) -> bool {
        *self
            .has_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn extensions(&self) -> Vec<Arc<dyn Extension>> {
        self.extensions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn replace_loaded_extensions(&self, extensions: Vec<Arc<dyn Extension>>) {
        *self
            .extensions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = extensions;
    }

    pub fn extension_paths(&self) -> Vec<String> {
        self.extensions()
            .iter()
            .map(|extension| extension.path().to_string())
            .collect()
    }

    /// Pre-load transaction. Each extension is initialized in its own
    /// `Loading` phase; on error its queued flag defaults, providers, MCP
    /// servers, and event-bus listeners are discarded atomically.
    pub async fn load_extensions(&self, factories: &[ExtensionFactory]) -> LoadExtensionsResult {
        let mut extensions = Vec::new();
        let mut errors = Vec::new();
        let warnings = Vec::new();

        for factory in factories {
            let mut extension = match factory() {
                Ok(extension) => extension,
                Err(error) => {
                    errors.push(ExtensionError {
                        extension_path: "<factory>".to_string(),
                        event: "load".to_string(),
                        error,
                        stack: None,
                    });
                    continue;
                }
            };
            let path = extension.path().to_string();
            self.bus.begin_transaction();
            self.host.begin_loading();
            let mut registration = ExtensionRegistration::new(path.clone());
            registration.replaceable = extension.replaceable();
            registration.hidden = extension.hidden();
            let init_result = {
                let mut context = ExtensionInitContext {
                    registration: &mut registration,
                    host: &self.host,
                    bus: &self.bus,
                    cwd: &self.cwd,
                };
                extension.init(&mut context).await
            };
            match init_result {
                Ok(()) => {
                    self.bus.commit();
                    self.host.commit();
                    extension.set_registration(registration);
                    extensions.push(Arc::from(extension) as Arc<dyn Extension>);
                }
                Err(error) => {
                    self.bus.rollback();
                    self.host.rollback();
                    errors.push(ExtensionError {
                        extension_path: path,
                        event: "init".to_string(),
                        error,
                        stack: None,
                    });
                }
            }
        }

        *self
            .extensions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = extensions.clone();
        self.late_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        LoadExtensionsResult {
            extensions,
            errors,
            warnings,
        }
    }

    /// Registration rules: first registration per tool name wins, in load
    /// order.
    pub fn get_all_registered_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = IndexMap::new();
        for tool in self
            .late_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
        {
            tools
                .entry(tool.name.clone())
                .or_insert_with(|| tool.clone());
        }
        for extension in self.extensions() {
            if let Some(dynamic) = extension.dynamic_tools() {
                for tool in dynamic {
                    tools.entry(tool.name.clone()).or_insert(tool);
                }
            } else {
                for tool in extension.registration().tools.values() {
                    tools
                        .entry(tool.name.clone())
                        .or_insert_with(|| tool.clone());
                }
            }
        }
        tools.into_values().collect()
    }

    pub fn get_tool_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.late_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
            .or_else(|| {
                self.extensions().iter().find_map(|extension| {
                    if let Some(dynamic) = extension.dynamic_tools() {
                        dynamic.into_iter().find(|tool| tool.name == name)
                    } else {
                        extension.registration().tools.get(name).cloned()
                    }
                })
            })
    }

    /// Late tool registration: first registration per name wins and the
    /// session tool list is refreshed (upstream `refresh_tools()`).
    pub fn register_tool(
        &self,
        extension_name: &str,
        definition: ToolDefinition,
    ) -> Result<(), String> {
        if !self
            .extensions()
            .iter()
            .any(|extension| extension.name() == extension_name)
        {
            return Err(format!("unknown extension: {extension_name}"));
        }
        if self.get_tool_definition(&definition.name).is_some() {
            return Ok(());
        }
        self.late_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(definition);
        self.host.refresh_tools();
        Ok(())
    }

    pub fn refresh_tools(&self) {
        self.host.refresh_tools();
    }

    /// Flags are first-wins across extensions.
    pub fn get_flags(&self) -> IndexMap<String, FlagDefinition> {
        let mut flags = IndexMap::new();
        for extension in self.extensions() {
            for (name, flag) in &extension.registration().flags {
                flags.entry(name.clone()).or_insert_with(|| flag.clone());
            }
        }
        flags
    }

    pub fn set_flag_value(&self, name: &str, value: FlagValue) {
        self.host.set_flag_value(name, value);
    }

    pub fn get_flag_values(&self) -> IndexMap<String, FlagValue> {
        self.host.flag_values()
    }

    fn builtin_keybindings(
        resolved: &IndexMap<String, Vec<String>>,
    ) -> IndexMap<String, (String, bool)> {
        let mut bindings = IndexMap::new();
        for (action, keys) in resolved {
            let restricted = RESERVED_KEYBINDINGS.contains(&action.as_str());
            for key in keys {
                let normalized = key.to_lowercase();
                let existing = bindings.get(&normalized);
                if existing.is_some_and(|(_, restricted)| *restricted) && !restricted {
                    continue;
                }
                bindings.insert(normalized, (action.clone(), restricted));
            }
        }
        bindings
    }

    /// Shortcut rules: reserved built-in keys are skipped with a diagnostic,
    /// non-reserved built-in conflicts warn but are allowed, and two extensions
    /// competing for one key warn with the later one winning.
    pub fn get_shortcuts(
        &self,
        resolved: &IndexMap<String, Vec<String>>,
    ) -> IndexMap<String, ShortcutDefinition> {
        let mut diagnostics = Vec::new();
        let builtins = Self::builtin_keybindings(resolved);
        let mut shortcuts: IndexMap<String, ShortcutDefinition> = IndexMap::new();
        for extension in self.extensions() {
            for (key, shortcut) in &extension.registration().shortcuts {
                if let Some((_action, restricted)) = builtins.get(key)
                    && *restricted
                {
                    diagnostics.push(ResourceDiagnostic::warning(
                        format!(
                            "Extension shortcut '{key}' from {} conflicts with built-in shortcut. Skipping.",
                            shortcut.extension_path
                        ),
                        None,
                    ));
                    continue;
                }
                if let Some((action, _)) = builtins.get(key) {
                    diagnostics.push(ResourceDiagnostic::warning(
                        format!(
                            "Extension shortcut conflict: '{key}' is built-in shortcut for {action} and {}. Using {}.",
                            shortcut.extension_path, shortcut.extension_path
                        ),
                        None,
                    ));
                }
                if let Some(existing) = shortcuts.get(key) {
                    diagnostics.push(ResourceDiagnostic::warning(
                        format!(
                            "Extension shortcut conflict: '{key}' registered by both {} and {}. Using {}.",
                            existing.extension_path, shortcut.extension_path, shortcut.extension_path
                        ),
                        None,
                    ));
                }
                shortcuts.insert(key.clone(), shortcut.clone());
            }
        }
        *self
            .shortcut_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = diagnostics;
        shortcuts
    }

    pub fn shortcut_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.shortcut_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Commands with duplicate names are disambiguated in insertion order as
    /// `name:1`, `name:2`. A name is only suffixed when more than one extension
    /// registered it.
    pub fn get_registered_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands = Vec::new();
        let mut counts = IndexMap::new();
        for extension in self.extensions() {
            for command in extension.registration().commands.values() {
                commands.push((command.clone(), extension.path().to_owned()));
                *counts.entry(command.name.clone()).or_insert(0usize) += 1;
            }
        }

        let mut seen = IndexMap::new();
        let mut taken = BTreeSet::new();
        let mut resolved = Vec::new();
        for (command, extension_path) in commands {
            let occurrence = seen.entry(command.name.clone()).or_insert(0usize);
            *occurrence += 1;
            let mut invocation = if counts.get(&command.name).copied().unwrap_or(0) > 1 {
                format!("{}:{}", command.name, occurrence)
            } else {
                command.name.clone()
            };
            let mut suffix = *occurrence;
            while taken.contains(&invocation) {
                suffix += 1;
                invocation = format!("{}:{suffix}", command.name);
            }
            taken.insert(invocation.clone());
            resolved.push(ResolvedCommand {
                name: command.name.clone(),
                invocation_name: invocation,
                definition: command,
                extension_path,
            });
        }
        *self
            .command_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Vec::new();
        resolved
    }

    pub fn command_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.command_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn get_command(&self, invocation_name: &str) -> Option<ResolvedCommand> {
        self.get_registered_commands()
            .into_iter()
            .find(|command| command.invocation_name == invocation_name)
    }

    pub async fn execute_command(
        &self,
        invocation_name: &str,
        args: &str,
        context: &super::types::ExtensionCommandContext,
    ) -> Result<(), String> {
        let command = self
            .get_command(invocation_name)
            .ok_or_else(|| format!("unknown command: {invocation_name}"))?;
        let owner = self
            .extensions()
            .into_iter()
            .find(|extension| extension.path() == command.extension_path)
            .ok_or_else(|| format!("command owner missing: {invocation_name}"))?;
        owner.run_command(&command.name, args, context).await
    }

    pub(crate) async fn execute_shortcut(
        &self,
        shortcut: &ShortcutDefinition,
        context: &super::types::ExtensionCommandContext,
    ) -> Result<(), String> {
        self.host.assert_active()?;
        let handler = self
            .extensions()
            .into_iter()
            .find_map(|extension| {
                (extension.path() == shortcut.extension_path)
                    .then(|| {
                        extension
                            .registration()
                            .shortcut_handlers
                            .get(&shortcut.key)
                            .cloned()
                    })
                    .flatten()
            })
            .ok_or_else(|| {
                format!(
                    "No handler registered for extension shortcut '{}'",
                    shortcut.key
                )
            })?;
        handler(String::new(), context.clone()).await
    }

    pub fn get_message_renderer(&self, custom_type: &str) -> Option<String> {
        self.extensions().iter().find_map(|extension| {
            extension
                .registration()
                .message_renderers
                .iter()
                .find(|renderer| renderer.as_str() == custom_type)
                .cloned()
        })
    }

    pub fn get_entry_renderer(&self, custom_type: &str) -> Option<String> {
        self.extensions().iter().find_map(|extension| {
            extension
                .registration()
                .entry_renderers
                .iter()
                .find(|renderer| renderer.as_str() == custom_type)
                .cloned()
        })
    }

    pub fn markdown_transformers(&self) -> Vec<MarkdownTransformerDefinition> {
        self.extensions()
            .iter()
            .filter_map(|extension| extension.registration().markdown_transformer.clone())
            .collect()
    }

    pub fn resolve_tool_renderers(&self, tool_name: &str) -> Option<ToolRenderers> {
        self.extensions().iter().find_map(|extension| {
            extension
                .registration()
                .tool_renderers
                .iter()
                .find_map(|renderer| renderer(tool_name))
        })
    }

    pub fn has_handlers(&self, event: ExtensionEvent) -> bool {
        self.bus.has_handlers(event)
            || self.extensions().iter().any(|extension| {
                extension
                    .handlers()
                    .get(&event)
                    .is_some_and(|handlers| !handlers.is_empty())
            })
    }

    pub async fn emit(&self, event: ExtensionEvent, data: Value) -> Vec<String> {
        self.bus.emit(event, data).await
    }

    pub async fn emit_project_trust(&self, data: Value) -> Option<Value> {
        self.bus
            .emit_first(ExtensionEvent::ProjectTrust, data, |result| {
                result.get("trusted").and_then(Value::as_str) != Some("undecided")
            })
            .await
    }

    pub async fn emit_session_before_switch(&self, data: Value) -> Option<Value> {
        self.bus
            .emit_first(ExtensionEvent::SessionBeforeSwitch, data, |result| {
                result.get("cancel") == Some(&json!(true))
            })
            .await
    }

    pub async fn emit_tool_call(&self, data: Value) -> Option<Value> {
        let updated_input = std::sync::Mutex::new(false);
        let blocker = std::sync::Mutex::new(None::<Value>);
        let (state, blocked) = self
            .bus
            .emit_chain_until(ExtensionEvent::ToolCall, data, |state, result| {
                if let Some(input) = result.get("input").filter(|v| !v.is_null()) {
                    state["input"] = input.clone();
                    if let Ok(mut flag) = updated_input.lock() {
                        *flag = true;
                    }
                }
                if result.get("block") == Some(&json!(true)) {
                    if let Ok(mut guard) = blocker.lock() {
                        *guard = Some(result.clone());
                    }
                    return true;
                }
                false
            })
            .await;
        if blocked {
            return blocker.into_inner().ok().flatten();
        }
        if updated_input.into_inner().unwrap_or(false) {
            return Some(json!({ "input": state["input"].clone() }));
        }
        None
    }

    pub async fn emit_user_bash(&self, data: Value) -> Option<Value> {
        self.bus
            .emit_first(ExtensionEvent::UserBash, data, |result| {
                result.get("operations").is_some() != result.get("result").is_some()
            })
            .await
    }

    /// Input transforms chain; `action: "handled"` short-circuits after any
    /// preceding transforms have been applied.
    pub async fn emit_input(
        &self,
        text: &str,
        images: Option<Value>,
        source: &str,
        streaming_behavior: Option<&str>,
    ) -> Value {
        let mut current_text = text.to_string();
        let mut current_images = images.clone();
        let mut handled = false;
        for handler in self.bus.handlers(ExtensionEvent::Input) {
            let mut data = json!({
                "type": "input",
                "text": current_text,
                "source": source,
            });
            if let Some(images) = &current_images {
                data["images"] = images.clone();
            }
            if let Some(behavior) = streaming_behavior {
                data["streamingBehavior"] = json!(behavior);
            }
            if let Some(result) = self.bus.call(ExtensionEvent::Input, &handler, data).await {
                match result.get("action").and_then(Value::as_str) {
                    Some("handled") => {
                        handled = true;
                        break;
                    }
                    Some("transform") => {
                        if let Some(new_text) = result.get("text").and_then(Value::as_str) {
                            current_text = new_text.to_string();
                        }
                        if let Some(new_images) = result.get("images") {
                            current_images = Some(new_images.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        if handled {
            return json!({ "action": "handled" });
        }
        if current_text != text || current_images != images {
            let mut result = json!({ "action": "transform", "text": current_text });
            if let Some(images) = current_images {
                result["images"] = images;
            }
            result
        } else {
            json!({ "action": "continue" })
        }
    }

    /// Chained transform: each handler sees the previous handler's payload.
    pub async fn emit_before_provider_request(&self, payload: Value) -> Value {
        let mut current = payload;
        for handler in self.bus.handlers(ExtensionEvent::BeforeProviderRequest) {
            let data = json!({ "type": "before_provider_request", "payload": current });
            if let Some(result) = self
                .bus
                .call(ExtensionEvent::BeforeProviderRequest, &handler, data)
                .await
            {
                current = result;
            }
        }
        current
    }

    /// Headers are mutated in place by upstream handlers. The returned map is
    /// the accumulated patch: `null` deletes a header.
    pub async fn emit_before_provider_headers(&self, headers: Value) -> Value {
        let mut current = headers;
        for handler in self.bus.handlers(ExtensionEvent::BeforeProviderHeaders) {
            let data = json!({ "type": "before_provider_headers", "headers": current });
            if let Some(result) = self
                .bus
                .call(ExtensionEvent::BeforeProviderHeaders, &handler, data)
                .await
            {
                let patch = result.get("headers").unwrap_or(&result);
                if let Some(object) = patch.as_object() {
                    for (key, value) in object {
                        if value.is_null() {
                            if let Some(map) = current.as_object_mut() {
                                map.remove(key);
                            }
                        } else if let Some(map) = current.as_object_mut() {
                            map.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
        }
        current
    }

    /// Chained transform with a role guard: a replacement message with a
    /// different role is reported and ignored.
    pub async fn emit_message_end(&self, message: Value) -> Option<Value> {
        let mut current = message;
        let mut modified = false;
        for handler in self.bus.handlers(ExtensionEvent::MessageEnd) {
            let data = json!({ "type": "message_end", "message": current });
            let Some(result) = self
                .bus
                .call(ExtensionEvent::MessageEnd, &handler, data)
                .await
            else {
                continue;
            };
            let Some(replacement) = result.get("message") else {
                continue;
            };
            if replacement.get("role") != current.get("role") {
                self.host.report_error(ExtensionError {
                    extension_path: "<extension>".to_string(),
                    event: "message_end".to_string(),
                    error: "message_end handlers must return a message with the same role"
                        .to_string(),
                    stack: None,
                });
                continue;
            }
            current = replacement.clone();
            modified = true;
        }
        modified.then_some(current)
    }

    /// Chained transform for tool results. Replacing `content` without also
    /// replacing `structuredContent` drops it, because it may no longer match.
    pub async fn emit_tool_result(&self, event: Value) -> Option<Value> {
        let mut current = event;
        let mut modified = false;
        for handler in self.bus.handlers(ExtensionEvent::ToolResult) {
            let data = current.clone();
            let Some(result) = self
                .bus
                .call(ExtensionEvent::ToolResult, &handler, data)
                .await
            else {
                continue;
            };
            if let Some(content) = result.get("content") {
                current["content"] = content.clone();
                if let Some(structured) = result.get("structuredContent") {
                    current["structuredContent"] = structured.clone();
                } else if let Some(object) = current.as_object_mut() {
                    object.remove("structuredContent");
                }
                modified = true;
            }
            for field in ["details", "structuredContent", "isError", "usage"] {
                if let Some(value) = result.get(field) {
                    current[field] = value.clone();
                    modified = true;
                }
            }
        }
        modified.then_some(current)
    }

    /// Sequentially dispatch `before_agent_start`, chaining `systemPrompt` / `sections`
    /// mutations and collecting any injected custom messages.
    pub async fn emit_before_agent_start(&self, event: Value) -> Value {
        let mut current_system_prompt = event
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut current_sections = event.get("sections").and_then(Value::as_object).cloned();
        let mut messages = Vec::new();
        for handler in self.bus.handlers(ExtensionEvent::BeforeAgentStart) {
            let mut data = event.clone();
            if let Some(map) = data.as_object_mut() {
                map.insert("type".to_owned(), json!("before_agent_start"));
                if let Some(sp) = &current_system_prompt {
                    map.insert("systemPrompt".to_owned(), json!(sp));
                }
                if let Some(sections) = &current_sections {
                    map.insert("sections".to_owned(), Value::Object(sections.clone()));
                }
            }
            let Some(result) = self
                .bus
                .call(ExtensionEvent::BeforeAgentStart, &handler, data)
                .await
            else {
                continue;
            };
            if let Some(message) = result.get("message").filter(|value| !value.is_null()) {
                messages.push(message.clone());
            }
            if let Some(sp) = result.get("systemPrompt").and_then(Value::as_str) {
                current_system_prompt = Some(sp.to_owned());
            }
            if let Some(sections) = result.get("sections").and_then(Value::as_object) {
                let target = current_sections.get_or_insert_with(serde_json::Map::new);
                for (name, content) in sections {
                    if content.is_null() {
                        target.remove(name);
                    } else {
                        target.insert(name.clone(), content.clone());
                    }
                }
            }
        }
        let mut out = serde_json::Map::new();
        if !messages.is_empty() {
            out.insert("messages".to_owned(), Value::Array(messages));
        }
        if let Some(sp) = current_system_prompt {
            out.insert("systemPrompt".to_owned(), Value::String(sp));
        }
        if let Some(sections) = current_sections {
            out.insert("sections".to_owned(), Value::Object(sections));
        }
        Value::Object(out)
    }

    pub async fn emit_cache_warming_decision(&self, event: Value) -> Value {
        let winner = self
            .bus
            .emit_last(
                ExtensionEvent::CacheWarmingDecision,
                event.clone(),
                |result| result.get("action").cloned(),
            )
            .await;
        match winner {
            Some(action) => {
                let mut event = event;
                event["action"] = action;
                event
            }
            None => event,
        }
    }

    pub async fn emit_context(&self, messages: Vec<Value>) -> Vec<Value> {
        let mut current = messages;
        for event in [ExtensionEvent::Context, ExtensionEvent::ContextWithSystem] {
            for handler in self.bus.handlers(event) {
                let data = json!({ "type": event.as_str(), "messages": current });
                if let Some(result) = self.bus.call(event, &handler, data).await
                    && let Some(messages) = result.get("messages").and_then(Value::as_array)
                {
                    current = messages.clone();
                }
            }
        }
        current
    }

    /// Collect resource paths contributed by `resources_discover` handlers.
    pub async fn emit_resources_discover(&self, reason: &str) -> Vec<Value> {
        let mut discovered = Vec::new();
        let data = json!({ "type": "resources_discover", "cwd": self.cwd, "reason": reason });
        for handler in self.bus.handlers(ExtensionEvent::ResourcesDiscover) {
            if let Some(result) = self
                .bus
                .call(ExtensionEvent::ResourcesDiscover, &handler, data.clone())
                .await
            {
                discovered.push(result);
            }
        }
        discovered
    }

    pub fn report_unhandled_mcp_servers(&self) {
        if self.has_handlers(ExtensionEvent::McpServersChange) {
            return;
        }
        let mut reported = self
            .reported_mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for server in self.host.mcp_servers() {
            if reported.contains(&server.name) {
                continue;
            }
            reported.push(server.name.clone());
            self.host.report_error(ExtensionError {
                extension_path: server.extension_path,
                event: "register_mcp_server".to_string(),
                error: format!(
                    "MCP server \"{}\" is registered, but no loaded extension connects MCP servers",
                    server.name
                ),
                stack: None,
            });
        }
    }

    /// Tear down this generation: contexts go stale, bus listeners are
    /// unsubscribed, and WASM stores are dropped.
    pub fn invalidate(&self) {
        self.host.invalidate(None);
        for extension in self.extensions() {
            extension.invalidate();
        }
    }

    pub fn bind_provider_registrations(&self) -> Vec<ProviderRegistration> {
        self.host.take_pending_providers()
    }

    pub fn bind_virtual_models(&self) -> Vec<VirtualModelRegistration> {
        self.host.take_pending_virtual_models()
    }
}

/// Build a command definition with a boxed async handler.
pub fn command(name: impl Into<String>, handler: CommandHandler) -> CommandDefinition {
    CommandDefinition {
        name: name.into(),
        description: None,
        handler,
    }
}

/// Flush pending MCP registrations into the runner (used by `/reload`).
pub fn register_mcp(runner: &ExtensionRunner, server: McpServerConfig) -> Result<(), String> {
    runner.host.register_mcp_server(server)
}

/// Register a provider through the shared host.
pub fn register_provider(host: &Arc<ExtensionHost>, registration: ProviderRegistration) {
    host.register_provider(registration);
}

/// Register a virtual model through the shared host.
pub fn register_virtual_model(host: &Arc<ExtensionHost>, registration: VirtualModelRegistration) {
    host.register_virtual_model(registration);
}

/// Register a shortcut definition with the given normalized key.
pub fn shortcut(key: impl Into<String>, extension_path: impl Into<String>) -> ShortcutDefinition {
    ShortcutDefinition {
        key: key.into().to_lowercase(),
        description: None,
        extension_path: extension_path.into(),
    }
}

/// Register an event handler with a plain JSON-returning closure.
pub fn handler<F>(function: F) -> EventHandler
where
    F: Fn(Value) -> Result<Option<Value>, String> + Send + Sync + 'static,
{
    super::types::sync_handler(function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::types::{ExtensionSetup, ExtensionToolContext, ToolOutcome};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestExtension {
        name: String,
        registration: ExtensionRegistration,
        setup: ExtensionSetup,
        replaceable: bool,
    }

    impl TestExtension {
        fn new(name: &str, setup: ExtensionSetup) -> Self {
            Self {
                name: name.to_string(),
                registration: ExtensionRegistration::new(name),
                setup,
                replaceable: false,
            }
        }
    }

    #[async_trait]
    impl Extension for TestExtension {
        fn name(&self) -> &str {
            &self.name
        }
        fn replaceable(&self) -> bool {
            self.replaceable
        }
        fn registration(&self) -> &ExtensionRegistration {
            &self.registration
        }
        fn set_registration(&mut self, registration: ExtensionRegistration) {
            self.registration = registration;
        }
        async fn init(&mut self, ctx: &mut ExtensionInitContext<'_>) -> Result<(), String> {
            (self.setup)(ctx)
        }
        async fn execute_tool(
            &self,
            name: &str,
            _args: Value,
            _ctx: &ExtensionToolContext,
        ) -> Result<ToolOutcome, String> {
            Ok(ToolOutcome::success(json!({"name": name}), json!({})))
        }
    }

    fn factory<F>(name: &str, setup: F) -> ExtensionFactory
    where
        F: Fn(&mut ExtensionInitContext<'_>) -> Result<(), String> + Send + Sync + 'static,
    {
        let name = Arc::new(name.to_string());
        let setup: ExtensionSetup = Arc::new(setup);
        Arc::new(move || {
            let name = (*name).clone();
            Ok(Box::new(TestExtension::new(&name, Arc::clone(&setup))))
        })
    }

    fn runner() -> ExtensionRunner {
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(Arc::clone(&bus)));
        ExtensionRunner::new(Vec::new(), host, bus, "/work")
    }

    #[tokio::test]
    async fn transaction_commits_successful_extension() {
        let runner = runner();
        let result = runner
            .load_extensions(&[factory("ok", |ctx| {
                ctx.register_flag(super::super::types::FlagDefinition {
                    name: "feature".into(),
                    description: None,
                    kind: super::super::types::FlagKind::Boolean,
                    default: Some(FlagValue::Bool(true)),
                    extension_path: ctx.registration.path.clone(),
                })
            })])
            .await;
        assert_eq!(result.extensions.len(), 1);
        assert!(result.errors.is_empty());
        assert_eq!(runner.host.get_flag("feature"), Some(FlagValue::Bool(true)));
        assert_eq!(runner.host.phase(), super::super::types::LoadPhase::Active);
    }

    #[tokio::test]
    async fn failed_init_rolls_back_flags_and_listeners() {
        let runner = runner();
        let failing = factory("bad", |ctx| {
            ctx.register_flag(super::super::types::FlagDefinition {
                name: "ghost".into(),
                description: None,
                kind: super::super::types::FlagKind::Boolean,
                default: Some(FlagValue::Bool(true)),
                extension_path: ctx.registration.path.clone(),
            })?;
            ctx.on(
                ExtensionEvent::SessionStart,
                super::super::types::constant_handler(json!({})),
            )?;
            Err("init failed".to_string())
        });
        let result = runner.load_extensions(&[failing]).await;
        assert!(result.extensions.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(runner.host.get_flag("ghost"), None);
        assert_eq!(runner.bus.listener_count(), 0);
        assert_eq!(runner.host.phase(), super::super::types::LoadPhase::Failed);
    }

    #[tokio::test]
    async fn first_tool_registration_wins() {
        let runner = runner();
        let first = factory("first", |ctx| {
            ctx.register_tool(ToolDefinition::new("shared", json!({"type":"object"})))
        });
        let second = factory("second", |ctx| {
            ctx.register_tool(ToolDefinition::new("shared", json!({"type":"object"})))
        });
        runner.load_extensions(&[first, second]).await;
        let tools = runner.get_all_registered_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "shared");
    }

    #[tokio::test]
    async fn invalid_tool_schema_is_rejected() {
        let runner = runner();
        let result = runner
            .load_extensions(&[factory("bad", |ctx| {
                ctx.register_tool(ToolDefinition::new("bad", json!([])))
            })])
            .await;
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].error.contains("object parameter schema"));
    }

    #[tokio::test]
    async fn duplicate_commands_are_disambiguated() {
        let runner = runner();
        let make = |name: &'static str| {
            factory(name, move |ctx| {
                ctx.register_command(super::command(
                    "dupe",
                    Arc::new(|_, _| Box::pin(async { Ok(()) })),
                ))
            })
        };
        runner
            .load_extensions(&[make("a"), make("b"), make("c")])
            .await;
        let commands = runner.get_registered_commands();
        let names: Vec<_> = commands
            .iter()
            .map(|command| command.invocation_name.as_str())
            .collect();
        assert_eq!(names, vec!["dupe:1", "dupe:2", "dupe:3"]);
        assert!(runner.get_command("dupe:2").is_some());
    }

    #[tokio::test]
    async fn reserved_shortcuts_are_skipped_with_diagnostics() {
        let runner = runner();
        runner
            .load_extensions(&[factory("shortcuts", |ctx| {
                ctx.register_shortcut(super::shortcut("Ctrl+O", "shortcuts"))?;
                ctx.register_shortcut(super::shortcut("Ctrl+K", "shortcuts"))
            })])
            .await;
        let mut resolved = IndexMap::new();
        resolved.insert("app.model.select".to_string(), vec!["ctrl+o".to_string()]);
        resolved.insert(
            "app.custom.noneReserved".to_string(),
            vec!["ctrl+k".to_string()],
        );
        let shortcuts = runner.get_shortcuts(&resolved);
        assert!(!shortcuts.contains_key("ctrl+o"));
        assert!(shortcuts.contains_key("ctrl+k"));
        assert_eq!(runner.shortcut_diagnostics().len(), 2);
        assert!(
            runner.shortcut_diagnostics()[0]
                .message
                .contains("Skipping")
        );
    }

    #[tokio::test]
    async fn two_extensions_claiming_a_shortcut_warn() {
        let runner = runner();
        let make = |name: &'static str| {
            factory(name, move |ctx| {
                let path = ctx.registration.path.clone();
                ctx.register_shortcut(super::shortcut("ctrl+j", path))
            })
        };
        runner.load_extensions(&[make("a"), make("b")]).await;
        let shortcuts = runner.get_shortcuts(&IndexMap::new());
        assert_eq!(shortcuts["ctrl+j"].extension_path, "b");
        assert!(
            runner.shortcut_diagnostics()[0]
                .message
                .contains("registered by both")
        );
    }

    #[tokio::test]
    async fn project_trust_continues_past_undecided() {
        let runner = runner();
        for decision in ["undecided", "no", "yes"] {
            let decision = decision.to_string();
            runner.bus.on(ExtensionEvent::ProjectTrust, move |_| {
                let decision = decision.clone();
                Box::pin(async move { Ok(Some(json!({ "trusted": decision }))) })
            });
        }
        let result = runner.emit_project_trust(json!({"cwd":"/work"})).await;
        assert_eq!(result, Some(json!({ "trusted": "no" })));
    }

    #[tokio::test]
    async fn tool_call_blocks_on_first_blocker() {
        let runner = runner();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        runner.bus.on(ExtensionEvent::ToolCall, move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Some(json!({ "block": false })))
            })
        });
        runner.bus.on(ExtensionEvent::ToolCall, |_| {
            Box::pin(async { Ok(Some(json!({ "block": true, "reason": "no" }))) })
        });
        let last_calls = Arc::new(AtomicUsize::new(0));
        let last_counter = Arc::clone(&last_calls);
        runner.bus.on(ExtensionEvent::ToolCall, move |_| {
            let counter = Arc::clone(&last_counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Some(json!({ "block": false })))
            })
        });
        let result = runner.emit_tool_call(json!({"toolName":"bash"})).await;
        assert_eq!(result, Some(json!({ "block": true, "reason": "no" })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(last_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn input_transforms_chain_and_handled_short_circuits() {
        let runner = runner();
        runner.bus.on(ExtensionEvent::Input, |event| {
            Box::pin(async move {
                let text = event["text"].as_str().unwrap_or("");
                Ok(Some(
                    json!({ "action": "transform", "text": format!("{text}-a") }),
                ))
            })
        });
        runner.bus.on(ExtensionEvent::Input, |event| {
            Box::pin(async move {
                let text = event["text"].as_str().unwrap_or("");
                if text.ends_with("-a") {
                    let transformed = format!("{text}-b");
                    Ok(Some(json!({ "action": "transform", "text": transformed })))
                } else {
                    Ok(Some(json!({ "action": "continue" })))
                }
            })
        });
        let result = runner.emit_input("hi", None, "interactive", None).await;
        assert_eq!(result["action"], "transform");
        assert_eq!(result["text"], "hi-a-b");

        runner.bus.on(ExtensionEvent::Input, |_| {
            Box::pin(async { Ok(Some(json!({ "action": "handled" }))) })
        });
        let result = runner.emit_input("hi", None, "interactive", None).await;
        assert_eq!(result, json!({ "action": "handled" }));
    }

    #[tokio::test]
    async fn message_end_preserves_role() {
        let runner = runner();
        runner.bus.on(ExtensionEvent::MessageEnd, |_| {
            Box::pin(async {
                Ok(Some(
                    json!({ "message": { "role": "assistant", "text": "v2" } }),
                ))
            })
        });
        runner.bus.on(ExtensionEvent::MessageEnd, |_| {
            Box::pin(async {
                Ok(Some(
                    json!({ "message": { "role": "user", "text": "bad" } }),
                ))
            })
        });
        let result = runner
            .emit_message_end(json!({"role":"assistant","text":"v1"}))
            .await;
        assert_eq!(result, Some(json!({ "role": "assistant", "text": "v2" })));
        assert_eq!(runner.host.stale_message(), None);
    }

    #[tokio::test]
    async fn tool_result_replacing_content_drops_structured_content() {
        let runner = runner();
        runner.bus.on(ExtensionEvent::ToolResult, |_| {
            Box::pin(async {
                Ok(Some(
                    json!({ "content": [{"type":"text","text":"redacted"}] }),
                ))
            })
        });
        let result = runner
            .emit_tool_result(json!({
                "content": [{"type":"text","text":"secret"}],
                "structuredContent": {"password":"secret"},
                "isError": false
            }))
            .await;
        let result = result.expect("modified");
        assert!(result.get("structuredContent").is_none());
        assert_eq!(result["content"][0]["text"], "redacted");
    }

    #[tokio::test]
    async fn tool_result_keeps_structured_content_when_replaced() {
        let runner = runner();
        runner.bus.on(ExtensionEvent::ToolResult, |_| {
            Box::pin(async {
                Ok(Some(json!({
                    "content": [{"type":"text","text":"v2"}],
                    "structuredContent": {"ok": true}
                })))
            })
        });
        let result = runner
            .emit_tool_result(json!({"content": [], "structuredContent": {"old": true}}))
            .await
            .expect("modified");
        assert_eq!(result["structuredContent"], json!({"ok": true}));
    }

    #[tokio::test]
    async fn cache_warming_last_override_wins() {
        let runner = runner();
        for action in ["warm", "cancel"] {
            runner
                .bus
                .on(ExtensionEvent::CacheWarmingDecision, move |_| {
                    let action = action;
                    Box::pin(async move { Ok(Some(json!({ "action": action }))) })
                });
        }
        let result = runner
            .emit_cache_warming_decision(json!({"action":"skip"}))
            .await;
        assert_eq!(result["action"], "cancel");
    }

    #[tokio::test]
    async fn before_provider_request_chains() {
        let runner = runner();
        runner
            .bus
            .on(ExtensionEvent::BeforeProviderRequest, |event| {
                Box::pin(async move {
                    let payload = event["payload"].as_str().unwrap_or("");
                    Ok(Some(json!(format!("{payload}-1"))))
                })
            });
        runner
            .bus
            .on(ExtensionEvent::BeforeProviderRequest, |event| {
                Box::pin(async move {
                    let payload = event["payload"].as_str().unwrap_or("");
                    Ok(Some(json!(format!("{payload}-2"))))
                })
            });
        assert_eq!(
            runner.emit_before_provider_request(json!("base")).await,
            json!("base-1-2")
        );
    }

    #[tokio::test]
    async fn mcp_servers_require_single_owner_and_unique_namespace() {
        let runner = runner();
        assert!(
            runner
                .host
                .register_mcp_server(McpServerConfig {
                    name: "docs_api".into(),
                    config: json!({}),
                    extension_path: "a".into(),
                })
                .is_ok()
        );
        let other_owner = runner.host.register_mcp_server(McpServerConfig {
            name: "docs_api".into(),
            config: json!({}),
            extension_path: "b".into(),
        });
        assert!(other_owner.is_err());
        let clash = runner.host.register_mcp_server(McpServerConfig {
            name: "docs-api".into(),
            config: json!({}),
            extension_path: "a".into(),
        });
        assert!(clash.is_err());
    }

    #[tokio::test]
    async fn unhandled_mcp_servers_are_reported_once() {
        let runner = runner();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        runner.host.on_error(Arc::new(move |error| {
            sink.lock().expect("errors poisoned").push(error)
        }));
        runner
            .host
            .register_mcp_server(McpServerConfig {
                name: "orphan".into(),
                config: json!({}),
                extension_path: "a".into(),
            })
            .expect("register");
        runner.report_unhandled_mcp_servers();
        runner.report_unhandled_mcp_servers();
        assert_eq!(errors.lock().expect("errors poisoned").len(), 1);
    }

    #[tokio::test]
    async fn invalidate_makes_all_extensions_stale() {
        let runner = runner();
        runner.load_extensions(&[factory("a", |_| Ok(()))]).await;
        assert!(runner.host.assert_active().is_ok());
        runner.invalidate();
        assert!(runner.host.assert_active().is_err());
        assert!(
            runner
                .host
                .stale_message()
                .is_some_and(|message| message.contains("stale"))
        );
    }

    #[tokio::test]
    async fn resources_discover_collects_paths() {
        let runner = runner();
        runner.bus.on(ExtensionEvent::ResourcesDiscover, |_| {
            Box::pin(async { Ok(Some(json!({ "skillPaths": ["/a/skills"] }))) })
        });
        let discovered = runner.emit_resources_discover("reload").await;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0]["skillPaths"][0], "/a/skills");
    }
}
