//! Extension discovery, trust-gated loading, replaceability, and `/reload`.
//!
//! The TypeScript host loads ES modules through `jiti`; the Rust host receives
//! `ExtensionFactory` values from the native registry instead. Everything else
//! (discovery rules, transactions, reload ordering) is ported verbatim.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::core::event_bus::EventBus;
use crate::core::skills::ResourceDiagnostic;

use super::runner::ExtensionRunner;
use super::types::{
    Extension, ExtensionError, ExtensionFactory, ExtensionInitContext, ExtensionRegistration,
    ExtensionSetup, FlagValue, LoadExtensionsResult, ToolDefinition,
};

pub const BUILTIN_PATH_PREFIX: &str = "builtin:";

/// Where extension factories come from. Paths are the same logical paths the
/// TypeScript loader uses; builtins are keyed as `builtin:<name>`.
#[derive(Default)]
pub struct ExtensionCatalog {
    pub factories: IndexMap<String, ExtensionFactory>,
    pub builtins: IndexMap<String, (ExtensionFactory, bool)>,
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub config_dir_name: String,
    pub cli_paths: Vec<PathBuf>,
    pub project_trusted: bool,
    pub no_extensions: bool,
    pub disabled_builtins: BTreeSet<String>,
}

impl LoadOptions {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            config_dir_name: ".pi".to_string(),
            cli_paths: Vec::new(),
            project_trusted: false,
            no_extensions: false,
            disabled_builtins: BTreeSet::new(),
        }
    }

    fn global_dir(&self) -> PathBuf {
        self.agent_dir.join("extensions")
    }

    fn project_dir(&self) -> PathBuf {
        self.cwd.join(&self.config_dir_name).join("extensions")
    }
}

/// A loaded set plus the logical path keys of its extensions, used to avoid
/// instantiating the same extension twice across the two trust passes.
#[derive(Clone, Default)]
pub struct LoadedExtensionSet {
    pub result: LoadExtensionsResult,
    pub path_keys: Vec<String>,
}

fn is_extension_file(name: &str) -> bool {
    name.ends_with(".wasm") || name.ends_with(".ts") || name.ends_with(".js")
}

/// Resolve the entry points of one extension directory: `package.json` with a
/// `pi.extensions` field first, then `index.ts`, then `index.js`.
pub fn resolve_extension_entries(dir: &Path) -> Vec<PathBuf> {
    let manifest_path = dir.join("package.json");
    if let Ok(contents) = fs::read_to_string(&manifest_path)
        && let Ok(manifest) = serde_json::from_str::<Value>(&contents)
        && let Some(entries) = manifest
            .get("pi")
            .and_then(|pi| pi.get("extensions"))
            .and_then(Value::as_array)
    {
        let resolved: Vec<PathBuf> = entries
            .iter()
            .filter_map(Value::as_str)
            .map(|entry| {
                let trimmed = entry.trim_start_matches('~').trim_start_matches('/');
                dir.join(trimmed)
            })
            .filter(|path| path.exists())
            .collect();
        if !resolved.is_empty() {
            return resolved;
        }
    }
    for candidate in ["index.wasm", "index.ts", "index.js"] {
        let path = dir.join(candidate);
        if path.exists() {
            return vec![path];
        }
    }
    Vec::new()
}

/// Discover extensions in one directory. Direct `.ts`/`.js` files and one
/// level of subdirectories with an entry point are considered; no deeper
/// recursion.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut discovered = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_file = entry
            .file_type()
            .is_ok_and(|kind| kind.is_file() || kind.is_symlink());
        if is_file && is_extension_file(&name) {
            discovered.push(path);
            continue;
        }
        if path.is_dir() {
            discovered.extend(resolve_extension_entries(&path));
        }
    }
    discovered.sort();
    discovered
}

/// Discover the ordered extension paths for a trust state.
pub fn discover_extension_paths(options: &LoadOptions) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |path: PathBuf| {
        let key = path.to_string_lossy().into_owned();
        if seen.insert(key.clone()) {
            paths.push(key);
        }
    };

    for path in &options.cli_paths {
        if path.is_dir() {
            let entries = resolve_extension_entries(path);
            if entries.is_empty() {
                for entry in discover_extensions_in_dir(path) {
                    add(entry);
                }
            } else {
                for entry in entries {
                    add(entry);
                }
            }
        } else {
            add(path.clone());
        }
    }
    if !options.no_extensions {
        for path in discover_extensions_in_dir(&options.global_dir()) {
            add(path);
        }
        if options.project_trusted {
            for path in discover_extensions_in_dir(&options.project_dir()) {
                add(path);
            }
        }
    }
    paths
}

/// Builtin paths are appended after file extensions, only after trust resolves
/// (project settings can disable them).
pub fn builtin_paths(names: &BTreeSet<String>) -> Vec<String> {
    let mut enabled: Vec<_> = names.iter().collect();
    enabled.sort();
    enabled
        .into_iter()
        .map(|name| format!("{BUILTIN_PATH_PREFIX}{name}"))
        .collect()
}

/// `omitReplacedExtensions`: a replaceable extension (typically a builtin) is
/// dropped when a non-replaceable extension registers any colliding tool,
/// command, or flag name. A warning explains how to re-enable it.
pub fn omit_replaced_extensions(
    extensions: Vec<Arc<dyn Extension>>,
    warnings: &mut Vec<ResourceDiagnostic>,
) -> Vec<Arc<dyn Extension>> {
    let mut taken = std::collections::BTreeSet::new();
    for extension in &extensions {
        if !extension.replaceable() {
            for name in extension.registration().names() {
                taken.insert(name);
            }
        }
    }
    extensions
        .into_iter()
        .filter(|extension| {
            if !extension.replaceable() {
                return true;
            }
            let Some(replacement) = extension
                .registration()
                .names()
                .into_iter()
                .find(|name| taken.contains(name))
            else {
                return true;
            };
            let path = extension.path();
            if let Some(builtin_name) = path.strip_prefix(BUILTIN_PATH_PREFIX) {
                let (kind, raw_name) = replacement.split_once(':').unwrap_or((replacement.as_str(), ""));
                let registered_name = match kind {
                    "command" => format!("/{raw_name}"),
                    "flag" => format!("--{raw_name}"),
                    _ => raw_name.to_string(),
                };
                warnings.push(ResourceDiagnostic::warning(
                    format!(
                        "Extension registers {kind} `{registered_name}`, so built-in extension `{builtin_name}` was not loaded. To use `{builtin_name}`, run `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing extension. We recommend only having one or the other loaded at a time."
                    ),
                    None,
                ));
            }
            false
        })
        .collect()
}

fn factory_for<'a>(catalog: &'a ExtensionCatalog, path: &str) -> Option<&'a ExtensionFactory> {
    catalog.factories.get(path).or_else(|| {
        path.strip_prefix(BUILTIN_PATH_PREFIX)
            .and_then(|name| catalog.builtins.get(name).map(|(factory, _)| factory))
    })
}

async fn load_paths(
    runner: &ExtensionRunner,
    catalog: &ExtensionCatalog,
    paths: &[String],
) -> (LoadExtensionsResult, Vec<String>) {
    // Keep the logical path key alongside its factory. An extension's
    // `path()` may be its display name, so it cannot be used to recognize an
    // already-loaded path in the second (post-trust) pass.
    let mut resolved: Vec<(String, ExtensionFactory)> = Vec::new();
    let mut skipped_warnings: Vec<ResourceDiagnostic> = Vec::new();
    for path in paths {
        if let Some(factory) = factory_for(catalog, path) {
            resolved.push((path.clone(), factory.clone()));
        } else if path.ends_with(".ts") || path.ends_with(".js") {
            skipped_warnings.push(ResourceDiagnostic::warning(
                format!(
                    "Skipped JavaScript/TypeScript extension `{path}`: pi-rs loads native and WASM/WIT extensions (`.wasm`). If you share `~/.pi/agent` with upstream pi, this is safe to ignore."
                ),
                Some(Path::new(path)),
            ));
        }
    }
    let factories: Vec<ExtensionFactory> = resolved
        .iter()
        .map(|(_, factory)| factory.clone())
        .collect();
    let mut result = runner.load_extensions(&factories).await;
    result.warnings.extend(skipped_warnings);
    let path_keys = resolved.into_iter().map(|(path, _)| path).collect();
    (result, path_keys)
}

/// Pre-trust pass: only global and CLI extensions load, with the restricted
/// `ProjectTrustContext` (cwd, mode, hasUI, and the four UI primitives).
pub async fn load_pre_trust(
    runner: &ExtensionRunner,
    catalog: &ExtensionCatalog,
    options: &LoadOptions,
) -> LoadedExtensionSet {
    let paths = discover_extension_paths(options);
    let (mut result, path_keys) = load_paths(runner, catalog, &paths).await;
    result.extensions = omit_replaced_extensions(result.extensions.clone(), &mut result.warnings);
    LoadedExtensionSet { result, path_keys }
}

/// Post-trust pass: project-local and builtin extensions are added; anything
/// already loaded before trust is reused rather than instantiated again.
pub async fn load_post_trust(
    runner: &ExtensionRunner,
    catalog: &ExtensionCatalog,
    options: &LoadOptions,
    pre_trust: &LoadedExtensionSet,
) -> LoadedExtensionSet {
    let mut paths = discover_extension_paths(options);
    for name in catalog.builtins.keys() {
        if !options.disabled_builtins.contains(name) {
            paths.push(format!("{BUILTIN_PATH_PREFIX}{name}"));
        }
    }
    let remaining: Vec<String> = paths
        .into_iter()
        .filter(|path| !pre_trust.path_keys.iter().any(|loaded| loaded == path))
        .collect();

    let (remaining_result, remaining_keys) = load_paths(runner, catalog, &remaining).await;
    let mut extensions = pre_trust.result.extensions.clone();
    extensions.extend(remaining_result.extensions.clone());

    let mut errors = pre_trust.result.errors.clone();
    errors.extend(remaining_result.errors.iter().cloned());
    let mut warnings = pre_trust.result.warnings.clone();
    warnings.extend(remaining_result.warnings.iter().cloned());
    extensions = omit_replaced_extensions(extensions, &mut warnings);

    let mut path_keys = pre_trust.path_keys.clone();
    path_keys.extend(remaining_keys);
    LoadedExtensionSet {
        result: LoadExtensionsResult {
            extensions,
            errors,
            warnings,
        },
        path_keys,
    }
}

/// The canonical `/reload` step list from
/// `rust/research/wasm-extensions.md` section 2.
pub const RELOAD_SEQUENCE: [&str; 9] = [
    "reject_while_busy",
    "capture_flags",
    "shutdown_and_invalidate",
    "reload_settings_and_discover",
    "preserve_tools",
    "bind_and_restore",
    "flush_providers",
    "emit_session_start",
    "emit_resources_discover",
];

#[derive(Clone)]
pub struct ReloadRequest<'a> {
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub old_runner: &'a ExtensionRunner,
    pub new_runner: &'a ExtensionRunner,
    pub factories: &'a [ExtensionFactory],
    pub previous_flag_values: IndexMap<String, FlagValue>,
    pub active_tools: Vec<String>,
    pub default_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReloadReport {
    pub steps: Vec<&'static str>,
    pub pending_tool_names: Vec<String>,
    pub flag_values_restored: IndexMap<String, FlagValue>,
    pub flushed_providers: usize,
    pub flushed_virtual_models: usize,
    pub load_errors: Vec<ExtensionError>,
}

/// Orchestrate a `/reload`. Strictly ordered; a busy agent rejects before any
/// side effects. Each step is recorded in the returned report so the sequence
/// is observable offline.
pub async fn reload(request: ReloadRequest<'_>) -> Result<ReloadReport, String> {
    if request.is_streaming || request.is_compacting {
        return Err("Cannot reload while the agent is streaming or compacting".to_string());
    }
    let mut steps = Vec::new();
    steps.push(RELOAD_SEQUENCE[0]);

    steps.push(RELOAD_SEQUENCE[1]);
    let previous = request.previous_flag_values.clone();

    steps.push(RELOAD_SEQUENCE[2]);
    request
        .old_runner
        .emit(
            crate::core::event_bus::ExtensionEvent::SessionShutdown,
            json!({ "reason": "reload" }),
        )
        .await;
    request.old_runner.invalidate();

    steps.push(RELOAD_SEQUENCE[3]);
    let loaded = request.new_runner.load_extensions(request.factories).await;

    steps.push(RELOAD_SEQUENCE[4]);
    let mut pending_tool_names = request.active_tools.clone();
    for tool in &request.default_tools {
        if !pending_tool_names.contains(tool) {
            pending_tool_names.push(tool.clone());
        }
    }

    steps.push(RELOAD_SEQUENCE[5]);
    request.new_runner.host.restore_flag_values(&previous);
    let flags_restored = request.new_runner.host.flag_values();

    steps.push(RELOAD_SEQUENCE[6]);
    let flushed_providers = request.new_runner.bind_provider_registrations().len();
    let flushed_virtual_models = request.new_runner.bind_virtual_models().len();
    request.new_runner.refresh_tools();

    steps.push(RELOAD_SEQUENCE[7]);
    request
        .new_runner
        .emit(
            crate::core::event_bus::ExtensionEvent::SessionStart,
            json!({ "reason": "reload" }),
        )
        .await;
    request.new_runner.report_unhandled_mcp_servers();

    steps.push(RELOAD_SEQUENCE[8]);
    request.new_runner.emit_resources_discover("reload").await;

    Ok(ReloadReport {
        steps,
        pending_tool_names,
        flag_values_restored: flags_restored,
        flushed_providers,
        flushed_virtual_models,
        load_errors: loaded.errors,
    })
}

/// Helper for building a `builtin:<name>` extension factory with explicit
/// replaceability, matching `builtin:llama.cpp` and `builtin:mcp`.
pub fn builtin_factory(
    name: impl Into<String>,
    replaceable: bool,
    factory: ExtensionFactory,
) -> (String, (ExtensionFactory, bool)) {
    (name.into(), (factory, replaceable))
}

/// A no-op bus/host triple for tests and embedders.
pub fn new_runtime() -> (Arc<ExtensionHostForLoader>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(ExtensionHostForLoader::new(Arc::clone(&bus)));
    (host, bus)
}

/// Re-export of the shared host under a loader-local name for call sites that
/// only need runtime construction.
pub use super::types::ExtensionHost as ExtensionHostForLoader;

/// Return the default CLI/config directories relative to a cwd and agent dir.
pub fn default_resource_dirs(options: &LoadOptions) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    (
        options.global_dir(),
        options.project_dir(),
        options.cwd.join(&options.config_dir_name).join("skills"),
        options.cwd.join(&options.config_dir_name).join("prompts"),
    )
}

/// Convenience: build a load result from already-loaded extensions.
pub fn loaded_result(extensions: Vec<Arc<dyn Extension>>) -> LoadExtensionsResult {
    LoadExtensionsResult {
        extensions,
        ..LoadExtensionsResult::default()
    }
}

/// Convenience: check whether a path is a builtin marker.
pub fn is_builtin_path(path: &str) -> bool {
    path.starts_with(BUILTIN_PATH_PREFIX)
}

/// A minimal extension implementation for loader tests and native built-ins.
pub struct NamedExtension {
    name: String,
    registration: ExtensionRegistration,
    setup: ExtensionSetup,
    replaceable: bool,
}

impl NamedExtension {
    /// Build an extension whose tool names are already registered, for tests
    /// and for built-ins that declare their registrations statically.
    pub fn with_tools(name: impl Into<String>, tools: &[&str]) -> Self {
        let name = name.into();
        let mut registration = ExtensionRegistration::new(name.clone());
        for tool in tools {
            registration.tools.insert(
                (*tool).to_string(),
                ToolDefinition::new(*tool, json!({"type":"object"})),
            );
        }
        Self {
            name,
            registration,
            setup: Arc::new(|_| Ok(())),
            replaceable: false,
        }
    }

    pub fn new(
        name: impl Into<String>,
        setup: impl Fn(&mut ExtensionInitContext<'_>) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        Self {
            registration: ExtensionRegistration::new(name.clone()),
            name,
            setup: Arc::new(setup),
            replaceable: false,
        }
    }

    pub fn replaceable(mut self, replaceable: bool) -> Self {
        self.replaceable = replaceable;
        self
    }
}

#[async_trait]
impl Extension for NamedExtension {
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &str {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::runner::ExtensionRunner;
    use crate::core::extensions::types::{FlagDefinition, FlagKind, ToolDefinition};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn extension(path: &str, replaceable: bool, tool: &str) -> ExtensionFactory {
        let path = Arc::new(path.to_string());
        let tool = Arc::new(tool.to_string());
        Arc::new(move || {
            let path = (*path).clone();
            let tool = Arc::clone(&tool);
            Ok(Box::new(
                NamedExtension::new(path, move |ctx| {
                    let tool = (*tool).clone();
                    ctx.register_tool(ToolDefinition::new(tool, json!({"type":"object"})))
                })
                .replaceable(replaceable),
            ))
        })
    }

    fn runner() -> ExtensionRunner {
        let (host, bus) = new_runtime();
        ExtensionRunner::new(Vec::new(), host, bus, "/work")
    }

    #[test]
    fn discovers_direct_files_and_one_level_subdirectories() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.ts"), "").expect("write a");
        fs::write(dir.path().join("b.js"), "").expect("write b");
        let sub = dir.path().join("nested");
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(sub.join("index.ts"), "").expect("write index");
        let deeper = sub.join("deeper");
        fs::create_dir_all(&deeper).expect("mkdir deeper");
        fs::write(deeper.join("index.js"), "").expect("write deep index");
        let found = discover_extensions_in_dir(dir.path());
        assert_eq!(found.len(), 3);
        assert!(
            found
                .iter()
                .all(|path| !path.to_string_lossy().contains("deeper"))
        );
    }

    #[test]
    fn package_manifest_entries_win_over_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("index.ts"), "").expect("write index");
        fs::write(
            dir.path().join("package.json"),
            r#"{"pi":{"extensions":["./custom.ts","./missing.ts"]}}"#,
        )
        .expect("write manifest");
        fs::write(dir.path().join("custom.ts"), "").expect("write custom");
        let entries = resolve_extension_entries(dir.path());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].ends_with("custom.ts"));
    }

    #[test]
    fn omit_replaced_drops_replaceable_builtin_with_warning() {
        let mut warnings = Vec::new();
        let extensions: Vec<Arc<dyn Extension>> = vec![
            Arc::new(NamedExtension::with_tools("user", &["mcp"])),
            Arc::new(NamedExtension::with_tools("builtin:mcp", &["mcp"]).replaceable(true)),
        ];
        let kept = omit_replaced_extensions(extensions, &mut warnings);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name(), "user");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .message
                .contains("built-in extension `mcp` was not loaded")
        );
    }

    #[test]
    fn omit_replaced_keeps_non_replaceable_collisions() {
        let extensions: Vec<Arc<dyn Extension>> = vec![
            Arc::new(NamedExtension::with_tools("a", &["shared"])),
            Arc::new(NamedExtension::with_tools("b", &["shared"])),
        ];
        assert_eq!(
            omit_replaced_extensions(extensions, &mut Vec::new()).len(),
            2
        );
    }

    #[tokio::test]
    async fn pre_trust_excludes_project_and_builtins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = LoadOptions::new(dir.path(), dir.path().join("agent"));
        let mut catalog = ExtensionCatalog::default();
        catalog.factories.insert(
            "global".to_string(),
            extension("global", false, "global-tool"),
        );
        catalog.factories.insert(
            "project".to_string(),
            extension("project", false, "project-tool"),
        );
        catalog.builtins.insert(
            "codemode".to_string(),
            (extension("builtin:codemode", true, "codemode"), true),
        );
        let runner = runner();
        // The catalog keys are logical paths; discovery only returns real
        // paths, so make the global dir contain a file that maps to "global".
        fs::create_dir_all(options.global_dir()).expect("mkdir");
        let global_file = options.global_dir().join("global.ts");
        fs::write(&global_file, "").expect("write");
        catalog.factories.insert(
            global_file.to_string_lossy().into_owned(),
            extension("global", false, "global-tool"),
        );

        let pre = load_pre_trust(&runner, &catalog, &options).await;
        assert_eq!(
            pre.path_keys,
            vec![global_file.to_string_lossy().into_owned()]
        );
    }

    #[tokio::test]
    async fn unregistered_upstream_ts_extension_is_skipped_with_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = LoadOptions::new(dir.path(), dir.path().join("agent"));
        fs::create_dir_all(options.global_dir()).expect("mkdir");
        let upstream_ext = options.global_dir().join("upstream-only.ts");
        fs::write(&upstream_ext, "export default function() {}").expect("write");

        let runner = runner();
        let pre = load_pre_trust(&runner, &ExtensionCatalog::default(), &options).await;
        assert!(pre.result.errors.is_empty());
        assert!(pre.result.extensions.is_empty());
        assert_eq!(pre.result.warnings.len(), 1);
        assert!(
            pre.result.warnings[0]
                .message
                .contains("Skipped JavaScript/TypeScript extension")
        );
    }

    #[tokio::test]
    async fn post_trust_adds_project_and_builtins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut options = LoadOptions::new(dir.path(), dir.path().join("agent"));
        fs::create_dir_all(options.global_dir()).expect("mkdir global");
        fs::create_dir_all(options.project_dir()).expect("mkdir project");
        let global_file = options.global_dir().join("global.ts");
        let project_file = options.project_dir().join("project.ts");
        fs::write(&global_file, "").expect("write global");
        fs::write(&project_file, "").expect("write project");

        let mut catalog = ExtensionCatalog::default();
        catalog.factories.insert(
            global_file.to_string_lossy().into_owned(),
            extension("global", false, "global-tool"),
        );
        catalog.factories.insert(
            project_file.to_string_lossy().into_owned(),
            extension("project", false, "project-tool"),
        );
        let builtin = extension("builtin:codemode", true, "codemode");
        catalog
            .builtins
            .insert("codemode".to_string(), (builtin, true));

        let runner = runner();
        let pre = load_pre_trust(&runner, &catalog, &options).await;
        assert_eq!(pre.path_keys.len(), 1);
        options.project_trusted = true;
        let post = load_post_trust(&runner, &catalog, &options, &pre).await;
        assert_eq!(post.path_keys.len(), 3);
        assert!(post.path_keys.iter().any(|path| path == "builtin:codemode"));
    }

    #[tokio::test]
    async fn reload_rejects_while_streaming_or_compacting() {
        let old = runner();
        let new = runner();
        for (streaming, compacting) in [(true, false), (false, true)] {
            let request = ReloadRequest {
                is_streaming: streaming,
                is_compacting: compacting,
                old_runner: &old,
                new_runner: &new,
                factories: &[],
                previous_flag_values: IndexMap::new(),
                active_tools: Vec::new(),
                default_tools: Vec::new(),
            };
            assert!(reload(request).await.is_err());
        }
    }

    #[tokio::test]
    async fn reload_runs_the_full_sequence() {
        let old = runner();
        let new = runner();
        let factories: Vec<ExtensionFactory> = vec![extension("fresh", false, "fresh-tool")];
        let mut flags = IndexMap::new();
        flags.insert("feature".to_string(), FlagValue::Bool(true));
        let request = ReloadRequest {
            is_streaming: false,
            is_compacting: false,
            old_runner: &old,
            new_runner: &new,
            factories: &factories,
            previous_flag_values: flags,
            active_tools: vec!["bash".to_string()],
            default_tools: vec!["fresh-tool".to_string(), "bash".to_string()],
        };
        let report = reload(request).await.expect("reload");
        assert_eq!(report.steps, RELOAD_SEQUENCE);
        assert_eq!(report.pending_tool_names, vec!["bash", "fresh-tool"]);
        assert_eq!(
            report.flag_values_restored.get("feature"),
            Some(&FlagValue::Bool(true))
        );
        assert!(old.host.assert_active().is_err());
        assert!(new.host.assert_active().is_ok());
    }

    #[tokio::test]
    async fn reload_emits_shutdown_and_start_on_the_correct_runners() {
        let old = runner();
        let new = runner();
        let shutdown = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(AtomicUsize::new(0));
        let shutdown_counter = Arc::clone(&shutdown);
        let start_counter = Arc::clone(&start);
        old.bus.on(
            crate::core::event_bus::ExtensionEvent::SessionShutdown,
            move |event| {
                let counter = Arc::clone(&shutdown_counter);
                Box::pin(async move {
                    assert_eq!(event["reason"], "reload");
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            },
        );
        new.bus.on(
            crate::core::event_bus::ExtensionEvent::SessionStart,
            move |event| {
                let counter = Arc::clone(&start_counter);
                Box::pin(async move {
                    assert_eq!(event["reason"], "reload");
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            },
        );
        let request = ReloadRequest {
            is_streaming: false,
            is_compacting: false,
            old_runner: &old,
            new_runner: &new,
            factories: &[],
            previous_flag_values: IndexMap::new(),
            active_tools: Vec::new(),
            default_tools: Vec::new(),
        };
        reload(request).await.expect("reload");
        assert_eq!(shutdown.load(Ordering::SeqCst), 1);
        assert_eq!(start.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reload_restores_previous_flag_values() {
        let old = runner();
        let new = runner();
        let mut flags = IndexMap::new();
        flags.insert("mode".to_string(), FlagValue::Str("fast".to_string()));
        new.host
            .set_flag_value("mode", FlagValue::Str("stale".to_string()));
        let request = ReloadRequest {
            is_streaming: false,
            is_compacting: false,
            old_runner: &old,
            new_runner: &new,
            factories: &[],
            previous_flag_values: flags,
            active_tools: Vec::new(),
            default_tools: Vec::new(),
        };
        let report = reload(request).await.expect("reload");
        assert_eq!(
            report.flag_values_restored.get("mode"),
            Some(&FlagValue::Str("fast".to_string()))
        );
    }

    #[test]
    fn reload_sequence_has_nine_steps() {
        assert_eq!(RELOAD_SEQUENCE.len(), 9);
        assert_eq!(RELOAD_SEQUENCE[0], "reject_while_busy");
        assert_eq!(RELOAD_SEQUENCE[8], "emit_resources_discover");
    }

    #[test]
    fn flag_defaults_are_validated_for_type() {
        let definition = FlagDefinition {
            name: "count".to_string(),
            description: None,
            kind: FlagKind::Boolean,
            default: Some(FlagValue::Str("1".to_string())),
            extension_path: "x".to_string(),
        };
        assert!(
            !definition
                .default
                .as_ref()
                .is_some_and(|value| value.matches_kind(definition.kind))
        );
    }
}
