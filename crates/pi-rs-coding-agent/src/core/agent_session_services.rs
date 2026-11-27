//! Cwd-bound adapters for the native CLI. The SDK's in-memory defaults are not
//! used here: the session, settings, and auth all refer to the real stores.

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use pi_rs_agent::{
    Agent, AgentInitialState, AgentMessage, AgentOptions, AgentTool, QueueMode, ThinkingLevel,
};
use pi_rs_ai::{Model, StreamOptions};
use serde_json::{Value, json};

use crate::{
    cli::{
        args::{Args, FlagValue},
        project_trust::create_project_trust_context,
    },
    config,
};

use super::{
    agent_session::{
        self, AgentSession, AgentSessionConfig, ForkPosition, SessionError,
        SessionManager as SessionStore, SettingsStore, TokenCounts,
    },
    agent_session_runtime::{
        CreateAgentSessionRuntimeFactory, CreateAgentSessionRuntimeResult, DiagnosticLevel,
        RuntimeDiagnostic, RuntimeServices, RuntimeTarget,
    },
    event_bus::{EventBus, ExtensionEvent},
    export_html::{ExportOptions, SessionSnapshot, export_session_html},
    extensions::{
        loader::{self, LoadOptions},
        runner::ExtensionRunner,
        types::{ExtensionFactory, ExtensionHost, ExtensionRegistration, FlagKind},
        wasm::WasmExtension,
    },
    model_resolver::{
        CliModelOptions, InitialModelOptions, ScopedModel, find_initial_model,
        parse_thinking_level, resolve_cli_model, resolve_model_scope_from_models,
    },
    model_runtime::ModelRuntime,
    package_manager::ResourceKind,
    project_trust::{
        AppMode, DefaultProjectTrust, ExtensionTrustResult, ProjectTrustStore,
        ResolveProjectTrustedOptions, has_trust_requiring_project_resources,
        resolve_project_trusted,
    },
    resource_loader::{ResourceExtensionPaths, ResourceLoader, ResourceLoaderOptions, Resources},
    session_manager::{SessionEntry, SessionManager},
    settings_diagnostics::collect_settings_diagnostics,
    settings_manager::{CreateOptions, DEFAULT_TOOL_NAMES, SettingsManager, apply_tool_modifiers},
    system_prompt::{ContextFile, SystemPromptOptions, build_system_prompt_sections},
    tools::wrappers::{
        BashTool, EditTool, FindTool, GrepTool, LsTool, PowerShellTool, ReadTool, WriteTool,
    },
};

/// Keeps all mutations to a v3 session on one manager, including no-session
/// runs (which still have a v3 header and tree, but no file).
pub struct DiskSession(Mutex<SessionManager>);

impl DiskSession {
    pub fn new(manager: SessionManager) -> Self {
        Self(Mutex::new(manager))
    }
    pub fn lock(&self) -> MutexGuard<'_, SessionManager> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn session_error(error: String) -> SessionError {
    SessionError::Extension(error)
}

impl SessionStore for DiskSession {
    fn session_id(&self) -> String {
        self.lock().get_session_id().to_owned()
    }
    fn session_file(&self) -> Option<String> {
        self.lock()
            .get_session_file()
            .map(|path| path.to_string_lossy().into_owned())
    }
    fn cwd(&self) -> String {
        self.lock().get_cwd().to_owned()
    }
    fn messages(&self) -> Vec<AgentMessage> {
        self.lock()
            .build_session_context()
            .messages
            .into_iter()
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect()
    }
    fn append_message(&self, message: &AgentMessage) -> String {
        self.try_append_message(message).unwrap_or_else(|error| {
            eprintln!("{error}");
            String::new()
        })
    }
    fn try_append_message(&self, message: &AgentMessage) -> agent_session::Result<String> {
        let message =
            serde_json::to_value(message).map_err(|error| session_error(error.to_string()))?;
        self.lock().append_message(message).map_err(session_error)
    }
    fn append_model_change(&self, provider: &str, model_id: &str) {
        if let Err(error) = self.lock().append_model_change(provider, model_id) {
            eprintln!("{error}");
        }
    }
    fn append_thinking_level_change(&self, level: ThinkingLevel) {
        if let Err(error) = self.lock().append_thinking_level_change(level.as_str()) {
            eprintln!("{error}");
        }
    }
    fn append_compaction(&self, summary: &str, first: Option<String>) -> String {
        let mut manager = self.lock();
        let first = first.or_else(|| {
            manager
                .get_branch(None)
                .into_iter()
                .filter(|entry| matches!(entry, SessionEntry::Message { .. }))
                .rev()
                .nth(2)
                .map(|entry| entry.id().to_owned())
        });
        manager
            .append_compaction(summary, first.as_deref(), 0)
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                String::new()
            })
    }
    fn try_append_compaction(
        &self,
        result: &agent_session::CompactionResult,
    ) -> agent_session::Result<String> {
        self.lock()
            .append_compaction(
                &result.summary,
                result.first_kept_entry_id.as_deref(),
                result.tokens_before,
            )
            .map_err(session_error)
    }
    fn entries(&self) -> Vec<agent_session::SessionEntry> {
        self.lock()
            .get_entries()
            .iter()
            .filter_map(|entry| {
                let data = serde_json::to_value(entry).ok()?;
                Some(agent_session::SessionEntry {
                    id: entry.id().to_owned(),
                    kind: entry.entry_type().to_owned(),
                    data,
                })
            })
            .collect()
    }
    fn navigate(&self, id: &str) -> agent_session::Result<()> {
        self.lock().branch(id).map_err(session_error)
    }
    fn fork(&self, id: &str, position: ForkPosition) -> agent_session::Result<()> {
        let mut manager = self.lock();
        let entry = manager
            .get_entry(id)
            .ok_or_else(|| SessionError::InvalidEntryId(id.to_owned()))?;
        let parent_session = manager
            .get_session_file()
            .map(|path| path.to_string_lossy().into_owned());
        let leaf = match position {
            ForkPosition::At => Some(id),
            ForkPosition::Before => entry.parent_id(),
        };
        match leaf.map(str::to_owned) {
            Some(leaf) => manager.fork(&leaf).map(|_| ()).map_err(session_error),
            None => manager
                .new_session(None, parent_session.as_deref())
                .map_err(session_error),
        }
    }
    fn stats(&self) -> agent_session::SessionStatsSnapshot {
        let mut stats = agent_session::SessionStatsSnapshot::default();
        for entry in self.lock().get_entries() {
            let usage = match entry {
                SessionEntry::Message { message, .. } => {
                    stats.total_messages += 1;
                    match message["role"].as_str() {
                        Some("user") => {
                            stats.user_messages += 1;
                            None
                        }
                        Some("assistant") => {
                            stats.assistant_messages += 1;
                            stats.tool_calls += message["content"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter(|block| block["type"] == "toolCall")
                                .count();
                            message.get("usage")
                        }
                        Some("toolResult") => {
                            stats.tool_results += 1;
                            message.get("usage")
                        }
                        _ => None,
                    }
                }
                SessionEntry::Usage { usage, .. } => Some(usage),
                SessionEntry::Compaction { usage, .. }
                | SessionEntry::BranchSummary { usage, .. } => usage.as_ref(),
                _ => None,
            };
            if let Some(usage) = usage {
                stats.tokens.input += usage["input"].as_f64().unwrap_or_default();
                stats.tokens.output += usage["output"].as_f64().unwrap_or_default();
                stats.tokens.cache_read += usage["cacheRead"].as_f64().unwrap_or_default();
                stats.tokens.cache_write += usage["cacheWrite"].as_f64().unwrap_or_default();
                stats.cost += usage["cost"]["total"].as_f64().unwrap_or_default();
            }
        }
        let TokenCounts {
            input,
            output,
            cache_read,
            cache_write,
            ..
        } = stats.tokens;
        stats.tokens.total = input + output + cache_read + cache_write;
        stats
    }
    fn disk(&self) -> Option<&Self> {
        Some(self)
    }
    fn append_usage(
        &self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: &pi_rs_ai::Usage,
        note: Option<&str>,
    ) {
        if let Ok(value) = serde_json::to_value(usage) {
            let _ = self.lock().append_usage(kind, provider, model, value, note);
        }
    }
}

struct DiskSettings(Arc<Mutex<SettingsManager>>);

fn queue_mode(value: &Value) -> QueueMode {
    if value.as_str() == Some("all") {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

fn set_setting(settings: &Mutex<SettingsManager>, path: &[&str], value: Value) {
    let mut settings = settings.lock().unwrap_or_else(|error| error.into_inner());
    let path = path.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
    if let Err(error) = settings.set_value(&path, value) {
        eprintln!("Warning: {error}");
    }
}

impl DiskSettings {
    fn value(&self) -> Value {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get()
    }
}

impl SettingsStore for DiskSettings {
    fn retry_enabled(&self) -> bool {
        self.value()["retry"]["enabled"].as_bool().unwrap_or(true)
    }
    fn max_retries(&self) -> usize {
        self.value()["retry"]["maxRetries"].as_u64().unwrap_or(3) as usize
    }
    fn retry_base_delay_ms(&self) -> u64 {
        self.value()["retry"]["baseDelayMs"]
            .as_u64()
            .unwrap_or(2_000)
    }
    fn compaction_enabled(&self) -> bool {
        self.value()["compaction"]["enabled"]
            .as_bool()
            .unwrap_or(true)
    }
    fn compaction_threshold(&self) -> f64 {
        0.8
    }
    fn steering_mode(&self) -> QueueMode {
        queue_mode(&self.value()["steeringMode"])
    }
    fn follow_up_mode(&self) -> QueueMode {
        queue_mode(&self.value()["followUpMode"])
    }
    fn set_retry_enabled(&self, enabled: bool) {
        set_setting(&self.0, &["retry", "enabled"], json!(enabled));
    }
    fn set_compaction_enabled(&self, enabled: bool) {
        set_setting(&self.0, &["compaction", "enabled"], json!(enabled));
    }
    fn set_steering_mode(&self, mode: QueueMode) {
        set_setting(&self.0, &["steeringMode"], json!(mode));
    }
    fn set_follow_up_mode(&self, mode: QueueMode) {
        set_setting(&self.0, &["followUpMode"], json!(mode));
    }
    fn compaction_reserve_tokens(&self) -> Option<u64> {
        Some(
            self.value()["compaction"]["reserveTokens"]
                .as_u64()
                .unwrap_or(16_384),
        )
    }
    fn shell_path(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_shell_path()
    }
    fn shell_command_prefix(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_shell_command_prefix()
    }
    fn auto_resize_images(&self) -> bool {
        self.value()["images"]["autoResize"]
            .as_bool()
            .unwrap_or(true)
    }
    fn trusted_project(&self) -> Option<bool> {
        Some(
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_project_trusted(),
        )
    }
    fn raw_value(&self) -> Value {
        self.value()
    }
    fn cache_warming_mode(&self) -> super::cache_warmer::CacheWarmingMode {
        let mode = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_cache_warming_mode();
        super::cache_warmer::CacheWarmingMode::from_setting(mode)
    }
    fn compaction_reserve_tokens_for_model(&self, provider: &str, model_id: &str) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_compaction_settings_for_model(Some(provider), Some(model_id))
            .ok()
            .map(|(_, reserve, _)| reserve as u64)
    }
    fn branch_summary_reserve_tokens(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_branch_summary_settings()
            .0
    }
    fn branch_summary_skip_prompt(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_branch_summary_skip_prompt()
    }
}

struct ModelCompactor {
    runtime: Arc<ModelRuntime>,
    settings: Arc<Mutex<SettingsManager>>,
}

impl agent_session::Compactor for ModelCompactor {
    fn compact<'a>(
        &'a self,
        model: Model,
        manager: &'a dyn SessionStore,
        instructions: &'a str,
        signal: &'a pi_rs_agent::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, agent_session::Result<agent_session::CompactionResult>>
    {
        Box::pin(async move {
            use super::compaction;
            let disk = manager
                .disk()
                .ok_or_else(|| session_error("Compaction requires a session tree".to_owned()))?;
            let projection = disk.lock().build_session_projection();
            let mut messages = Vec::new();
            let mut previous_summary = None;
            for entry in projection.entries {
                for mut message in entry.messages {
                    if message.get("role").and_then(Value::as_str) == Some("compactionSummary") {
                        previous_summary = message["summary"].as_str().map(str::to_owned);
                    } else if let Some(fields) = message.as_object_mut() {
                        fields.insert("id".to_owned(), json!(entry.source_entry.id()));
                        messages.push(message);
                    }
                }
            }
            let (reserve_tokens, keep_recent_tokens) = self
                .settings
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_compaction_settings_for_model(Some(&model.provider), Some(&model.id))
                .map(|(_, reserve, keep_recent)| (reserve, keep_recent))
                .unwrap_or((
                    compaction::DEFAULT_RESERVE_TOKENS,
                    compaction::DEFAULT_KEEP_RECENT_TOKENS,
                ));
            let instructions = (!instructions.is_empty()).then_some(instructions);
            let context = compaction::CompactionContext::new(messages);
            let options = compaction::CompactionOptions {
                reserve_tokens,
                keep_recent_tokens,
                previous_summary: previous_summary.clone(),
                ..compaction::CompactionOptions::default()
            };
            let before = compaction::estimate_context_tokens(&context.messages).tokens as u64;
            let result = compaction::compact(&context, &options, |prefix| {
                self.runtime.summarize(
                    model.clone(),
                    compaction::build_summarization_prompt(
                        &prefix,
                        previous_summary.as_deref(),
                        instructions,
                    ),
                    signal.clone(),
                )
            })
            .await
            .map_err(session_error)?;
            let after = compaction::estimate_context_tokens(&result.messages_kept).tokens
                + compaction::estimate_tokens(&result.summary);
            Ok(agent_session::CompactionResult {
                summary: result.summary,
                first_kept_entry_id: Some(result.first_kept_entry_id),
                tokens_before: before,
                estimated_tokens_after: after as u64,
            })
        })
    }
}

struct NativeExtensions {
    runner: Arc<ExtensionRunner>,
    resources: Resources,
}

impl agent_session::ExtensionRunner for NativeExtensions {
    fn emit(&self, event: &Value) -> Value {
        if let Some(name) = event["type"].as_str()
            && let Ok(kind) = ExtensionEvent::parse(name)
            && !matches!(
                kind,
                ExtensionEvent::SessionStart | ExtensionEvent::SessionShutdown
            )
            && self.runner.has_handlers(kind)
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let runner = self.runner.clone();
            let data = event.clone();
            handle.spawn(async move { runner.emit(kind, data).await });
        }
        Value::Null
    }
    fn emit_async<'a>(&'a self, event: Value) -> futures_util::future::BoxFuture<'a, Value> {
        Box::pin(async move {
            let Some(name) = event["type"].as_str() else {
                return Value::Null;
            };
            let Ok(kind) = ExtensionEvent::parse(name) else {
                return Value::Null;
            };
            if !self.runner.has_handlers(kind) {
                return Value::Null;
            }
            if kind == ExtensionEvent::BeforeAgentStart {
                return self.runner.emit_before_agent_start(event).await;
            }
            let _ = self.runner.emit(kind, event).await;
            Value::Null
        })
    }
    fn has_handlers(&self, event_type: &str) -> bool {
        ExtensionEvent::parse(event_type).is_ok_and(|kind| self.runner.has_handlers(kind))
    }
    fn native(&self) -> Option<&Arc<ExtensionRunner>> {
        Some(&self.runner)
    }
    fn resources(&self) -> Option<&Resources> {
        Some(&self.resources)
    }
}

pub fn create_factory(
    args: Arc<Args>,
    launch_cwd: PathBuf,
    mode: AppMode,
) -> CreateAgentSessionRuntimeFactory {
    Arc::new(move |target| {
        let args = args.clone();
        let launch_cwd = launch_cwd.clone();
        Box::pin(async move {
            create_services(&args, &launch_cwd, mode, target)
                .await
                .map_err(session_error)
        })
    })
}

fn cli_paths(paths: &[String], launch_cwd: &Path) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|path| config::resolve_path(path, launch_cwd, false))
        .collect()
}

fn error(message: impl Into<String>) -> RuntimeDiagnostic {
    RuntimeDiagnostic {
        level: DiagnosticLevel::Error,
        message: message.into(),
    }
}

async fn load_extensions(
    runner: &ExtensionRunner,
    paths: Vec<String>,
    agent_dir: &Path,
    project_trusted: bool,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    let mut factories = Vec::<ExtensionFactory>::new();
    for path in paths {
        if let Some(factory) =
            crate::extensions::headless::factory(&path, agent_dir, project_trusted)
        {
            factories.push(factory);
        } else if Path::new(&path)
            .extension()
            .is_some_and(|ext| ext == "wasm")
        {
            factories.push(Arc::new(move || {
                let bytes = std::fs::read(&path).map_err(|error| format!("{path}: {error}"))?;
                let extension = WasmExtension::new(
                    &path,
                    Some(&bytes),
                    ExtensionRegistration::new(&path),
                    false,
                    false,
                )
                .map_err(|error| format!("{path}: {error}"))?;
                Ok(Box::new(extension))
            }));
        } else {
            let reason = if Path::new(&path).exists() {
                "native pi extensions must be WASM components"
            } else {
                "file not found"
            };
            diagnostics.push(error(format!(
                "Failed to load extension \"{path}\": {reason}"
            )));
        }
    }
    let loaded = runner.load_extensions(&factories).await;
    for issue in loaded.errors {
        diagnostics.push(error(format!(
            "Failed to load extension \"{}\": {}",
            issue.extension_path, issue.error
        )));
    }
}

fn diagnostic_flags(
    runner: &ExtensionRunner,
    args: &Args,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    let flags = runner.get_flags();
    let mut unknown = Vec::new();
    for (name, value) in &args.unknown_flags {
        match flags.get(name).map(|flag| flag.kind) {
            Some(FlagKind::Boolean) => runner.set_flag_value(name, FlagValue::Bool(true)),
            Some(FlagKind::String) if matches!(value, FlagValue::Str(_)) => {
                runner.set_flag_value(name, value.clone())
            }
            Some(FlagKind::String) => diagnostics.push(error(format!(
                "Extension flag \"--{name}\" requires a value"
            ))),
            None => unknown.push(format!("--{name}")),
        }
    }
    if !unknown.is_empty() {
        diagnostics.push(error(format!(
            "Unknown option{}: {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown.join(", ")
        )));
    }
}

async fn create_services(
    args: &Args,
    launch_cwd: &Path,
    mode: AppMode,
    target: RuntimeTarget,
) -> Result<CreateAgentSessionRuntimeResult, String> {
    let cwd = PathBuf::from(&target.cwd);
    let agent_dir = target
        .agent_dir
        .as_ref()
        .map(PathBuf::from)
        .map_or_else(config::get_agent_dir, Ok)?;
    let mut settings = SettingsManager::create_with_dirs(
        &cwd,
        &agent_dir,
        CreateOptions {
            project_trusted: false,
        },
    );
    let load_options = LoadOptions {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        config_dir_name: config::CONFIG_DIR_NAME.to_owned(),
        cli_paths: args
            .extensions
            .iter()
            .map(|path| {
                if path.starts_with("builtin:") {
                    Ok(PathBuf::from(path))
                } else {
                    config::resolve_path(path, launch_cwd, false)
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
        project_trusted: false,
        no_extensions: args.no_extensions,
        disabled_builtins: if args.no_mcp {
            BTreeSet::from(["mcp".to_owned()])
        } else {
            BTreeSet::new()
        },
    };
    let bus = Arc::new(EventBus::new());
    let runner = Arc::new(ExtensionRunner::new(
        Vec::new(),
        Arc::new(ExtensionHost::new(bus.clone())),
        bus.clone(),
        &target.cwd,
    ));
    let mut diagnostics = Vec::new();
    let before_trust =
        crate::modes::interactive::selectors::ConfiguredResources::new(&settings, &cwd, &agent_dir);
    let pre_trust_paths = configured_extensions(&load_options, &before_trust);
    load_extensions(
        &runner,
        pre_trust_paths
            .iter()
            .filter(|path| !path.starts_with("builtin:"))
            .cloned()
            .collect(),
        &agent_dir,
        false,
        &mut diagnostics,
    )
    .await;
    let extension_result = if args.project_trust_override.is_none()
        && runner.has_handlers(ExtensionEvent::ProjectTrust)
    {
        let context = create_project_trust_context(&cwd, mode, false);
        runner
            .emit_project_trust(context.extension_event())
            .await
            .and_then(|result| {
                Some(ExtensionTrustResult {
                    trusted: result["trusted"].as_str()?.to_owned(),
                    remember: result["remember"].as_bool().unwrap_or(false),
                    errors: Vec::new(),
                })
            })
    } else {
        None
    };
    let default_project_trust = match settings.get()["defaultProjectTrust"].as_str() {
        Some("always") => DefaultProjectTrust::Always,
        Some("never") => DefaultProjectTrust::Never,
        _ => DefaultProjectTrust::Ask,
    };
    let trust_store = ProjectTrustStore::new(&agent_dir);
    let prompt_trust = mode == AppMode::Interactive
        && target.reason == agent_session::SessionReason::Startup
        && !args.help
        && args.list_models.is_none()
        && args.project_trust_override.is_none()
        && extension_result.is_none()
        && default_project_trust == DefaultProjectTrust::Ask
        && has_trust_requiring_project_resources(&cwd)
        && trust_store
            .get(&cwd)
            .map_err(|error| error.to_string())?
            .is_none();
    let ui_decision = if prompt_trust {
        let cwd = cwd.clone();
        let agent_dir = agent_dir.clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let settings = SettingsManager::create_with_dirs(
                &cwd,
                &agent_dir,
                CreateOptions {
                    project_trusted: false,
                },
            );
            handle.block_on(crate::cli::startup_ui::select_project_trust(
                &settings, &agent_dir, &cwd,
            ))
        })
        .await
        .map_err(|error| error.to_string())??
    } else {
        None
    };
    let trusted = resolve_project_trusted(
        &trust_store,
        &ResolveProjectTrustedOptions {
            cwd: Some(&cwd),
            trust_override: args.project_trust_override,
            default_project_trust: Some(default_project_trust),
            extension_result,
            ui_decision,
            has_ui: prompt_trust,
            mode: Some(mode),
            ..ResolveProjectTrustedOptions::default()
        },
    )
    .map_err(|error| error.to_string())?;
    settings.set_project_trusted(trusted);
    if !args.help && args.list_models.is_none() {
        let auto_install = crate::core::startup::auto_install_missing_packages(
            &settings,
            &cwd,
            &agent_dir,
            args.offline,
        )
        .await;
        diagnostics.extend(auto_install.diagnostics);
    }
    let resources_config =
        crate::modes::interactive::selectors::ConfiguredResources::new(&settings, &cwd, &agent_dir);
    let mut post_paths = configured_extensions(
        &LoadOptions {
            project_trusted: trusted,
            ..load_options.clone()
        },
        &resources_config,
    );
    post_paths.retain(|path| path.starts_with("builtin:") || !pre_trust_paths.contains(path));
    if !args.no_extensions {
        for path in crate::extensions::headless::COMMANDS {
            let bare = path.strip_prefix("builtin:").unwrap_or(path);
            if !load_options.disabled_builtins.contains(bare)
                && resources_config.enabled(ResourceKind::Extension, Path::new(path), true)
                && !post_paths.iter().any(|entry| entry == path)
            {
                post_paths.push((*path).to_owned());
            }
        }
    }
    if !post_paths.is_empty() {
        let mut extensions = runner.extensions();
        load_extensions(&runner, post_paths, &agent_dir, trusted, &mut diagnostics).await;
        extensions.extend(runner.extensions());
        let mut warnings = Vec::new();
        runner
            .replace_loaded_extensions(loader::omit_replaced_extensions(extensions, &mut warnings));
        diagnostics.extend(
            warnings
                .into_iter()
                .map(|warning| RuntimeDiagnostic::warning(warning.message)),
        );
    }
    diagnostic_flags(&runner, args, &mut diagnostics);
    diagnostics.extend(collect_settings_diagnostics(&mut settings));
    let mut loader = ResourceLoader::new(&cwd, &agent_dir, bus);
    loader.project_trusted = trusted;
    loader.options = ResourceLoaderOptions {
        no_extensions: args.no_extensions,
        no_skills: args.no_skills,
        no_prompts: args.no_prompt_templates,
        no_themes: args.no_themes,
        no_context_files: args.no_context_files,
        cli_paths: load_options.cli_paths,
        disabled_builtins: load_options.disabled_builtins,
        system_prompt: read_prompt(args.system_prompt.as_deref(), launch_cwd)?,
        append_system_prompt: args
            .append_system_prompt
            .iter()
            .map(|text| read_prompt(Some(text), launch_cwd).map(Option::unwrap_or_default))
            .collect::<Result<_, _>>()?,
    };
    let skill_paths = cli_paths(&args.skills, launch_cwd)?;
    let prompt_paths = cli_paths(&args.prompt_templates, launch_cwd)?;
    let theme_paths = cli_paths(&args.themes, launch_cwd)?;
    let combine = |cli: &[PathBuf], kind, directory, no_defaults: bool| {
        let mut paths = cli.to_vec();
        if !no_defaults {
            let global = agent_dir.join(directory);
            let project = cwd.join(config::CONFIG_DIR_NAME).join(directory);
            let discovered = |path: &Path| {
                if kind == ResourceKind::Skill {
                    path.starts_with(&global) || (trusted && path.starts_with(&project))
                } else {
                    path.parent()
                        .is_some_and(|parent| parent == global || (trusted && parent == project))
                }
            };
            paths.extend(
                resources_config
                    .enabled_paths(kind)
                    .into_iter()
                    .filter(|path| {
                        !discovered(path) && !cli.iter().any(|base| path.starts_with(base))
                    }),
            );
        }
        paths
    };
    loader.extend_resources(ResourceExtensionPaths {
        skill_paths: combine(&skill_paths, ResourceKind::Skill, "skills", args.no_skills),
        prompt_paths: combine(
            &prompt_paths,
            ResourceKind::Prompt,
            "prompts",
            args.no_prompt_templates,
        ),
        theme_paths: combine(&theme_paths, ResourceKind::Theme, "themes", args.no_themes),
    });
    loader.reload(Some(&runner))?;
    let mut resources = loader.resources().clone();
    resources.skills.retain(|entry| {
        included_resource(
            &resources_config,
            ResourceKind::Skill,
            &entry.file_path,
            &skill_paths,
            args.no_skills,
        )
    });
    resources.prompts.retain(|entry| {
        included_resource(
            &resources_config,
            ResourceKind::Prompt,
            &entry.file_path,
            &prompt_paths,
            args.no_prompt_templates,
        )
    });
    resources.themes.retain(|entry| {
        included_resource(
            &resources_config,
            ResourceKind::Theme,
            &entry.source_path,
            &theme_paths,
            args.no_themes,
        )
    });

    let mut model_runtime =
        ModelRuntime::create_with_settings(&agent_dir, &runner, &settings, &mut diagnostics)
            .await?;
    if args.list_models.is_some()
        && let Some(load_error) = model_runtime.get_error()
    {
        diagnostics.push(RuntimeDiagnostic::warning(format!(
            "errors loading models.json:\n{load_error}"
        )));
    }
    let context = target
        .session_manager
        .disk()
        .map(|disk| disk.lock().build_session_context());
    let settings_value = settings.get();
    let (model, thinking, fallback) = select_model(
        args,
        &settings_value,
        context.as_ref(),
        &mut model_runtime,
        &mut diagnostics,
    );
    let hooks = build_agent_hooks(&runner);
    let nested_tools_slot: Arc<Mutex<Arc<super::nested_tool_calls::GetToolsFn>>> =
        Arc::new(Mutex::new(Arc::new(Arc::new(Vec::new))));
    let nested_seq_slot: Arc<Mutex<Arc<super::nested_tool_calls::IsSequentialFn>>> =
        Arc::new(Mutex::new(Arc::new(Arc::new(|| true))));
    let tools_slot_for_nested = Arc::clone(&nested_tools_slot);
    let seq_slot_for_nested = Arc::clone(&nested_seq_slot);
    let nested_runner = Arc::new(super::nested_tool_calls::NestedToolCallRunner::new(
        Arc::new(move || {
            let getter = tools_slot_for_nested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            getter()
        }),
        Arc::new(move || {
            let checker = seq_slot_for_nested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            checker()
        }),
        hooks.tool_calls.clone(),
        Arc::new(|_| {}),
    ));
    let nested_for_wasm = Arc::clone(&nested_runner);
    for extension in runner.extensions() {
        if let Some(wasm) = extension
            .as_any()
            .and_then(|any| any.downcast_ref::<WasmExtension>())
        {
            let runner_wasm = Arc::clone(&nested_for_wasm);
            wasm.set_async_tool_handler(Arc::new(move |caller_id, name, args_json| {
                let runner = Arc::clone(&runner_wasm);
                Box::pin(async move {
                    let parsed: Value = serde_json::from_str(&args_json)
                        .map_err(|error| format!("invalid nested tool args JSON: {error}"))?;
                    let outcome = runner
                        .execute(&caller_id, &name, parsed, None, false, None)
                        .await;
                    serde_json::to_string(&json!({
                        "toolCall": outcome.tool_call,
                        "result": outcome.result,
                        "isError": outcome.is_error,
                    }))
                    .map_err(|error| error.to_string())
                })
            }));
        }
    }
    let builtin_tools = selected_tools(&cwd, args, &settings);
    let all_nested_tools =
        collect_all_extension_tools(&builtin_tools, &runner, Some(&nested_runner), args);
    {
        let all_for_nested = all_nested_tools.clone();
        *nested_tools_slot.lock().unwrap_or_else(|e| e.into_inner()) =
            Arc::new(Arc::new(move || all_for_nested.clone()));
    }
    let tools = collect_session_tools(&builtin_tools, &runner, Some(&nested_runner), args);
    let system_prompt = build_system_prompt_sections(&SystemPromptOptions {
        cwd: cwd.clone(),
        custom_prompt: resources.system_prompt.clone(),
        selected_tools: Some(
            tools
                .iter()
                .map(|tool| tool.declaration().name.clone())
                .collect(),
        ),
        append_system_prompt: (!resources.append_system_prompt.is_empty())
            .then(|| resources.append_system_prompt.join("\n\n")),
        context_files: resources
            .agents_files
            .iter()
            .map(|file| ContextFile {
                path: file.path.to_string_lossy().into_owned(),
                content: file.content.clone(),
            })
            .collect(),
        skills: resources.skills.clone(),
        ..SystemPromptOptions::default()
    })?;
    if let Some(disk) = target.session_manager.disk() {
        let mut manager = disk.lock();
        if manager.get_entry_count() == 0 {
            if let Some(model) = &model {
                manager.append_model_change(&model.provider, &model.id)?;
            }
            manager.append_thinking_level_change(thinking.as_str())?;
        }
    }
    let (provider_timeout_ms, provider_max_retries, provider_max_retry_delay_ms) =
        settings.get_provider_retry_settings();
    let http_idle_timeout_ms = settings.get_http_idle_timeout_ms().unwrap_or(300_000);
    let effective_timeout_ms = provider_timeout_ms.or(Some(if http_idle_timeout_ms == 0 {
        i32::MAX as u64
    } else {
        http_idle_timeout_ms as u64
    }));
    let websocket_connect_timeout_ms = settings.get_websocket_connect_timeout_ms().ok().flatten();
    let thinking_budgets = settings.get_thinking_budgets();
    let model_runtime = Arc::new(model_runtime.with_extensions(runner.clone()));
    let stream_fn = model_runtime.stream();
    let settings_store = Arc::new(Mutex::new(settings));
    let settings: Arc<dyn SettingsStore> = Arc::new(DiskSettings(settings_store.clone()));
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model,
            thinking_level: Some(thinking),
            tools: Vec::new(),
            system_prompt: None,
            messages: target.session_manager.messages(),
        },
        stream_fn: Some(stream_fn),
        hooks,
        stream_options: StreamOptions {
            session_id: Some(target.session_manager.session_id()),
            websocket_connect_timeout_ms,
            thinking_budgets: thinking_budgets.clone(),
            request: pi_rs_ai::ProviderRequestOptions {
                timeout_ms: effective_timeout_ms,
                max_retries: provider_max_retries,
                max_retry_delay_ms: Some(provider_max_retry_delay_ms),
                ..pi_rs_ai::ProviderRequestOptions::default()
            },
            ..StreamOptions::default()
        },
        ..AgentOptions::default()
    })
    .map_err(|error| error.to_string())?;
    {
        let agent_for_seq = agent.clone();
        *nested_seq_slot.lock().unwrap_or_else(|e| e.into_inner()) =
            Arc::new(Arc::new(move || {
                agent_for_seq.tool_execution() == pi_rs_agent::ToolExecutionMode::Sequential
            }));
    }
    // Declare the initial loadout on the first prompt, not as hidden history.
    agent.set_tools(tools);
    {
        let agent_refresh = agent.clone();
        let runner_weak = Arc::downgrade(&runner);
        let args_refresh = args.clone();
        let builtins_refresh = builtin_tools.clone();
        let nested_refresh = Arc::clone(&nested_runner);
        let nested_slot_refresh = Arc::clone(&nested_tools_slot);
        runner.host.set_refresh_tools(Arc::new(move || {
            if let Some(runner) = runner_weak.upgrade() {
                let all = collect_all_extension_tools(
                    &builtins_refresh,
                    &runner,
                    Some(&nested_refresh),
                    &args_refresh,
                );
                *nested_slot_refresh
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Arc::new(Arc::new(move || all.clone()));
                agent_refresh.set_tools(collect_session_tools(
                    &builtins_refresh,
                    &runner,
                    Some(&nested_refresh),
                    &args_refresh,
                ));
            }
        }));

        let agent_model = agent.clone();
        let agent_set_model = agent.clone();
        let registry_for_model = model_runtime.clone();
        let agent_thinking = agent.clone();
        let agent_set_thinking = agent.clone();
        let agent_tools = agent.clone();
        let agent_set_tools = agent.clone();
        let runner_set_tools = Arc::downgrade(&runner);
        let runner_all_tools = Arc::downgrade(&runner);
        let runner_for_commands = Arc::downgrade(&runner);
        let builtins_all_tools = builtin_tools.clone();
        let args_set_tools = args.clone();
        let nested_set_tools = Arc::clone(&nested_runner);
        let session_for_entry = target.session_manager.clone();
        let session_for_set_name = target.session_manager.clone();
        let session_for_get_name = target.session_manager.clone();
        let session_for_label = target.session_manager.clone();
        let session_for_message = target.session_manager.clone();
        let agent_for_message = agent.clone();
        let agent_for_user = agent.clone();
        let session_for_user = target.session_manager.clone();
        let settings_for_host = settings.clone();
        runner
            .host
            .set_actions(super::extensions::types::ExtensionHostActions {
                get_model: Some(Arc::new(move || {
                    let model = agent_model.model();
                    Some(format!("{}/{}", model.provider, model.id))
                })),
                get_active_tools: Some(Arc::new(move || {
                    agent_tools
                        .tools()
                        .into_iter()
                        .map(|tool| tool.declaration().name.clone())
                        .collect()
                })),
                get_all_tools: Some(Arc::new(move || {
                    let mut defs = Vec::new();
                    for tool in &builtins_all_tools {
                        let decl = tool.declaration();
                        let mut def = super::extensions::types::ToolDefinition::new(
                            &decl.name,
                            decl.parameters.clone(),
                        );
                        def.label = tool.label().to_owned();
                        def.description = decl.description.clone();
                        defs.push(def);
                    }
                    if let Some(runner) = runner_all_tools.upgrade() {
                        defs.extend(runner.get_all_registered_tools());
                    }
                    defs
                })),
                set_active_tools: Some(Arc::new(move |names| {
                    let Some(runner) = runner_set_tools.upgrade() else {
                        return Err(super::extensions::types::STALE_MESSAGE.to_string());
                    };
                    let available = collect_all_extension_tools(
                        &builtin_tools,
                        &runner,
                        Some(&nested_set_tools),
                        &args_set_tools,
                    );
                    let selected = resolve_active_tools(&available, &names)?;
                    agent_set_tools.set_tools(selected);
                    Ok(())
                })),
                send_message: Some(Arc::new(move |custom_type, content, trigger_turn| {
                    let display = true;
                    if let Some(disk) = session_for_message.disk() {
                        disk.lock().append_custom_message_entry(
                            &custom_type,
                            content.clone(),
                            display,
                            None,
                        )?;
                    }
                    let mut fields = serde_json::Map::new();
                    fields.insert("customType".to_string(), Value::String(custom_type));
                    fields.insert("content".to_string(), content);
                    fields.insert("display".to_string(), Value::Bool(display));
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0_u64, |duration| {
                            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                        });
                    fields.insert("timestamp".to_string(), Value::from(timestamp));
                    let message = AgentMessage::custom("custom", fields)
                        .map_err(|error| error.to_string())?;
                    if trigger_turn {
                        agent_for_message.follow_up(message);
                    } else {
                        agent_for_message.append_message(message);
                    }
                    Ok(())
                })),
                append_entry: Some(Arc::new(move |custom_type, data| {
                    let Some(disk) = session_for_entry.disk() else {
                        return Err("Session does not support custom entries".to_string());
                    };
                    disk.lock().append_custom_entry(&custom_type, data)
                })),
                send_user_message: Some(Arc::new(move |content, deliver_as| {
                    let text = match content {
                        Value::String(s) => s,
                        other => serde_json::to_string(&other).unwrap_or_default(),
                    };
                    let message = AgentMessage::user(text);
                    match deliver_as.as_deref() {
                        Some("steer") => agent_for_user.steer(message),
                        Some("followUp") => agent_for_user.follow_up(message),
                        _ => {
                            let _ = session_for_user.try_append_message(&message);
                            agent_for_user.append_message(message);
                        }
                    }
                    Ok(())
                })),
                set_session_name: Some(Arc::new(move |name| {
                    let Some(disk) = session_for_set_name.disk() else {
                        return Err("Session does not support session names".to_string());
                    };
                    disk.lock().append_session_info(&name).map(|_| ())
                })),
                get_session_name: Some(Arc::new(move || {
                    session_for_get_name
                        .disk()
                        .and_then(|disk| disk.lock().get_session_name().map(str::to_owned))
                })),
                set_label: Some(Arc::new(move |target_id, label| {
                    let Some(disk) = session_for_label.disk() else {
                        return Err("Session does not support labels".to_string());
                    };
                    disk.lock()
                        .append_label_change(&target_id, label.as_deref())
                        .map(|_| ())
                })),
                get_settings: Some(Arc::new(move || settings_for_host.raw_value())),
                get_commands: Some(Arc::new(move || {
                    let mut cmds = Vec::new();
                    for cmd in super::slash_commands::BUILTIN_SLASH_COMMANDS {
                        cmds.push(json!({
                            "name": cmd.name,
                            "description": cmd.description,
                            "source": "builtin",
                        }));
                    }
                    if let Some(runner) = runner_for_commands.upgrade() {
                        for cmd in runner.get_registered_commands() {
                            cmds.push(json!({
                                "name": cmd.invocation_name,
                                "description": cmd.definition.description,
                                "source": "extension",
                            }));
                        }
                    }
                    Value::Array(cmds)
                })),
                set_model: Some(Arc::new(move |provider, model_id| {
                    let Some(model) = registry_for_model.registry().get(&provider, &model_id)
                    else {
                        return Err(format!("Model not found: {provider}/{model_id}"));
                    };
                    agent_set_model.set_model(model);
                    Ok(())
                })),
                get_thinking_level: Some(Arc::new(move || {
                    agent_thinking.thinking_level().as_str().to_owned()
                })),
                set_thinking_level: Some(Arc::new(move |level| {
                    let parsed = parse_thinking_level(&level)
                        .ok_or_else(|| format!("Invalid thinking level: {level}"))?;
                    agent_set_thinking.set_thinking_level(parsed);
                    Ok(())
                })),
            });
    }
    let compactor = Arc::new(ModelCompactor {
        runtime: model_runtime.clone(),
        settings: settings_store,
    });
    let extensions = Arc::new(NativeExtensions { runner, resources });
    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager: target.session_manager,
        settings: settings.clone(),
        auth: model_runtime.clone(),
        models: model_runtime.clone(),
        compactor: compactor.clone(),
        extensions: extensions.clone(),
        cwd: target.cwd.clone(),
        retry_prompt: None,
    })
    .map_err(|error| error.to_string())?
    .with_system_prompt_sections(system_prompt)
    .with_nested_runner(nested_runner);
    Ok(CreateAgentSessionRuntimeResult {
        session,
        services: RuntimeServices {
            cwd: target.cwd,
            agent_dir: target.agent_dir,
            settings,
            auth: model_runtime.clone(),
            models: model_runtime,
            compactor,
            extensions,
        },
        diagnostics,
        model_fallback_message: fallback,
    })
}

fn configured_extensions(
    options: &LoadOptions,
    resources: &crate::modes::interactive::selectors::ConfiguredResources,
) -> Vec<String> {
    let mut paths = loader::discover_extension_paths(options);
    paths.retain(|path| {
        let path = Path::new(path);
        options
            .cli_paths
            .iter()
            .any(|explicit| path.starts_with(explicit))
            || resources.enabled(ResourceKind::Extension, path, true)
    });
    if !options.no_extensions {
        for path in resources.enabled_paths(ResourceKind::Extension) {
            if path
                .extension()
                .is_some_and(|extension| extension == "wasm")
            {
                let path = path.to_string_lossy().into_owned();
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn included_resource(
    resources: &crate::modes::interactive::selectors::ConfiguredResources,
    kind: ResourceKind,
    path: &Path,
    cli: &[PathBuf],
    no_defaults: bool,
) -> bool {
    cli.iter().any(|explicit| path.starts_with(explicit))
        || !no_defaults && resources.enabled(kind, path, true)
}

fn read_prompt(value: Option<&str>, cwd: &Path) -> Result<Option<String>, String> {
    let Some(value) = value else { return Ok(None) };
    let path = config::resolve_path(value, cwd, false)?;
    if path.is_file() {
        std::fs::read_to_string(path)
            .map(Some)
            .map_err(|error| error.to_string())
    } else {
        Ok(Some(value.to_owned()))
    }
}

fn select_model(
    args: &Args,
    settings: &Value,
    context: Option<&super::session_manager::SessionContext>,
    runtime: &mut ModelRuntime,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) -> (Option<Model>, ThinkingLevel, Option<String>) {
    let models = runtime.registry().list();
    let patterns = args.models.clone().or_else(|| {
        settings["enabledModels"].as_array().map(|patterns| {
            patterns
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
    });
    let scope = resolve_model_scope_from_models(patterns.as_deref().unwrap_or_default(), &models);
    diagnostics.extend(
        scope
            .diagnostics
            .into_iter()
            .map(|issue| RuntimeDiagnostic::warning(issue.message)),
    );
    if let Some(provider) = args.provider.as_deref()
        && args.model.is_none()
    {
        diagnostics.push(error(format!(
            "--provider requires --model (for example: --provider {provider} --model <pattern>)"
        )));
    }
    let resolved = resolve_cli_model(&CliModelOptions {
        cli_provider: args.provider.as_deref(),
        cli_model: args.model.as_deref(),
        cli_thinking: args.thinking,
        inline_api_key: args.api_key.is_some(),
        models: &models,
        has_configured_auth: &|provider| runtime.has_auth(provider),
    });
    diagnostics.extend(resolved.warning.map(RuntimeDiagnostic::warning));
    diagnostics.extend(resolved.error.map(error));
    let default_provider = settings["defaultProvider"].as_str();
    let default_model_id = settings["defaultModel"].as_str();
    let is_continuing = context.is_some_and(|ctx| !ctx.messages.is_empty());
    let scoped: &[ScopedModel] = &scope.scoped_models;
    let preferred_scope = (!is_continuing)
        .then(|| {
            scoped
                .iter()
                .find(|scoped| {
                    Some(scoped.model.provider.as_str()) == default_provider
                        && Some(scoped.model.id.as_str()) == default_model_id
                })
                .or_else(|| scoped.first())
        })
        .flatten();
    let explicit = resolved
        .model
        .or_else(|| preferred_scope.map(|scope| scope.model.clone()));
    if let Some(key) = &args.api_key {
        if let Some(model) = &explicit {
            runtime.set_runtime_api_key(&model.provider, key.clone());
        } else {
            diagnostics.push(error("--api-key requires a model to be specified via --model, --provider/--model, or --models"));
        }
    }
    let default_thinking_level = settings["defaultThinkingLevel"]
        .as_str()
        .and_then(parse_thinking_level);
    let levels: Option<HashMap<String, ThinkingLevel>> =
        settings["modelThinkingLevels"].as_object().map(|levels| {
            levels
                .iter()
                .filter_map(|(key, level)| {
                    Some((key.clone(), parse_thinking_level(level.as_str()?)?))
                })
                .collect()
        });
    let initial = find_initial_model(&InitialModelOptions {
        scoped_models: scoped,
        is_continuing,
        default_provider,
        default_model_id,
        default_thinking_level,
        model_thinking_levels: levels.as_ref(),
        models: &models,
        available: runtime.available(),
        has_configured_auth: &|provider| runtime.has_auth(provider),
    });
    let restored = context
        .filter(|ctx| !ctx.messages.is_empty())
        .and_then(|ctx| ctx.model.as_ref());
    let restored_model = restored.and_then(|name| {
        runtime
            .has_auth(&name.provider)
            .then(|| runtime.registry().get(&name.provider, &name.model_id))
            .flatten()
    });
    let model = explicit.or(restored_model).or(initial.model);
    let thinking = args
        .thinking
        .or(resolved.thinking_level)
        .or_else(|| preferred_scope.and_then(|scope| scope.thinking_level))
        .or_else(|| {
            context
                .filter(|ctx| !ctx.messages.is_empty())
                .and_then(|ctx| parse_thinking_level(&ctx.thinking_level))
        })
        .or_else(|| {
            model.as_ref().and_then(|model| {
                levels
                    .as_ref()?
                    .get(&format!("{}/{}", model.provider, model.id))
                    .copied()
            })
        })
        .or(default_thinking_level)
        .unwrap_or(initial.thinking_level);
    let thinking = model
        .as_ref()
        .map_or(thinking, |model| model.clamp_thinking_level(thinking));
    let fallback = restored.and_then(|name| {
        let model = model.as_ref()?;
        (args.model.is_none() && (model.provider != name.provider || model.id != name.model_id))
            .then(|| {
                format!(
                    "Could not restore model {}/{}. Using {}/{}.",
                    name.provider, name.model_id, model.provider, model.id
                )
            })
    });
    diagnostics.extend(fallback.as_ref().map(RuntimeDiagnostic::warning));
    runtime.set_scope(scope.scoped_models);
    (model, thinking, fallback)
}

struct ExtensionAgentTool {
    declaration: pi_rs_ai::Tool,
    label: String,
    extension: Arc<dyn super::extensions::types::Extension>,
    host: Arc<ExtensionHost>,
    bus: Arc<EventBus>,
    cwd: String,
    mode: super::extensions::types::ExtensionMode,
    nested_runner: Option<Arc<super::nested_tool_calls::NestedToolCallRunner>>,
}

impl AgentTool for ExtensionAgentTool {
    fn declaration(&self) -> &pi_rs_ai::Tool {
        &self.declaration
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a pi_rs_agent::CancellationToken,
        on_update: pi_rs_agent::AgentToolUpdateCallback,
    ) -> futures_util::future::BoxFuture<'a, pi_rs_agent::Result<pi_rs_agent::AgentToolResult>>
    {
        let name = self.declaration.name.clone();
        let params = params.clone();
        let tool_call_id = tool_call_id.to_owned();
        let signal = signal.clone();
        let extension = Arc::clone(&self.extension);
        let host = Arc::clone(&self.host);
        let bus = Arc::clone(&self.bus);
        let cwd = self.cwd.clone();
        let mode = self.mode;
        let nested_runner = self.nested_runner.clone();
        Box::pin(async move {
            let base = super::extensions::types::ExtensionContext {
                host,
                cwd,
                mode,
                ui: super::extensions::types::ExtensionUi::no_op(mode),
                bus: Some(bus),
                is_idle: Arc::new(|| false),
                is_project_trusted: Arc::new(|| true),
                has_pending_messages: Arc::new(|| false),
                get_context_usage: Arc::new(|| None),
                get_system_prompt: Arc::new(String::new),
                abort: Arc::new(|| {}),
                shutdown: Arc::new(|| {}),
                compact: Arc::new(|_| {}),
            };
            let mut ctx = super::extensions::types::ExtensionToolContext::new(base, &tool_call_id);
            if let Some(nested) = nested_runner {
                let signal_for_nested = signal.clone();
                let on_update_for_nested = on_update.clone();
                ctx = ctx.with_execute_tool(Arc::new(move |caller_id, target_name, args, opts| {
                    let nested = Arc::clone(&nested);
                    let sig = signal_for_nested.clone();
                    let cb = on_update_for_nested.clone();
                    Box::pin(async move {
                        let cb_ref: Option<&(dyn Fn(pi_rs_agent::AgentToolResult) + Send + Sync)> =
                            if opts.deliver_as_update {
                                Some(&*cb)
                            } else {
                                None
                            };
                        nested
                            .execute(
                                &caller_id,
                                &target_name,
                                args,
                                Some(&sig),
                                opts.deliver_as_update,
                                cb_ref,
                            )
                            .await
                    })
                }));
            }
            let outcome = extension
                .execute_tool(&name, params, &ctx)
                .await
                .map_err(pi_rs_agent::AgentError::Callback)?;
            let mut result = pi_rs_agent::AgentToolResult::default();
            if let Some(content) = outcome.result.get("content")
                && let Ok(parsed) =
                    serde_json::from_value::<Vec<pi_rs_ai::InputContent>>(content.clone())
            {
                result.content = parsed;
            }
            if let Some(details) = outcome.result.get("details").filter(|v| !v.is_null()) {
                result.details = Some(details.clone());
            }
            if let Some(structured) = outcome
                .result
                .get("structuredContent")
                .filter(|v| !v.is_null())
            {
                result.structured_content = Some(structured.clone());
            }
            if let Some(usage) = outcome.result.get("usage").filter(|v| !v.is_null())
                && let Ok(parsed) = serde_json::from_value::<pi_rs_ai::Usage>(usage.clone())
            {
                result.usage = Some(parsed);
            }
            if outcome.is_error {
                result.is_error = Some(true);
            }
            Ok(result)
        })
    }
}

fn collect_session_tools(
    builtin_tools: &[Arc<dyn AgentTool>],
    runner: &ExtensionRunner,
    nested_runner: Option<&Arc<super::nested_tool_calls::NestedToolCallRunner>>,
    args: &Args,
) -> Vec<Arc<dyn AgentTool>> {
    collect_tools_with_filter(builtin_tools, runner, nested_runner, args, true)
}

fn collect_all_extension_tools(
    builtin_tools: &[Arc<dyn AgentTool>],
    runner: &ExtensionRunner,
    nested_runner: Option<&Arc<super::nested_tool_calls::NestedToolCallRunner>>,
    args: &Args,
) -> Vec<Arc<dyn AgentTool>> {
    collect_tools_with_filter(builtin_tools, runner, nested_runner, args, false)
}

fn collect_tools_with_filter(
    builtin_tools: &[Arc<dyn AgentTool>],
    runner: &ExtensionRunner,
    nested_runner: Option<&Arc<super::nested_tool_calls::NestedToolCallRunner>>,
    args: &Args,
    direct_only: bool,
) -> Vec<Arc<dyn AgentTool>> {
    let mut tools = builtin_tools.to_vec();
    if !args.no_tools {
        for extension in runner.extensions() {
            let mut provided = std::collections::HashSet::new();
            for tool in extension.agent_tools() {
                let name = tool.declaration().name.clone();
                provided.insert(name.clone());
                if args
                    .tools
                    .as_ref()
                    .is_none_or(|patterns| patterns.iter().any(|p| tool_name_matches(p, &name)))
                    && args.exclude_tools.as_ref().is_none_or(|patterns| {
                        !patterns.iter().any(|p| tool_name_matches(p, &name))
                    })
                {
                    tools.push(tool);
                }
            }
            for (name, def) in &extension.registration().tools {
                if provided.contains(name) {
                    continue;
                }
                if direct_only
                    && !matches!(
                        def.exposure,
                        super::extensions::types::ToolExposure::Direct
                            | super::extensions::types::ToolExposure::ModelOnly
                    )
                {
                    continue;
                }
                if args
                    .tools
                    .as_ref()
                    .is_none_or(|patterns| patterns.iter().any(|p| tool_name_matches(p, name)))
                    && args
                        .exclude_tools
                        .as_ref()
                        .is_none_or(|patterns| !patterns.iter().any(|p| tool_name_matches(p, name)))
                {
                    let label = if def.label.is_empty() {
                        def.name.clone()
                    } else {
                        def.label.clone()
                    };
                    tools.push(Arc::new(ExtensionAgentTool {
                        declaration: pi_rs_ai::Tool {
                            name: def.name.clone(),
                            description: def.description.clone(),
                            parameters: def.parameters.clone(),
                            constrained_sampling: None,
                        },
                        label,
                        extension: Arc::clone(&extension),
                        host: Arc::clone(&runner.host),
                        bus: Arc::clone(&runner.bus),
                        cwd: runner.cwd.clone(),
                        mode: runner.mode(),
                        nested_runner: nested_runner.cloned(),
                    }));
                }
            }
        }
    }
    tools
}

fn tool_name_matches(pattern: &str, name: &str) -> bool {
    let Some((prefix, rest)) = pattern.split_once('*') else {
        return name == pattern;
    };
    let (interior, suffix) = rest.rsplit_once('*').unwrap_or(("", rest));
    let Some(mut remaining) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
    else {
        return false;
    };
    for part in interior.split('*') {
        let Some((_, after)) = remaining.split_once(part) else {
            return false;
        };
        remaining = after;
    }
    true
}

fn selected_tools(cwd: &Path, args: &Args, settings: &SettingsManager) -> Vec<Arc<dyn AgentTool>> {
    const ALL: [&str; 8] = [
        "read",
        "bash",
        "powershell",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
    ];
    let allowlist = args.tools.as_deref().filter(|tools| {
        !tools
            .iter()
            .any(|name| super::settings_manager::is_tool_modifier(name))
    });
    let enabled = match allowlist {
        Some(tools) => Cow::Borrowed(tools),
        None => {
            let configured = if args.no_tools || args.no_builtin_tools {
                Vec::new()
            } else {
                settings.get_default_tools().unwrap_or_else(|| {
                    DEFAULT_TOOL_NAMES
                        .iter()
                        .map(|tool| (*tool).to_owned())
                        .collect()
                })
            };
            Cow::Owned(match &args.tools {
                Some(modifiers) => apply_tool_modifiers(&configured, modifiers),
                None => configured,
            })
        }
    };
    let expanded = ALL.into_iter().filter(|name| {
        allowlist.is_some_and(|patterns| {
            patterns
                .iter()
                .any(|pattern| tool_name_matches(pattern, name))
        })
    });
    let mut seen = [false; ALL.len()];
    let auto_resize = settings.get()["images"]["autoResize"]
        .as_bool()
        .unwrap_or(true);
    let mutation_queue = Arc::new(super::tools::file_mutation_queue::FileMutationQueue::new());
    enabled
        .iter()
        .map(String::as_str)
        .chain(expanded)
        .filter(|name| {
            let Some(index) = ALL.iter().position(|tool| tool == name) else {
                return false;
            };
            !std::mem::replace(&mut seen[index], true)
                && args.exclude_tools.as_ref().is_none_or(|patterns| {
                    !patterns
                        .iter()
                        .any(|pattern| tool_name_matches(pattern, name))
                })
        })
        .filter_map(|name| {
            let tool: Arc<dyn AgentTool> = match name {
                "read" => Arc::new(ReadTool::new(cwd).with_auto_resize_images(auto_resize)),
                "bash" => Arc::new(BashTool::new(cwd)),
                "powershell" => Arc::new(PowerShellTool::new(cwd)),
                "edit" => Arc::new(EditTool::with_queue(cwd, mutation_queue.clone())),
                "write" => Arc::new(WriteTool::with_queue(cwd, mutation_queue.clone())),
                "grep" => Arc::new(GrepTool::new(cwd)),
                "find" => Arc::new(FindTool::new(cwd)),
                "ls" => Arc::new(LsTool::new(cwd)),
                _ => return None,
            };
            Some(tool)
        })
        .collect()
}

pub fn export_to_html(
    manager: &SessionManager,
    output: Option<&str>,
    system_prompt: Option<String>,
) -> Result<String, String> {
    let file = manager
        .get_session_file()
        .ok_or("Cannot export in-memory session to HTML")?;
    if !file.exists() {
        return Err("Nothing to export yet - start a conversation first".to_owned());
    }
    let output = output.filter(|path| !path.is_empty()).map_or_else(
        || {
            format!(
                "pi-session-{}.html",
                file.file_stem().unwrap_or_default().to_string_lossy()
            )
        },
        |path| config::normalize_path(path, true),
    );
    let snapshot = SessionSnapshot {
        header: serde_json::to_value(manager.get_header()).map_err(|error| error.to_string())?,
        entries: manager
            .get_entries()
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?,
        leaf_id: manager.get_leaf_id().map(str::to_owned),
        system_prompt,
        ..SessionSnapshot::default()
    };
    let html = export_session_html(&snapshot, &ExportOptions::default())?;
    std::fs::write(&output, html).map_err(|error| format!("{}: {output}", error))?;
    Ok(output)
}

fn build_agent_hooks(runner: &Arc<ExtensionRunner>) -> pi_rs_agent::AgentHooks {
    let before_runner = Arc::clone(runner);
    let after_runner = Arc::clone(runner);
    let context_runner = Arc::clone(runner);
    pi_rs_agent::AgentHooks {
        transform_context: Some(Arc::new(move |messages, _signal| {
            let runner = Arc::clone(&context_runner);
            let owned = messages.to_vec();
            Box::pin(async move {
                if !runner.has_handlers(ExtensionEvent::Context)
                    && !runner.has_handlers(ExtensionEvent::ContextWithSystem)
                {
                    return owned;
                }
                let encoded = owned
                    .iter()
                    .filter_map(|message| serde_json::to_value(message).ok())
                    .collect::<Vec<_>>();
                let transformed = runner.emit_context(encoded).await;
                let parsed = transformed
                    .into_iter()
                    .map(serde_json::from_value::<AgentMessage>)
                    .collect::<std::result::Result<Vec<_>, _>>();
                parsed.unwrap_or(owned)
            })
        })),
        tool_calls: pi_rs_agent::ToolCallHooks {
            before_tool_call: Some(Arc::new(move |ctx, _signal| {
                let runner = Arc::clone(&before_runner);
                let tool_call_id = ctx.tool_call.id.clone();
                let tool_name = ctx.tool_call.name.clone();
                let args_val = ctx.args.clone();
                Box::pin(async move {
                    if !runner.has_handlers(ExtensionEvent::ToolCall) {
                        return Ok(None);
                    }
                    let event = json!({
                        "type": "tool_call",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "input": args_val,
                    });
                    let Some(decision) = runner.emit_tool_call(event).await else {
                        return Ok(None);
                    };
                    if let Some(updated_input) = decision.get("input").filter(|v| !v.is_null()) {
                        *ctx.args = updated_input.clone();
                    }
                    if decision.get("block").and_then(Value::as_bool) == Some(true) {
                        return Ok(Some(pi_rs_agent::BeforeToolCallResult {
                            block: true,
                            reason: decision
                                .get("reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            terminate: decision
                                .get("terminate")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        }));
                    }
                    Ok(None)
                })
            })),
            after_tool_call: Some(Arc::new(move |ctx, _signal| {
                let runner = Arc::clone(&after_runner);
                let tool_call_id = ctx.tool_call.id.clone();
                let tool_name = ctx.tool_call.name.clone();
                let input = ctx.args.clone();
                let content = ctx.result.content.clone();
                let details = ctx.result.details.clone();
                let structured_content = ctx.result.structured_content.clone();
                let is_error = ctx.is_error;
                let usage = ctx.result.usage;
                Box::pin(async move {
                    if !runner.has_handlers(ExtensionEvent::ToolResult) {
                        return Ok(None);
                    }
                    let event = json!({
                        "type": "tool_result",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "input": input,
                        "content": content,
                        "details": details,
                        "structuredContent": structured_content,
                        "isError": is_error,
                        "usage": usage,
                    });
                    let Some(modified) = runner.emit_tool_result(event).await else {
                        return Ok(None);
                    };
                    let mut result = pi_rs_agent::AfterToolCallResult::default();
                    if let Some(value) = modified.get("content")
                        && let Ok(parsed) =
                            serde_json::from_value::<Vec<pi_rs_ai::InputContent>>(value.clone())
                    {
                        result.content = Some(parsed);
                    }
                    if let Some(value) = modified.get("details").filter(|v| !v.is_null()) {
                        result.details = Some(value.clone());
                    }
                    if let Some(value) = modified.get("structuredContent").filter(|v| !v.is_null())
                    {
                        result.structured_content = Some(value.clone());
                    }
                    if let Some(err) = modified.get("isError").and_then(Value::as_bool) {
                        result.is_error = Some(err);
                    }
                    if let Some(value) = modified.get("usage").filter(|v| !v.is_null())
                        && let Ok(parsed) = serde_json::from_value::<pi_rs_ai::Usage>(value.clone())
                    {
                        result.usage = Some(parsed);
                    }
                    Ok(Some(result))
                })
            })),
        },
        ..pi_rs_agent::AgentHooks::default()
    }
}

fn resolve_active_tools(
    available: &[Arc<dyn AgentTool>],
    names: &[String],
) -> Result<Vec<Arc<dyn AgentTool>>, String> {
    let mut selected = Vec::with_capacity(names.len());
    let mut seen_valid = std::collections::HashSet::with_capacity(names.len());
    let mut unknown = Vec::new();
    let mut seen_unknown = std::collections::HashSet::new();
    for name in names {
        let name_str = name.as_str();
        if let Some(tool) = available
            .iter()
            .find(|tool| tool.declaration().name == name_str)
        {
            if seen_valid.insert(name_str) {
                selected.push(tool.clone());
            }
        } else if seen_unknown.insert(name_str) {
            unknown.push(name_str);
        }
    }
    if !unknown.is_empty() {
        return Err(format!("Unknown tool(s): {}", unknown.join(", ")));
    }
    Ok(selected)
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn explicit_tools_override_disabled_defaults_and_globs_respect_denylist() -> Result<(), String>
    {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let settings =
            SettingsManager::create_with_dirs(dir.path(), dir.path(), CreateOptions::default());
        let args = Args {
            no_tools: true,
            tools: Some(["write", "*e*", "read", "b?sh"].map(str::to_owned).to_vec()),
            exclude_tools: Some(vec!["p*sh*".to_owned()]),
            ..Args::default()
        };
        let tools = selected_tools(dir.path(), &args, &settings);
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool.declaration().name.as_str())
            .collect();
        assert_eq!(names, ["write", "read", "edit", "grep"]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tools(dir: &Path) -> Vec<Arc<dyn AgentTool>> {
        vec![
            Arc::new(ReadTool::new(dir)),
            Arc::new(BashTool::new(dir)),
            Arc::new(WriteTool::new(dir)),
        ]
    }

    #[test]
    fn resolve_active_tools_preserves_order_and_deduplicates() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let available = sample_tools(dir.path());
        let requested = ["write", "read", "write", "bash", "read"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let resolved = resolve_active_tools(&available, &requested)?;
        let names: Vec<_> = resolved
            .iter()
            .map(|tool| tool.declaration().name.as_str())
            .collect();
        assert_eq!(names, ["write", "read", "bash"]);
        Ok(())
    }

    #[test]
    fn resolve_active_tools_rejects_unknown_names_without_silent_drop() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let available = sample_tools(dir.path());
        let requested = ["read", "missing_one", "bash", "missing_one", "missing_two"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let err = match resolve_active_tools(&available, &requested) {
            Ok(_) => return Err("unknown tool names must return an error".to_string()),
            Err(err) => err,
        };
        assert_eq!(err, "Unknown tool(s): missing_one, missing_two");
        Ok(())
    }

    #[tokio::test]
    async fn build_agent_hooks_routes_before_after_and_context_through_runner() -> Result<(), String>
    {
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus.clone()));
        let runner = Arc::new(ExtensionRunner::new(Vec::new(), host, bus, "."));
        runner.bus.on(ExtensionEvent::ToolCall, |event| {
            Box::pin(async move {
                let name = event["toolName"].as_str().unwrap_or_default();
                if name == "blocked" {
                    return Ok(Some(json!({ "block": true, "reason": "denied" })));
                }
                Ok(Some(json!({ "input": { "command": "rewritten" } })))
            })
        });
        runner.bus.on(ExtensionEvent::ToolResult, |_event| {
            Box::pin(async move {
                Ok(Some(json!({
                    "content": [{ "type": "text", "text": "hooked-output" }],
                    "isError": true
                })))
            })
        });
        runner.bus.on(ExtensionEvent::Context, |event| {
            Box::pin(async move {
                let mut messages = event["messages"].as_array().cloned().unwrap_or_default();
                messages.push(
                    serde_json::to_value(AgentMessage::user("from-hook")).unwrap_or(Value::Null),
                );
                Ok(Some(json!({ "messages": messages })))
            })
        });
        let hooks = build_agent_hooks(&runner);
        let signal = pi_rs_agent::CancellationToken::new();
        let assistant = pi_rs_ai::faux::faux_assistant_message(Vec::new());
        let ctx = pi_rs_agent::AgentContext::default();

        let before = hooks
            .tool_calls
            .before_tool_call
            .as_ref()
            .ok_or("missing before_tool_call")?;
        let mut ok_args = json!({ "command": "orig" });
        let ok_call = pi_rs_ai::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::Map::from_iter([("command".to_owned(), json!("orig"))]),
            ..Default::default()
        };
        let decision = before(
            pi_rs_agent::BeforeToolCallContext {
                assistant_message: &assistant,
                tool_call: &ok_call,
                args: &mut ok_args,
                context: &ctx,
            },
            &signal,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(decision.is_none());
        assert_eq!(ok_args, json!({ "command": "rewritten" }));

        let mut blocked_args = json!({});
        let blocked_call = pi_rs_ai::ToolCall {
            id: "c2".into(),
            name: "blocked".into(),
            arguments: serde_json::Map::new(),
            ..Default::default()
        };
        let blocked = before(
            pi_rs_agent::BeforeToolCallContext {
                assistant_message: &assistant,
                tool_call: &blocked_call,
                args: &mut blocked_args,
                context: &ctx,
            },
            &signal,
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or("expected block result")?;
        assert!(blocked.block);
        assert_eq!(blocked.reason.as_deref(), Some("denied"));

        let after = hooks
            .tool_calls
            .after_tool_call
            .as_ref()
            .ok_or("missing after_tool_call")?;
        let result = pi_rs_agent::AgentToolResult::text("raw");
        let after_res = after(
            pi_rs_agent::AfterToolCallContext {
                assistant_message: &assistant,
                tool_call: &ok_call,
                args: &ok_args,
                result: &result,
                is_error: false,
                context: &ctx,
            },
            &signal,
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or("expected after result")?;
        assert_eq!(after_res.is_error, Some(true));
        let txt = match after_res.content.and_then(|c| c.into_iter().next()) {
            Some(pi_rs_ai::InputContent::Text(t)) => t.text,
            _ => String::new(),
        };
        assert_eq!(txt, "hooked-output");

        let transform = hooks
            .transform_context
            .as_ref()
            .ok_or("missing transform_context")?;
        let transformed = transform(&[AgentMessage::user("hi")], &signal).await;
        assert_eq!(transformed.len(), 2);
        Ok(())
    }
}
