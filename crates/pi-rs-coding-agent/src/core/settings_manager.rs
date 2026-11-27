//! Port of `packages/coding-agent/src/core/settings-manager.ts` (JSON settings
//! storage, deep merge, legacy migrations, and error tracking).
//!
//! Upstream exposes dozens of typed accessors. This module keeps the data model
//! generic (`serde_json::Value`) so callers can read or write any key while the
//! accessors that carry non-trivial behavior (defaults, path normalization,
//! tool modifiers) are provided explicitly.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::{CONFIG_DIR_NAME, get_settings_path};
use crate::lockfile::{LockOptions, lock_sync};
use crate::utils::paths::normalize_path;
use crate::utils::text::strip_bom;

/// Settings layer written by a `set_value` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsScope {
    Global,
    Project,
}

impl SettingsScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

/// Options accepted when creating a manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOptions {
    pub project_trusted: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            project_trusted: true,
        }
    }
}

/// A load or write problem, with the scope and file path when applicable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<PathBuf>,
    pub error: String,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(
                formatter,
                "{} settings ({}): {}",
                self.scope.as_str(),
                path.display(),
                self.error
            ),
            None => write!(
                formatter,
                "{} settings: {}",
                self.scope.as_str(),
                self.error
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

/// Tools enabled at startup when no plain `defaultTools` list is configured.
pub const DEFAULT_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write"];
/// Upstream `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (five minutes).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: i64 = 300_000;

/// Defaults applied by [`SettingsManager::get`] for keys that are absent.
pub fn default_settings() -> Value {
    serde_json::json!({
        "transport": "auto",
        "sessionShareDestination": "auto",
        "steeringMode": "one-at-a-time",
        "followUpMode": "one-at-a-time",
        "compaction": {
            "enabled": true,
            "reserveTokens": 16384,
            "keepRecentTokens": 20000
        },
        "branchSummary": {
            "reserveTokens": 16384,
            "skipPrompt": false
        },
        "retry": {
            "enabled": true,
            "maxRetries": 3,
            "baseDelayMs": 2000,
            "maxAgentDelayMs": 60000,
            "provider": { "maxRetryDelayMs": 60000 }
        },
        "httpIdleTimeoutMs": DEFAULT_HTTP_IDLE_TIMEOUT_MS,
        "cacheWarming": "idle",
        "hideThinkingBlock": false,
        "showCacheMissNotices": false,
        "quietStartup": false,
        "defaultProjectTrust": "ask",
        "collapseChangelog": false,
        "enableInstallTelemetry": true,
        "enableAnalytics": false,
        "enableSkillCommands": true,
        "terminal": {
            "showImages": true,
            "imageWidthCells": 60,
            "clearOnShrink": false,
            "showTerminalProgress": false
        },
        "images": { "autoResize": true, "blockImages": false },
        "doubleEscapeAction": "tree",
        "treeFilterMode": "default",
        "editorPaddingX": 0,
        "outputPad": 1,
        "autocompleteMaxVisible": 5,
        "markdown": { "codeBlockIndent": "  ", "mermaid": "streaming" },
        "warnings": { "anthropicExtraUsage": true },
        "codemode": { "mode": "on", "inlineBudget": 3000 },
        "tuiMode": "fullscreen",
        "fullscreenExitOutput": "transcript",
        "fullscreenScrollbar": "auto",
        "fullscreenCopyOnSelect": true,
        "fullscreenWheelScrollLines": "auto",
        "theme": null,
        "shellCommandPrefix": null,
        "defaultTools": ["read", "bash", "edit", "write"],
        "sessionDir": null,
        "shellPath": null
    })
}

/// True when a tool list entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// Apply `+name` and `-name` entries to `base` in order.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries {
        if !is_tool_modifier(entry) {
            continue;
        }
        let name = &entry[1..];
        if let Some(index) = tools.iter().position(|tool| tool == name) {
            if entry.starts_with('-') {
                tools.remove(index);
            }
        } else if entry.starts_with('+') && !name.is_empty() {
            tools.push(name.to_string());
        }
    }
    tools
}

/// Merge two `defaultTools` layers. A plain list replaces the inherited one; a
/// list consisting only of modifiers extends it.
fn merge_default_tools(base: Option<&Value>, overrides: Option<&Value>) -> Option<Value> {
    let overrides = overrides?;
    match (base, overrides) {
        (Some(Value::Array(base_items)), Value::Array(override_items))
            if override_items
                .iter()
                .all(|item| item.as_str().is_some_and(is_tool_modifier)) =>
        {
            let mut merged = base_items.clone();
            merged.extend(override_items.iter().cloned());
            Some(Value::Array(merged))
        }
        _ => Some(overrides.clone()),
    }
}

fn as_object(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Recursive object merge; project values take precedence over global ones.
/// `null` is a real override, while nested objects are merged per key.
pub fn deep_merge_objects(
    base: &Map<String, Value>,
    overrides: &Map<String, Value>,
) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, override_value) in overrides {
        if let (Some(base_value), Some(override_object)) =
            (result.get(key), as_object(override_value))
            && let Some(base_object) = as_object(base_value)
        {
            result.insert(
                key.clone(),
                Value::Object(deep_merge_objects(base_object, override_object)),
            );
            continue;
        }
        result.insert(key.clone(), override_value.clone());
    }
    result
}

/// Deep merge two settings values, special-casing `defaultTools`.
pub fn deep_merge_settings(base: &Value, overrides: &Value) -> Value {
    let base_map = as_object(base).cloned().unwrap_or_default();
    let override_map = as_object(overrides).cloned().unwrap_or_default();
    let mut merged = deep_merge_objects(&base_map, &override_map);
    if let Some(tools) = merge_default_tools(
        base_map.get("defaultTools"),
        override_map.get("defaultTools"),
    ) {
        merged.insert("defaultTools".to_string(), tools);
    }
    Value::Object(merged)
}

/// Fill keys that are absent from `target` with their defaults. Null defaults
/// are intentionally not injected so absent optional keys stay absent.
fn fill_defaults(target: &mut Map<String, Value>, defaults: &Map<String, Value>) {
    for (key, default) in defaults {
        if default.is_null() {
            continue;
        }
        match (target.get_mut(key), as_object(default)) {
            (Some(Value::Object(existing)), Some(default_object)) => {
                fill_defaults(existing, default_object)
            }
            (None, _) => {
                target.insert(key.clone(), default.clone());
            }
            _ => {}
        }
    }
}

/// Apply the legacy migrations performed by upstream when settings are read.
pub fn migrate_settings(settings: &mut Map<String, Value>) {
    // queueMode -> steeringMode
    if !settings.contains_key("steeringMode")
        && let Some(queue_mode) = settings.shift_remove("queueMode")
    {
        settings.insert("steeringMode".to_string(), queue_mode);
    }

    // Legacy websockets boolean -> transport enum.
    if !settings.contains_key("transport")
        && let Some(Value::Bool(websockets)) = settings.get("websockets")
    {
        let transport = if *websockets { "websocket" } else { "sse" };
        settings.insert(
            "transport".to_string(),
            Value::String(transport.to_string()),
        );
        settings.shift_remove("websockets");
    }

    // Old skills object -> enableSkillCommands plus a customDirectories array.
    if let Some(Value::Object(skills)) = settings.get("skills").cloned() {
        if let Some(enabled) = skills.get("enableSkillCommands")
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".to_string(), enabled.clone());
        }
        match skills.get("customDirectories") {
            Some(Value::Array(directories)) if !directories.is_empty() => {
                settings.insert("skills".to_string(), Value::Array(directories.clone()));
            }
            _ => {
                settings.shift_remove("skills");
            }
        }
    }

    // retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    if let Some(Value::Object(retry)) = settings.get_mut("retry") {
        let max_delay = retry.get("maxDelayMs").cloned();
        if let Some(max_delay) = max_delay {
            let mut provider = retry
                .get("provider")
                .and_then(as_object)
                .cloned()
                .unwrap_or_default();
            if provider
                .get("maxRetryDelayMs")
                .is_none_or(|value| value.is_null())
            {
                provider.insert("maxRetryDelayMs".to_string(), max_delay);
                retry.insert("provider".to_string(), Value::Object(provider));
            }
            retry.shift_remove("maxDelayMs");
        }
    }
}

/// Parse a JSON object, stripping a leading BOM like upstream's `stripBom`.
fn parse_settings(content: Option<&str>) -> Result<Map<String, Value>, String> {
    let Some(content) = content else {
        return Ok(Map::new());
    };
    let value: Value = serde_json::from_str(strip_bom(content))
        .map_err(|error| format!("Invalid settings JSON: {error}"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("Invalid settings JSON: expected an object".to_string()),
    }
}

/// Storage abstraction behind [`SettingsManager`]. File writes are serialized
/// with the same directory lock upstream takes through `proper-lockfile`.
enum SettingsStorage {
    File {
        global: PathBuf,
        project: PathBuf,
    },
    InMemory {
        global: Option<String>,
        project: Option<String>,
    },
}

impl SettingsStorage {
    fn path(&self, scope: SettingsScope) -> Option<&Path> {
        match self {
            Self::File { global, project } => Some(match scope {
                SettingsScope::Global => global,
                SettingsScope::Project => project,
            }),
            Self::InMemory { .. } => None,
        }
    }

    /// Read without taking a lock. Upstream never creates `.pi/` while only
    /// reading project settings, so this path has no filesystem side effects.
    fn read_only(&self, scope: SettingsScope) -> Result<Option<String>, String> {
        match self {
            Self::File { global, project } => {
                let path = match scope {
                    SettingsScope::Global => global,
                    SettingsScope::Project => project,
                };
                match std::fs::read_to_string(path) {
                    Ok(content) => Ok(Some(content)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.to_string()),
                }
            }
            Self::InMemory { global, project } => Ok(match scope {
                SettingsScope::Global => global,
                SettingsScope::Project => project,
            }
            .clone()),
        }
    }

    /// Read-modify-write under a lock. `None` for `next` leaves the file alone.
    fn with_lock<T>(
        &mut self,
        scope: SettingsScope,
        callback: impl FnOnce(Option<&str>) -> Result<(T, Option<String>), String>,
    ) -> Result<T, String> {
        let path = self.path(scope).map(Path::to_path_buf);
        if let Some(path) = &path
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let _guard = match &path {
            Some(path) => Some(lock_sync(
                path,
                &LockOptions {
                    realpath: false,
                    retries: 10,
                    ..LockOptions::default()
                },
            )?),
            None => None,
        };
        let current = self.read_only(scope)?;
        let (result, next) = callback(current.as_deref())?;
        if let Some(next) = next {
            match self {
                Self::File { global, project } => {
                    let target = match scope {
                        SettingsScope::Global => global,
                        SettingsScope::Project => project,
                    };
                    std::fs::write(target, next).map_err(|error| error.to_string())?;
                }
                Self::InMemory { global, project } => {
                    let slot = match scope {
                        SettingsScope::Global => global,
                        SettingsScope::Project => project,
                    };
                    *slot = Some(next);
                }
            }
        }
        Ok(result)
    }
}

/// Read the effective settings from global and project files.
pub struct SettingsManager {
    storage: SettingsStorage,
    global: Map<String, Value>,
    project: Map<String, Value>,
    overrides: Map<String, Value>,
    effective: Value,
    project_trusted: bool,
    errors: Vec<SettingsError>,
    global_load_error: Option<String>,
    project_load_error: Option<String>,
}

impl SettingsManager {
    /// Create a manager using `~/.pi/agent/settings.json` and
    /// `<cwd>/.pi/settings.json`.
    pub fn create(cwd: &Path) -> Result<Self, String> {
        Ok(Self::from_storage(
            SettingsStorage::File {
                global: get_settings_path()?,
                project: cwd.join(CONFIG_DIR_NAME).join("settings.json"),
            },
            CreateOptions::default(),
        ))
    }

    /// Create a manager with explicit global and project root directories.
    pub fn create_with_dirs(cwd: &Path, agent_dir: &Path, options: CreateOptions) -> Self {
        let storage = SettingsStorage::File {
            global: agent_dir.join("settings.json"),
            project: cwd.join(CONFIG_DIR_NAME).join("settings.json"),
        };
        Self::from_storage(storage, options)
    }

    /// Create a manager backed only by memory, used by callers without a cwd.
    pub fn in_memory(settings: Value, options: CreateOptions) -> Self {
        let storage = SettingsStorage::InMemory {
            global: Some(serde_json::to_string(&settings).unwrap_or_else(|_| "{}".to_string())),
            project: None,
        };
        Self::from_storage(storage, options)
    }

    fn from_storage(storage: SettingsStorage, options: CreateOptions) -> Self {
        let mut manager = Self {
            storage,
            global: Map::new(),
            project: Map::new(),
            overrides: Map::new(),
            effective: Value::Object(Map::new()),
            project_trusted: options.project_trusted,
            errors: Vec::new(),
            global_load_error: None,
            project_load_error: None,
        };
        manager.load_scope(SettingsScope::Global);
        if options.project_trusted {
            manager.load_scope(SettingsScope::Project);
        }
        manager.recompute();
        manager
    }

    fn load_scope(&mut self, scope: SettingsScope) {
        let result = self
            .storage
            .read_only(scope)
            .and_then(|content| parse_settings(content.as_deref()))
            .map(|mut settings| {
                migrate_settings(&mut settings);
                settings
            });
        match result {
            Ok(settings) => match scope {
                SettingsScope::Global => {
                    self.global = settings;
                    self.global_load_error = None;
                }
                SettingsScope::Project => {
                    self.project = settings;
                    self.project_load_error = None;
                }
            },
            Err(error) => {
                match scope {
                    SettingsScope::Global => self.global_load_error = Some(error.clone()),
                    SettingsScope::Project => self.project_load_error = Some(error.clone()),
                }
                self.record_error(scope, error);
            }
        }
    }

    fn record_error(&mut self, scope: SettingsScope, error: String) {
        let path = self.storage.path(scope).map(Path::to_path_buf);
        self.errors.push(SettingsError { scope, path, error });
    }

    fn recompute(&mut self) {
        let merged = deep_merge_settings(
            &Value::Object(self.global.clone()),
            &Value::Object(self.project.clone()),
        );
        let merged = deep_merge_settings(&merged, &Value::Object(self.overrides.clone()));
        self.effective = merged;
    }

    /// Effective settings with defaults filled in for absent keys.
    pub fn get(&self) -> Value {
        let mut value = self.effective.clone();
        if let Some(map) = as_object_mut(&mut value) {
            let defaults = default_settings();
            if let Some(defaults) = as_object(&defaults) {
                fill_defaults(map, defaults);
            }
        }
        value
    }

    /// Merged global/project/override settings without injected defaults.
    pub fn get_settings(&self) -> Value {
        self.effective.clone()
    }

    pub fn get_global_settings(&self) -> Value {
        Value::Object(self.global.clone())
    }

    pub fn get_project_settings(&self) -> Value {
        Value::Object(self.project.clone())
    }

    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn errors(&self) -> &[SettingsError] {
        &self.errors
    }

    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        if !trusted {
            self.project = Map::new();
            self.project_load_error = None;
        } else {
            self.load_scope(SettingsScope::Project);
        }
        self.recompute();
    }

    /// Reload both scopes from disk, keeping the last good value on error.
    pub fn reload(&mut self) {
        self.load_scope(SettingsScope::Global);
        if self.project_trusted {
            self.load_scope(SettingsScope::Project);
        }
        self.recompute();
    }

    /// Merge runtime overrides on top of the stored settings (not persisted).
    pub fn apply_overrides(&mut self, overrides: Value) {
        self.overrides = deep_merge_objects(
            &self.overrides,
            &as_object(&overrides).cloned().unwrap_or_default(),
        );
        self.recompute();
    }

    /// Update a dotted/nested path in the global scope and persist it atomically.
    pub fn set_value(&mut self, path: &[String], value: Value) -> Result<(), String> {
        self.set_value_scoped(SettingsScope::Global, path, value)
    }

    /// Update a nested path in the project scope. Requires project trust.
    pub fn set_project_value(&mut self, path: &[String], value: Value) -> Result<(), String> {
        if !self.project_trusted {
            return Err("Project is not trusted; refusing to write project settings".to_string());
        }
        self.set_value_scoped(SettingsScope::Project, path, value)
    }

    /// Update a nested path in the chosen scope under the file lock.
    pub fn set_value_scoped(
        &mut self,
        scope: SettingsScope,
        path: &[String],
        value: Value,
    ) -> Result<(), String> {
        if scope == SettingsScope::Project && !self.project_trusted {
            return Err("Project is not trusted; refusing to write project settings".to_string());
        }
        // A global file that failed to parse must never be overwritten.
        if scope == SettingsScope::Global && self.global_load_error.is_some() {
            return Err(self
                .global_load_error
                .clone()
                .unwrap_or_else(|| "invalid global settings".to_string()));
        }
        if scope == SettingsScope::Project && self.project_load_error.is_some() {
            return Err(self
                .project_load_error
                .clone()
                .unwrap_or_else(|| "invalid project settings".to_string()));
        }

        let result = self.storage.with_lock(scope, |current| {
            let mut settings = parse_settings(current)?;
            set_nested_value(&mut settings, path, value.clone())?;
            let next = serde_json::to_string_pretty(&Value::Object(settings))
                .map_err(|error| error.to_string())?;
            Ok(((), Some(next)))
        });
        if let Err(error) = result {
            self.record_error(scope, error.clone());
            return Err(error);
        }
        // Refresh the in-memory snapshot from the file we just wrote.
        self.load_scope(scope);
        self.recompute();
        Ok(())
    }

    /// Writes are synchronous; retained for parity with upstream callers.
    pub fn flush(&self) {}

    fn raw_get(&self, key: &str) -> Option<&Value> {
        as_object(&self.effective).and_then(|map| map.get(key))
    }

    pub fn get_theme_setting(&self) -> Option<String> {
        self.get()
            .get("theme")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The fixed theme name, or `None` for automatic slash-separated themes.
    pub fn get_theme(&self) -> Option<String> {
        self.get_theme_setting()
            .filter(|theme| !theme.contains('/'))
    }

    pub fn get_output_pad(&self) -> i64 {
        if self.raw_get("outputPad").and_then(Value::as_i64) == Some(0) {
            0
        } else {
            1
        }
    }

    pub fn get_mermaid_rendering_mode(&self) -> &'static str {
        match self
            .get()
            .get("markdown")
            .and_then(|value| value.get("mermaid"))
            .and_then(Value::as_str)
        {
            Some("off") => "off",
            Some("final") => "final",
            _ => "streaming",
        }
    }

    pub fn get_shell_command_prefix(&self) -> Option<String> {
        self.raw_get("shellCommandPrefix")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn get_default_provider(&self) -> Option<String> {
        self.raw_get("defaultProvider")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn get_default_model(&self) -> Option<String> {
        self.raw_get("defaultModel")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn get_session_dir(&self) -> Option<String> {
        self.raw_get("sessionDir")
            .and_then(Value::as_str)
            .map(normalize_path)
    }

    pub fn get_shell_path(&self) -> Option<String> {
        self.raw_get("shellPath")
            .and_then(Value::as_str)
            .map(normalize_path)
    }

    /// Resolved `defaultTools`: plain names replace the built-in list, then
    /// `+name`/`-name` entries modify it in order.
    pub fn get_default_tools(&self) -> Option<Vec<String>> {
        let tools = match self.raw_get("defaultTools") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>(),
            Some(_) => Vec::new(),
            None => return None,
        };
        let has_plain = tools.iter().any(|entry| !is_tool_modifier(entry));
        let base = if has_plain || tools.is_empty() {
            tools.clone()
        } else {
            DEFAULT_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        };
        Some(apply_tool_modifiers(&base, &tools))
    }

    /// Cache warming is read from global settings only: warming costs money.
    pub fn get_cache_warming_mode(&self) -> &'static str {
        match self.global.get("cacheWarming").and_then(Value::as_str) {
            Some("off") => "off",
            Some("streaming") => "streaming",
            Some("idle") => "idle",
            _ => "idle",
        }
    }

    /// Parse a numeric or `"disabled"` timeout, rejecting invalid values.
    pub fn get_http_idle_timeout_ms(&self) -> Result<i64, String> {
        match self.get().get("httpIdleTimeoutMs") {
            None => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            Some(value) => parse_timeout_setting(value, "httpIdleTimeoutMs"),
        }
    }

    pub fn get_branch_summary_settings(&self) -> (usize, bool) {
        let effective = self.get();
        let branch = &effective["branchSummary"];
        let reserve_tokens = branch["reserveTokens"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(16_384);
        let skip_prompt = branch["skipPrompt"].as_bool().unwrap_or(false);
        (reserve_tokens, skip_prompt)
    }

    pub fn get_branch_summary_skip_prompt(&self) -> bool {
        self.get_branch_summary_settings().1
    }

    pub fn get_compaction_settings_for_model(
        &self,
        provider: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<(bool, usize, usize), String> {
        let effective = self.get();
        let compaction = effective.get("compaction");
        let enabled = compaction
            .and_then(|c| c.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let reserve = resolve_compaction_token_setting(
            compaction,
            "reserveTokens",
            provider,
            model_id,
            16_384,
        )?;
        let keep_recent = resolve_compaction_token_setting(
            compaction,
            "keepRecentTokens",
            provider,
            model_id,
            20_000,
        )?;
        Ok((enabled, reserve, keep_recent))
    }

    pub fn get_provider_retry_settings(&self) -> (Option<u64>, Option<u32>, u64) {
        let effective = self.get();
        let provider = &effective["retry"]["provider"];
        let timeout_ms = provider.get("timeoutMs").and_then(Value::as_u64);
        let max_retries = provider
            .get("maxRetries")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        let max_retry_delay_ms = provider
            .get("maxRetryDelayMs")
            .and_then(Value::as_u64)
            .unwrap_or(60_000);
        (timeout_ms, max_retries, max_retry_delay_ms)
    }

    pub fn get_websocket_connect_timeout_ms(&self) -> Result<Option<u64>, String> {
        let Some(value) = self.raw_get("websocketConnectTimeoutMs") else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let parsed = parse_timeout_setting(value, "websocketConnectTimeoutMs")?;
        Ok(u64::try_from(parsed).ok())
    }

    pub fn get_external_editor_command(&self) -> String {
        if let Some(configured) = self
            .get()
            .get("externalEditor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return configured.to_owned();
        }
        for variable in ["VISUAL", "EDITOR"] {
            if let Ok(value) = std::env::var(variable)
                && !value.trim().is_empty()
            {
                return value;
            }
        }
        if cfg!(windows) {
            "notepad".to_owned()
        } else {
            "nano".to_owned()
        }
    }

    pub fn get_thinking_budgets(&self) -> indexmap::IndexMap<pi_rs_agent::ThinkingLevel, u64> {
        let mut budgets = indexmap::IndexMap::new();
        let binding = self.get();
        let Some(map) = binding.get("thinkingBudgets").and_then(Value::as_object) else {
            return budgets;
        };
        for (level, value) in map {
            let Some(tokens) = value.as_u64() else {
                continue;
            };
            let parsed = match level.as_str() {
                "minimal" => Some(pi_rs_agent::ThinkingLevel::Minimal),
                "low" => Some(pi_rs_agent::ThinkingLevel::Low),
                "medium" => Some(pi_rs_agent::ThinkingLevel::Medium),
                "high" => Some(pi_rs_agent::ThinkingLevel::High),
                "xhigh" => Some(pi_rs_agent::ThinkingLevel::Xhigh),
                _ => None,
            };
            if let Some(level) = parsed {
                budgets.insert(level, tokens);
            }
        }
        budgets
    }

    pub fn get_collapse_changelog(&self) -> bool {
        self.get()
            .get("collapseChangelog")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn get_codemode_mode(&self) -> crate::extensions::codemode::CodemodeMode {
        match self.get()["codemode"]["mode"].as_str() {
            Some("only") => crate::extensions::codemode::CodemodeMode::Only,
            _ => crate::extensions::codemode::CodemodeMode::On,
        }
    }

    pub fn get_codemode_inline_budget(&self) -> usize {
        self.get()["codemode"]["inlineBudget"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(3_000)
    }

    pub fn get_show_hardware_cursor(&self) -> bool {
        show_hardware_cursor_from(&self.get())
    }

    pub fn get_show_images(&self) -> bool {
        show_images_from(&self.get())
    }

    pub fn get_terminal_capability_overrides(
        &self,
    ) -> pi_rs_tui::terminal_image::CapabilityOverrides {
        terminal_capability_overrides_from(&self.get())
    }
}

pub fn show_images_from(settings: &Value) -> bool {
    let terminal = &settings["terminal"];
    if terminal["images"].as_bool() == Some(false)
        || matches!(
            terminal["images"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("none" | "off")
        )
    {
        return false;
    }
    terminal["showImages"].as_bool().unwrap_or(true)
}

pub fn terminal_capability_overrides_from(
    settings: &Value,
) -> pi_rs_tui::terminal_image::CapabilityOverrides {
    let terminal = &settings["terminal"];
    let images = match &terminal["images"] {
        Value::Bool(false) => Some(None),
        value => match value.as_str().map(str::to_ascii_lowercase).as_deref() {
            Some("none" | "off") => Some(None),
            Some("kitty") => Some(Some(pi_rs_tui::ImageProtocol::Kitty)),
            Some("iterm2") => Some(Some(pi_rs_tui::ImageProtocol::Iterm2)),
            _ => None,
        },
    };
    pi_rs_tui::terminal_image::CapabilityOverrides {
        images,
        true_color: terminal["trueColor"].as_bool(),
        hyperlinks: terminal["hyperlinks"].as_bool(),
    }
}

pub fn show_hardware_cursor_from(settings: &Value) -> bool {
    settings
        .get("showHardwareCursor")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| std::env::var("PI_HARDWARE_CURSOR").ok().as_deref() == Some("1"))
}

fn resolve_compaction_token_setting(
    compaction: Option<&Value>,
    field: &str,
    provider: Option<&str>,
    model_id: Option<&str>,
    default: usize,
) -> Result<usize, String> {
    let parse_non_neg_int = |value: &Value, label: &str| -> Result<usize, String> {
        value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                format!("Invalid {label} setting: {value}. Expected a non-negative safe integer.")
            })
    };
    let ordinary = match compaction
        .and_then(|c| c.get(field))
        .filter(|v| !v.is_null())
    {
        Some(value) => Some(parse_non_neg_int(value, &format!("compaction.{field}"))?),
        None => None,
    };
    let override_val = if let (Some(provider), Some(model_id)) = (provider, model_id)
        && let Some(overrides) = compaction
            .and_then(|c| c.get("modelOverrides"))
            .and_then(Value::as_object)
    {
        let model_key = format!("{provider}/{model_id}");
        if let Some(entry) = overrides.get(&model_key).filter(|v| !v.is_null()) {
            if !entry.is_object() {
                return Err(format!(
                    "Invalid compaction.modelOverrides[\"{model_key}\"] setting: {entry}. Expected an object."
                ));
            }
            match entry.get(field).filter(|v| !v.is_null()) {
                Some(value) => Some(parse_non_neg_int(
                    value,
                    &format!("compaction.modelOverrides[\"{model_key}\"].{field}"),
                )?),
                None => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    Ok(override_val.or(ordinary).unwrap_or(default))
}

fn as_object_mut(value: &mut Value) -> Option<&mut Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn parse_timeout_setting(value: &Value, name: &str) -> Result<i64, String> {
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            if text.trim().eq_ignore_ascii_case("disabled") {
                Some(0.0)
            } else {
                text.trim().parse::<f64>().ok()
            }
        }
        _ => None,
    };
    match parsed {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => Ok(seconds.floor() as i64),
        _ => Err(format!("Invalid {name} setting: {value}")),
    }
}

/// Set a value at `path`, creating intermediate objects as needed.
fn set_nested_value(
    settings: &mut Map<String, Value>,
    path: &[String],
    value: Value,
) -> Result<(), String> {
    if path.is_empty() {
        match value {
            Value::Object(map) => {
                *settings = map;
                Ok(())
            }
            _ => Err("Cannot replace settings root with a non-object".to_string()),
        }
    } else {
        let mut cursor = settings;
        for key in &path[..path.len() - 1] {
            let entry = cursor
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cursor = match entry {
                Value::Object(map) => map,
                _ => return Err(format!("Path segment {key} is not an object")),
            };
        }
        cursor.insert(path[path.len() - 1].clone(), value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(name: &str) -> Result<(tempfile::TempDir, PathBuf, PathBuf), String> {
        let root = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .map_err(|error| error.to_string())?;
        let agent = root.path().join("agent");
        let project = root.path().join("project");
        std::fs::create_dir_all(&agent).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(project.join(CONFIG_DIR_NAME))
            .map_err(|error| error.to_string())?;
        Ok((root, agent, project))
    }

    fn write(path: &Path, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|error| error.to_string())
    }

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn deep_merge_nested_objects_project_wins() {
        let base = serde_json::json!({"a": 1, "nested": {"x": 1, "y": 2}});
        let overrides = serde_json::json!({"nested": {"y": 3, "z": 4}});
        assert_eq!(
            deep_merge_settings(&base, &overrides),
            serde_json::json!({"a": 1, "nested": {"x": 1, "y": 3, "z": 4}})
        );
    }

    #[test]
    fn deep_merge_treats_null_as_an_override() {
        let base = serde_json::json!({"theme": "dark"});
        let overrides = serde_json::json!({"theme": null});
        assert_eq!(
            deep_merge_settings(&base, &overrides),
            serde_json::json!({"theme": null})
        );
    }

    #[test]
    fn default_tools_replaces_with_plain_names_and_appends_modifiers() {
        let base = serde_json::json!({"defaultTools": ["read", "bash"]});
        assert_eq!(
            deep_merge_settings(&base, &serde_json::json!({"defaultTools": ["grep"]})),
            serde_json::json!({"defaultTools": ["grep"]})
        );
        assert_eq!(
            deep_merge_settings(
                &base,
                &serde_json::json!({"defaultTools": ["+grep", "-bash"]})
            ),
            serde_json::json!({"defaultTools": ["read", "bash", "+grep", "-bash"]})
        );
        // An empty modifier layer preserves the inherited selection upstream.
        assert_eq!(
            deep_merge_settings(&base, &serde_json::json!({"defaultTools": []})),
            serde_json::json!({"defaultTools": ["read", "bash"]})
        );
    }

    #[test]
    fn default_tools_resolves_modifiers_against_builtin_list() {
        let manager = SettingsManager::in_memory(
            serde_json::json!({"defaultTools": ["+codemode", "-write"]}),
            CreateOptions::default(),
        );
        assert_eq!(
            manager.get_default_tools(),
            Some(vec![
                "read".to_string(),
                "bash".to_string(),
                "edit".to_string(),
                "codemode".to_string()
            ])
        );
    }

    #[test]
    fn default_tools_preserves_empty_list_and_absence() {
        let empty = SettingsManager::in_memory(
            serde_json::json!({"defaultTools": []}),
            CreateOptions::default(),
        );
        assert_eq!(empty.get_default_tools(), Some(Vec::new()));
        let absent = SettingsManager::in_memory(serde_json::json!({}), CreateOptions::default());
        assert_eq!(absent.get_default_tools(), None);
    }

    #[test]
    fn migrates_queue_mode_and_websockets() -> Result<(), String> {
        let mut settings: Map<String, Value> =
            serde_json::from_str(r#"{"queueMode":"all","websockets":true}"#)
                .map_err(|e| e.to_string())?;
        migrate_settings(&mut settings);
        assert_eq!(
            settings.get("steeringMode"),
            Some(&Value::String("all".into()))
        );
        assert_eq!(
            settings.get("transport"),
            Some(&Value::String("websocket".into()))
        );
        assert!(!settings.contains_key("queueMode"));
        assert!(!settings.contains_key("websockets"));
        Ok(())
    }

    #[test]
    fn migrates_false_websockets_and_skills_object() -> Result<(), String> {
        let mut settings: Map<String, Value> = serde_json::from_str(
            r#"{"websockets":false,"skills":{"enableSkillCommands":false,"customDirectories":["/x"]}}"#,
        )
        .map_err(|e| e.to_string())?;
        migrate_settings(&mut settings);
        assert_eq!(
            settings.get("transport"),
            Some(&Value::String("sse".into()))
        );
        assert_eq!(
            settings.get("enableSkillCommands"),
            Some(&Value::Bool(false))
        );
        assert_eq!(settings.get("skills"), Some(&serde_json::json!(["/x"])));
        Ok(())
    }

    #[test]
    fn migrates_retry_max_delay_and_leaves_existing_provider_value() -> Result<(), String> {
        let mut settings: Map<String, Value> =
            serde_json::from_str(r#"{"retry":{"maxDelayMs":1234}}"#).map_err(|e| e.to_string())?;
        migrate_settings(&mut settings);
        assert_eq!(
            settings.get("retry"),
            Some(&serde_json::json!({"provider": {"maxRetryDelayMs": 1234}}))
        );

        let mut settings: Map<String, Value> =
            serde_json::from_str(r#"{"retry":{"maxDelayMs":9,"provider":{"maxRetryDelayMs":5}}}"#)
                .map_err(|e| e.to_string())?;
        migrate_settings(&mut settings);
        assert_eq!(
            settings.get("retry"),
            Some(&serde_json::json!({"provider": {"maxRetryDelayMs": 5}}))
        );
        Ok(())
    }

    #[test]
    fn load_strips_bom_and_reports_parse_errors() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-load-")?;
        let global = agent.join("settings.json");
        write(&global, "\u{feff}{\"theme\":\"dark\"}")?;
        write(&project.join(".pi/settings.json"), "{ broken")?;
        let mut manager =
            SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        assert_eq!(manager.get_theme_setting(), Some("dark".to_string()));
        let errors = manager.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].scope, SettingsScope::Project);
        assert_eq!(
            errors[0].path.as_deref(),
            Some(project.join(".pi/settings.json").as_path())
        );
        assert_eq!(manager.drain_errors().len(), 1);
        assert!(manager.errors().is_empty());
        Ok(())
    }

    #[test]
    fn invalid_global_settings_are_not_overwritten() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-invalid-")?;
        let global = agent.join("settings.json");
        write(&global, "{invalid")?;
        let mut manager =
            SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        assert!(
            manager
                .set_value(&path(&["theme"]), Value::String("light".into()))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&global).map_err(|e| e.to_string())?,
            "{invalid"
        );
        Ok(())
    }

    #[test]
    fn set_value_writes_nested_path_and_preserves_external_edits() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-set-")?;
        let global = agent.join("settings.json");
        write(&global, r#"{"theme":"dark","enabledModels":["a"]}"#)?;
        let mut manager =
            SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        write(&global, r#"{"theme":"dark","enabledModels":["b"]}"#)?;
        manager.set_value(
            &path(&["markdown", "mermaid"]),
            Value::String("final".into()),
        )?;
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&global).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        assert_eq!(saved["enabledModels"], serde_json::json!(["b"]));
        assert_eq!(saved["markdown"]["mermaid"], Value::String("final".into()));
        assert_eq!(manager.get_mermaid_rendering_mode(), "final");
        Ok(())
    }

    #[test]
    fn set_project_value_requires_trust_and_creates_pi_dir() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-project-")?;
        std::fs::remove_dir(project.join(CONFIG_DIR_NAME)).map_err(|e| e.to_string())?;
        let mut manager = SettingsManager::create_with_dirs(
            &project,
            &agent,
            CreateOptions {
                project_trusted: false,
            },
        );
        let result = manager.set_project_value(&path(&["packages"]), serde_json::json!(["npm:x"]));
        assert!(result.is_err());
        assert!(!project.join(CONFIG_DIR_NAME).exists());
        manager.set_project_trusted(true);
        manager.set_project_value(&path(&["packages"]), serde_json::json!(["npm:x"]))?;
        assert!(project.join(".pi/settings.json").is_file());
        Ok(())
    }

    #[test]
    fn untrusted_project_settings_are_ignored_then_reloaded() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-trust-")?;
        write(&agent.join("settings.json"), r#"{"theme":"global"}"#)?;
        write(&project.join(".pi/settings.json"), r#"{"theme":"project"}"#)?;
        let mut manager = SettingsManager::create_with_dirs(
            &project,
            &agent,
            CreateOptions {
                project_trusted: false,
            },
        );
        assert_eq!(manager.get_theme(), Some("global".to_string()));
        assert_eq!(manager.get_project_settings(), Value::Object(Map::new()));
        manager.set_project_trusted(true);
        assert_eq!(manager.get_theme(), Some("project".to_string()));
        Ok(())
    }

    #[test]
    fn get_injects_defaults_and_respects_overrides() {
        let manager = SettingsManager::in_memory(
            serde_json::json!({"retry": {"maxRetries": 9}}),
            CreateOptions::default(),
        );
        let effective = manager.get();
        assert_eq!(effective["retry"]["maxRetries"], Value::from(9));
        assert_eq!(effective["retry"]["baseDelayMs"], Value::from(2000));
        assert_eq!(
            effective["retry"]["provider"]["maxRetryDelayMs"],
            Value::from(60000)
        );
        assert_eq!(
            effective["httpIdleTimeoutMs"],
            Value::from(DEFAULT_HTTP_IDLE_TIMEOUT_MS)
        );
        assert_eq!(effective["outputPad"], Value::from(1));
        assert_eq!(
            effective["markdown"]["mermaid"],
            Value::String("streaming".into())
        );
        assert!(effective.get("theme").is_none());

        let mut manager = manager;
        manager.apply_overrides(serde_json::json!({"outputPad": 0}));
        assert_eq!(manager.get_output_pad(), 0);
    }

    #[test]
    fn cache_warming_is_global_only_and_validated() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-warming-")?;
        write(
            &project.join(".pi/settings.json"),
            r#"{"cacheWarming":"off"}"#,
        )?;
        let manager = SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        assert_eq!(manager.get_cache_warming_mode(), "idle");
        write(
            &agent.join("settings.json"),
            r#"{"cacheWarming":"streaming"}"#,
        )?;
        let manager = SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        assert_eq!(manager.get_cache_warming_mode(), "streaming");
        write(&agent.join("settings.json"), r#"{"cacheWarming":"bogus"}"#)?;
        let manager = SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        assert_eq!(manager.get_cache_warming_mode(), "idle");
        Ok(())
    }

    #[test]
    fn http_timeout_parses_disabled_and_rejects_negative() {
        let manager = SettingsManager::in_memory(
            serde_json::json!({"httpIdleTimeoutMs": "disabled"}),
            CreateOptions::default(),
        );
        assert_eq!(manager.get_http_idle_timeout_ms(), Ok(0));
        let manager = SettingsManager::in_memory(
            serde_json::json!({"httpIdleTimeoutMs": -1}),
            CreateOptions::default(),
        );
        assert!(manager.get_http_idle_timeout_ms().is_err());
        let manager = SettingsManager::in_memory(
            serde_json::json!({"httpIdleTimeoutMs": 7.9}),
            CreateOptions::default(),
        );
        assert_eq!(manager.get_http_idle_timeout_ms(), Ok(7));
    }

    #[test]
    fn theme_and_paths_follow_upstream_special_cases() {
        let manager = SettingsManager::in_memory(
            serde_json::json!({"theme": "light/dark", "shellCommandPrefix": "shopt -s expand_aliases", "sessionDir": "~/sessions", "shellPath": "/bin/zsh"}),
            CreateOptions::default(),
        );
        assert_eq!(manager.get_theme_setting(), Some("light/dark".to_string()));
        assert_eq!(manager.get_theme(), None);
        assert_eq!(
            manager.get_shell_command_prefix(),
            Some("shopt -s expand_aliases".to_string())
        );
        assert_eq!(manager.get_shell_path(), Some("/bin/zsh".to_string()));
        if let Some(home) = std::env::var_os("HOME") {
            let expected = PathBuf::from(home)
                .join("sessions")
                .to_string_lossy()
                .into_owned();
            assert_eq!(manager.get_session_dir(), Some(expected));
        }
    }

    #[test]
    fn reload_keeps_last_good_settings_on_error() -> Result<(), String> {
        let (_root, agent, project) = paths("settings-reload-")?;
        let global = agent.join("settings.json");
        write(&global, r#"{"theme":"dark"}"#)?;
        let mut manager =
            SettingsManager::create_with_dirs(&project, &agent, CreateOptions::default());
        write(&global, "{ invalid")?;
        manager.reload();
        assert_eq!(manager.get_theme(), Some("dark".to_string()));
        assert_eq!(manager.drain_errors().len(), 1);
        Ok(())
    }

    #[test]
    fn set_nested_value_replaces_root_and_creates_intermediate_objects() -> Result<(), String> {
        let mut settings = Map::new();
        set_nested_value(&mut settings, &path(&["a", "b"]), Value::from(1))?;
        assert_eq!(settings.get("a"), Some(&serde_json::json!({"b": 1})));
        set_nested_value(&mut settings, &[], serde_json::json!({"c": 2}))?;
        assert_eq!(settings.get("c"), Some(&Value::from(2)));
        assert!(set_nested_value(&mut settings, &[], Value::from(1)).is_err());
        Ok(())
    }

    #[test]
    fn get_tool_helpers_apply_modifiers_in_order() {
        let base = vec!["read".to_string(), "bash".to_string()];
        let entries = vec![
            "-read".to_string(),
            "+grep".to_string(),
            "+grep".to_string(),
        ];
        assert_eq!(apply_tool_modifiers(&base, &entries), vec!["bash", "grep"]);
        assert!(is_tool_modifier("+x"));
        assert!(!is_tool_modifier("x"));
    }

    #[test]
    fn newly_wired_settings_getters_and_terminal_overrides_read_values() -> Result<(), String> {
        let manager = SettingsManager::in_memory(
            serde_json::json!({
                "branchSummary": { "reserveTokens": 4096, "skipPrompt": true },
                "compaction": {
                    "reserveTokens": 12000,
                    "modelOverrides": { "anthropic/claude-opus": { "reserveTokens": 24000 } }
                },
                "retry": { "provider": { "timeoutMs": 9000, "maxRetries": 5, "maxRetryDelayMs": 12000 } },
                "websocketConnectTimeoutMs": 4500,
                "externalEditor": "hx",
                "thinkingBudgets": { "low": 1024, "high": 8192 },
                "terminal": { "images": "kitty", "hyperlinks": true, "trueColor": false },
                "collapseChangelog": true,
                "codemode": { "mode": "only", "inlineBudget": 4200 },
                "showHardwareCursor": true,
            }),
            CreateOptions::default(),
        );
        assert_eq!(manager.get_branch_summary_settings().0, 4096);
        assert!(manager.get_branch_summary_skip_prompt());
        let (_, reserve, _) =
            manager.get_compaction_settings_for_model(Some("anthropic"), Some("claude-opus"))?;
        assert_eq!(reserve, 24000);
        assert_eq!(
            manager.get_provider_retry_settings(),
            (Some(9000), Some(5), 12000)
        );
        assert_eq!(manager.get_websocket_connect_timeout_ms()?, Some(4500));
        assert_eq!(manager.get_external_editor_command(), "hx");
        let budgets = manager.get_thinking_budgets();
        assert_eq!(budgets.get(&pi_rs_agent::ThinkingLevel::Low), Some(&1024));
        assert_eq!(budgets.get(&pi_rs_agent::ThinkingLevel::High), Some(&8192));
        let overrides = manager.get_terminal_capability_overrides();
        assert_eq!(
            overrides.images,
            Some(Some(pi_rs_tui::ImageProtocol::Kitty))
        );
        assert_eq!(overrides.hyperlinks, Some(true));
        assert_eq!(overrides.true_color, Some(false));
        assert!(manager.get_collapse_changelog());
        assert_eq!(
            manager.get_codemode_mode(),
            crate::extensions::codemode::CodemodeMode::Only
        );
        assert_eq!(manager.get_codemode_inline_budget(), 4200);
        assert!(manager.get_show_hardware_cursor());
        Ok(())
    }
}
