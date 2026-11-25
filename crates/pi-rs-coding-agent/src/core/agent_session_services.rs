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
}

struct DiskSettings(Mutex<SettingsManager>);

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
}

struct ModelCompactor {
    runtime: Arc<ModelRuntime>,
    keep_recent_tokens: usize,
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
            let instructions = (!instructions.is_empty()).then_some(instructions);
            let context = compaction::CompactionContext::new(messages);
            let options = compaction::CompactionOptions {
                keep_recent_tokens: self.keep_recent_tokens,
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

    let mut model_runtime = ModelRuntime::create(&agent_dir, &runner, &mut diagnostics).await?;
    if args.list_models.is_some()
        && let Some(load_error) = model_runtime.registry().get_error()
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
    let builtin_tools = selected_tools(&cwd, args, &settings);
    let tools = collect_session_tools(&builtin_tools, &runner, args);
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
    let model_runtime = Arc::new(model_runtime);
    let stream_fn = model_runtime.stream();
    let settings: Arc<dyn SettingsStore> = Arc::new(DiskSettings(Mutex::new(settings)));
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model,
            thinking_level: Some(thinking),
            tools: Vec::new(),
            system_prompt: None,
            messages: target.session_manager.messages(),
        },
        stream_fn: Some(stream_fn),
        stream_options: StreamOptions {
            session_id: Some(target.session_manager.session_id()),
            ..StreamOptions::default()
        },
        ..AgentOptions::default()
    })
    .map_err(|error| error.to_string())?;
    // Declare the initial loadout on the first prompt, not as hidden history.
    agent.set_tools(tools);
    {
        let agent = agent.clone();
        let runner_weak = Arc::downgrade(&runner);
        let args = args.clone();
        runner.host.set_refresh_tools(Arc::new(move || {
            if let Some(runner) = runner_weak.upgrade() {
                agent.set_tools(collect_session_tools(&builtin_tools, &runner, &args));
            }
        }));
    }
    let compactor = Arc::new(ModelCompactor {
        runtime: model_runtime.clone(),
        keep_recent_tokens: settings_value["compaction"]["keepRecentTokens"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS),
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
    .with_system_prompt_sections(system_prompt);
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

fn collect_session_tools(
    builtin_tools: &[Arc<dyn AgentTool>],
    runner: &ExtensionRunner,
    args: &Args,
) -> Vec<Arc<dyn AgentTool>> {
    let mut tools = builtin_tools.to_vec();
    if !args.no_tools {
        for extension in runner.extensions() {
            tools.extend(extension.agent_tools().into_iter().filter(|tool| {
                let name = &tool.declaration().name;
                args.tools
                    .as_ref()
                    .is_none_or(|patterns| patterns.iter().any(|p| tool_name_matches(p, name)))
                    && args
                        .exclude_tools
                        .as_ref()
                        .is_none_or(|patterns| !patterns.iter().any(|p| tool_name_matches(p, name)))
            }));
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
                "edit" => Arc::new(EditTool::new(cwd)),
                "write" => Arc::new(WriteTool::new(cwd)),
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
