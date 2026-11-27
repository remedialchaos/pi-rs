//! Offline package manager port of `packages/coding-agent/src/core/package-manager.ts`.
//!
//! Upstream installs npm/git packages by spawning package managers. This module
//! deliberately performs no network or process I/O: it parses sources, computes
//! the paths packages would occupy, plans the commands that would be run, and
//! reports structured results. `ensure_installed` takes the same lockfile that
//! upstream's settings managers use, so lock contention is observable without
//! installing anything.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::config;
use crate::lockfile::{self, LockOptions};
use crate::utils::git::{GitSource, parse_git_url};

/// Prefix for npm package sources (`npm:pkg@1.2.3`).
pub const NPM_PREFIX: &str = "npm:";
/// Prefix for git sources (`git:github.com/user/repo@v1`).
pub const GIT_PREFIX: &str = "git:";
/// Project settings directory.
pub const CONFIG_DIR_NAME: &str = ".pi";

/// A parsed package source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Npm {
        spec: String,
        name: String,
        version: Option<String>,
        pinned: bool,
    },
    Git {
        repo: String,
        host: String,
        path: String,
        git_ref: Option<String>,
        pinned: bool,
    },
    Local {
        path: PathBuf,
    },
}

impl PackageSource {
    /// Parse a configured package string.
    ///
    /// `npm:` selects npm packages, `git:` and explicit `https://`, `ssh://`,
    /// `git://` URLs select git packages; everything else (including `./` and
    /// `../` paths) is local.
    pub fn parse(source: &str) -> Result<Self, String> {
        let trimmed = source.trim();
        if let Some(spec) = trimmed.strip_prefix(NPM_PREFIX) {
            let spec = spec.trim();
            if spec.is_empty() {
                return Err("empty npm package spec".to_string());
            }
            let (name, version) = parse_npm_spec(spec);
            let pinned = version.as_deref().is_some_and(is_exact_version);
            return Ok(Self::Npm {
                spec: spec.to_string(),
                name,
                version,
                pinned,
            });
        }
        if is_local_path(trimmed) {
            return Ok(Self::Local {
                path: PathBuf::from(trimmed),
            });
        }
        parse_git_url(trimmed)
            .map(Self::from_git_source)
            .ok_or_else(|| format!("unsupported package source: {source}"))
    }

    fn from_git_source(source: GitSource) -> Self {
        Self::Git {
            repo: source.repo,
            host: source.host,
            path: source.path,
            git_ref: source.git_ref,
            pinned: source.pinned,
        }
    }

    /// Identity used for deduplication: npm names, normalized `host/path` for
    /// git, and an absolute-ish normalized path for local sources.
    pub fn identity(&self, base: &Path) -> String {
        match self {
            Self::Npm { name, .. } => format!("npm:{name}"),
            Self::Git { host, path, .. } => format!("git:{host}/{path}"),
            Self::Local { path } => format!("local:{}", normalize_path(path, base).display()),
        }
    }

    /// Display name shown in update reports and progress messages.
    pub fn display_name(&self) -> String {
        match self {
            Self::Npm { name, .. } => name.clone(),
            Self::Git { host, path, .. } => format!("{host}/{path}"),
            Self::Local { path } => path.to_string_lossy().into_owned(),
        }
    }

    /// Stable kind label for structured results.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Npm { .. } => "npm",
            Self::Git { .. } => "git",
            Self::Local { .. } => "local",
        }
    }
}

fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let separator = if let Some(rest) = spec.strip_prefix('@') {
        rest.find('@').map(|index| index + 1)
    } else {
        spec.find('@')
    };
    match separator {
        Some(index) if index > 0 => (
            spec[..index].to_string(),
            Some(spec[index + 1..].to_string()),
        ),
        _ => (spec.to_string(), None),
    }
}

fn is_exact_version(version: &str) -> bool {
    let version = version.trim();
    let version = version.strip_prefix('=').unwrap_or(version);
    let version = version.strip_prefix('v').unwrap_or(version);
    let parts: Vec<&str> = version.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_local_path(value: &str) -> bool {
    ![
        "npm:", "git:", "github:", "http:", "https:", "ssh:", "builtin:",
    ]
    .iter()
    .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
}

/// Lexically resolve `path` against `base`, removing `.` and handling `..`
/// without requiring the path to exist.
fn normalize_path(path: &Path, base: &Path) -> PathBuf {
    let combined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in combined.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

/// Whether a path, after normalization, stays under `root`.
fn is_under_root(root: &Path, candidate: &Path) -> bool {
    let root = normalize_path(root, Path::new("/"));
    let candidate = normalize_path(candidate, Path::new("/"));
    candidate == root || candidate.starts_with(&root)
}

/// Where a package would be installed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageScope {
    User,
    Project,
    Temporary,
}

impl PackageScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Temporary => "temporary",
        }
    }
}

/// State of a configured package in the offline model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// A package is already present at its managed path.
    Installed,
    /// The path does not exist and installs are enabled.
    NeedsInstall,
    /// The path does not exist and `PI_OFFLINE` (or `with_offline`) is set.
    SkippedOffline,
    /// Parsing or path resolution failed.
    Failed(String),
}

/// A configured package entry from settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: PackageScope,
    pub filtered: bool,
}

/// A package resolved to a concrete path and state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub source: String,
    pub identity: String,
    pub parsed: PackageSource,
    pub scope: PackageScope,
    pub path: PathBuf,
    pub state: InstallState,
}

/// A package that could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInstallFailure {
    pub source: String,
    pub scope: PackageScope,
    pub message: String,
}

/// Kinds of resource exposed by a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Extension,
    Skill,
    Prompt,
    Theme,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
            Self::Theme => "theme",
        }
    }
}

/// A resource discovered under a package root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub kind: ResourceKind,
    pub path: PathBuf,
    pub enabled: bool,
    pub source: String,
    pub scope: PackageScope,
}

/// Outcome of [`PackageManager::resolve_all`] or
/// [`PackageManager::resolve_extension_sources`].
#[derive(Debug, Clone, Default)]
pub struct ResolveResult {
    pub packages: Vec<ResolvedPackage>,
    pub resources: Vec<ResolvedResource>,
    pub failures: Vec<PackageInstallFailure>,
}

/// The command that would install a package. Nothing is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub source: String,
    pub scope: PackageScope,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl InstallPlan {
    /// Shell-independent display form of the plan.
    pub fn display(&self) -> String {
        let mut parts = vec![self.command.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// Result of [`PackageManager::ensure_installed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub source: String,
    pub state: InstallState,
    pub plan: Option<InstallPlan>,
}

/// Update check outcome. No registry or git command is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// Checks are disabled because the manager is offline.
    Offline,
    /// An exact npm version or git ref was pinned, so no check applies.
    Pinned,
    /// The package could be checked; the result is intentionally unknown.
    Unchecked,
}

/// Structured update check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub source: String,
    pub display_name: String,
    pub package_type: &'static str,
    pub scope: PackageScope,
    pub state: UpdateState,
}

/// A package-manager invocation (`npmCommand` setting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Flatten into a single argv vector.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![self.command.clone()];
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// A single step of a self-update command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateStep {
    pub command: String,
    pub args: Vec<String>,
}

impl SelfUpdateStep {
    pub fn display(&self) -> String {
        let mut parts = vec![self.command.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// A self-update command, with an optional cleanup step for renamed packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateCommand {
    pub install: SelfUpdateStep,
    pub cleanup: Option<SelfUpdateStep>,
}

impl SelfUpdateCommand {
    pub fn display(&self) -> String {
        match &self.cleanup {
            Some(cleanup) => format!("{} && {}", self.install.display(), cleanup.display()),
            None => self.install.display(),
        }
    }
}

/// Offline package manager. Settings are plain JSON so the module stays
/// independent of the settings manager crate.
#[derive(Debug, Clone)]
pub struct PackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    global_settings: Value,
    project_settings: Value,
    builtin_extensions: Vec<String>,
    offline: bool,
}

impl PackageManager {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self::with_settings(cwd, agent_dir, Value::Null, Value::Null)
    }

    pub fn with_settings(
        cwd: impl Into<PathBuf>,
        agent_dir: impl Into<PathBuf>,
        global_settings: Value,
        project_settings: Value,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            global_settings,
            project_settings,
            builtin_extensions: Vec::new(),
            offline: false,
        }
    }

    pub fn with_builtin_extensions(mut self, extensions: Vec<String>) -> Self {
        self.builtin_extensions = extensions;
        self
    }

    /// Force offline mode on or off (the environment is still consulted).
    pub fn with_offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// True when `PI_OFFLINE` is `1`, `true` or `yes`, or offline was forced.
    pub fn is_offline(&self) -> bool {
        self.offline || crate::config::is_truthy_env_flag("PI_OFFLINE")
    }

    /// The configured `npmCommand` argv, defaulting to `npm`.
    pub fn npm_command(&self) -> CommandSpec {
        let configured = self
            .global_settings
            .get("npmCommand")
            .and_then(Value::as_array);
        let argv: Vec<String> = configured
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        match argv.first() {
            None => CommandSpec {
                command: "npm".to_string(),
                args: Vec::new(),
            },
            Some(command) => CommandSpec {
                command: command.clone(),
                args: argv[1..].to_vec(),
            },
        }
    }

    /// Detect the effective package manager. A command after the final `--`
    /// separator wins over the outer wrapper (upstream issue #9863).
    pub fn package_manager_name(&self) -> String {
        let spec = self.npm_command();
        let normalize = |command: &str| {
            Path::new(command)
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_else(|| command.to_ascii_lowercase())
                .trim_end_matches(".cmd")
                .trim_end_matches(".exe")
                .to_string()
        };
        let direct = normalize(&spec.command);
        let supported = ["npm", "pnpm", "bun"];
        if let Some(position) = spec.args.iter().rposition(|arg| arg == "--")
            && let Some(wrapped) = spec.args.get(position + 1)
        {
            return normalize(wrapped);
        }
        if supported.contains(&direct.as_str()) {
            return direct;
        }
        let mut found = Vec::new();
        for arg in &spec.args {
            let name = normalize(arg);
            if supported.contains(&name.as_str()) && !found.contains(&name) {
                found.push(name);
            }
        }
        if found.len() > 1 {
            return direct;
        }
        found.first().cloned().unwrap_or(direct)
    }

    /// Arguments used to install the dependencies of a git checkout.
    pub fn git_dependency_install_args(&self) -> Vec<String> {
        match self.package_manager_name().as_str() {
            "bun" => ["install", "--omit=dev", "--omit=peer"]
                .iter()
                .map(|arg| arg.to_string())
                .collect(),
            "pnpm" => [
                "install",
                "--prod",
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]
            .iter()
            .map(|arg| arg.to_string())
            .collect(),
            _ => [
                "install",
                "--omit=dev",
                "--legacy-peer-deps",
                "--no-audit",
                "--no-fund",
            ]
            .iter()
            .map(|arg| arg.to_string())
            .collect(),
        }
    }

    /// Arguments used to install npm packages into a managed root.
    pub fn npm_install_args(&self, specs: &[String], root: &Path) -> Vec<String> {
        // Realpath avoids broken nested package-lock.json entries when --prefix
        // points through a symlink (upstream issue #11358).
        let install_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let root = install_root.to_string_lossy().into_owned();
        let mut args = vec!["install".to_string()];
        args.extend(specs.iter().cloned());
        match self.package_manager_name().as_str() {
            "bun" => {
                args.push("--cwd".to_string());
                args.push(root);
                args.push("--omit=peer".to_string());
            }
            "pnpm" => {
                args.push("--prefix".to_string());
                args.push(root);
                args.extend(
                    [
                        "--config.auto-install-peers=false",
                        "--config.strict-peer-dependencies=false",
                        "--config.strict-dep-builds=false",
                    ]
                    .iter()
                    .map(|arg| arg.to_string()),
                );
            }
            _ => {
                args.push("--prefix".to_string());
                args.push(root);
                args.extend(
                    ["--legacy-peer-deps", "--no-audit", "--no-fund"]
                        .iter()
                        .map(|arg| arg.to_string()),
                );
            }
        }
        args
    }

    /// Configured packages, project entries first. Project entries win when the
    /// same identity is configured in both scopes.
    pub fn configured_packages(&self) -> Vec<ConfiguredPackage> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for (settings, scope) in [
            (&self.project_settings, PackageScope::Project),
            (&self.global_settings, PackageScope::User),
        ] {
            let Some(packages) = settings.get("packages").and_then(Value::as_array) else {
                continue;
            };
            for package in packages {
                let (source, filtered) = match package {
                    Value::String(source) => (source.clone(), false),
                    Value::Object(object) => {
                        let source = object
                            .get("source")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let filtered = ["extensions", "skills", "prompts", "themes"]
                            .iter()
                            .any(|key| object.get(*key).is_some());
                        (source, filtered)
                    }
                    _ => continue,
                };
                let identity = PackageSource::parse(&source)
                    .map(|parsed| parsed.identity(&self.base_dir(scope)))
                    .unwrap_or_else(|_| source.clone());
                if !seen.insert(identity) {
                    continue;
                }
                result.push(ConfiguredPackage {
                    source,
                    scope,
                    filtered,
                });
            }
        }
        result
    }

    /// Entries configured with `builtin:` names (resolved as virtual paths).
    pub fn builtin_extensions(&self) -> Vec<ResolvedResource> {
        self.builtin_extensions
            .iter()
            .map(|name| ResolvedResource {
                kind: ResourceKind::Extension,
                path: PathBuf::from(format!("builtin:{name}")),
                enabled: true,
                source: "builtin".to_string(),
                scope: PackageScope::User,
            })
            .collect()
    }

    fn base_dir(&self, scope: PackageScope) -> PathBuf {
        match scope {
            PackageScope::User => self.agent_dir.clone(),
            PackageScope::Project => self.cwd.join(CONFIG_DIR_NAME),
            PackageScope::Temporary => self.agent_dir.join("tmp").join("extensions"),
        }
    }

    /// Managed root for a scope: `~/.pi/agent`, `<cwd>/.pi`, or the temp folder.
    pub fn managed_root(&self, scope: PackageScope) -> PathBuf {
        self.base_dir(scope)
    }

    /// Managed install root for a source kind.
    pub fn install_root(&self, source: &PackageSource, scope: PackageScope) -> PathBuf {
        let folder = match source {
            PackageSource::Npm { .. } => "npm",
            PackageSource::Git { .. } => "git",
            PackageSource::Local { .. } => "local",
        };
        self.base_dir(scope).join(folder)
    }

    /// Resolve `parts` under `root`, rejecting traversal outside it.
    pub fn managed_path(&self, root: &Path, parts: &[&str]) -> Result<PathBuf, String> {
        let mut candidate = root.to_path_buf();
        for part in parts {
            candidate = candidate.join(part);
        }
        let resolved = normalize_path(&candidate, &self.cwd);
        if !is_under_root(root, &resolved) {
            return Err(format!(
                "Refusing to use path outside package install root: {}",
                resolved.display()
            ));
        }
        Ok(resolved)
    }

    /// Path a parsed source occupies in `scope`.
    pub fn install_path(
        &self,
        source: &PackageSource,
        scope: PackageScope,
    ) -> Result<PathBuf, String> {
        match source {
            PackageSource::Npm { name, .. } => {
                let root = self.install_root(source, scope);
                self.managed_path(&root, &["node_modules", name])
            }
            PackageSource::Git {
                host,
                path,
                git_ref,
                ..
            } => {
                let root = self.install_root(source, scope);
                if scope == PackageScope::Temporary {
                    // Pinned refs get their own checkout via a short hash.
                    use std::hash::{DefaultHasher, Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    format!("{host}/{path}@{}", git_ref.as_deref().unwrap_or("")).hash(&mut hasher);
                    let hash = format!("{:08x}", hasher.finish());
                    return self.managed_path(&root, &[&hash, host, path]);
                }
                self.managed_path(&root, &[host, path])
            }
            PackageSource::Local { path } => Ok(normalize_path(path, &self.base_dir(scope))),
        }
    }

    /// Parse a source and resolve it to a path and state without installing.
    pub fn resolve(&self, source: &str) -> Result<ResolvedPackage, String> {
        let parsed = PackageSource::parse(source)?;
        let scope = self
            .configured_packages()
            .into_iter()
            .find(|entry| {
                PackageSource::parse(&entry.source)
                    .map(|candidate| candidate.identity(&self.base_dir(entry.scope)))
                    .ok()
                    == Some(parsed.identity(&self.base_dir(entry.scope)))
            })
            .map(|entry| entry.scope)
            .unwrap_or(PackageScope::User);
        self.resolve_in_scope(source, scope)
    }

    /// Parse a source and resolve it to a path and state in an explicit scope without installing.
    pub fn resolve_in_scope(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<ResolvedPackage, String> {
        let parsed = PackageSource::parse(source)?;
        let path = self.install_path(&parsed, scope)?;
        let state = if path.exists() {
            InstallState::Installed
        } else if self.is_offline() {
            InstallState::SkippedOffline
        } else {
            InstallState::NeedsInstall
        };
        Ok(ResolvedPackage {
            source: source.to_string(),
            identity: parsed.identity(&self.base_dir(scope)),
            parsed,
            scope,
            path,
            state,
        })
    }

    /// Resolve every configured package, collecting failures instead of
    /// aborting on the first bad source.
    pub fn resolve_all(&self) -> ResolveResult {
        let mut result = ResolveResult::default();
        for configured in self.configured_packages() {
            match self.resolve_in_scope(&configured.source, configured.scope) {
                Ok(resolved) => result.packages.push(resolved),
                Err(message) => result.failures.push(PackageInstallFailure {
                    source: configured.source,
                    scope: configured.scope,
                    message,
                }),
            }
        }
        result.resources = self.builtin_extensions();
        result
    }

    /// Lock directory used by `ensure_installed`.
    pub fn lock_path(&self) -> PathBuf {
        self.agent_dir.join("packages.lock")
    }

    /// Lock options matching the short critical sections upstream protects.
    pub fn lock_options(&self) -> LockOptions {
        LockOptions {
            stale_ms: 10_000,
            retries: 0,
            realpath: true,
            lockfile_path: None,
        }
    }

    /// Command that would be executed to install a resolved source.
    pub fn install_plan(&self, source: &str, scope: PackageScope) -> Result<InstallPlan, String> {
        let parsed = PackageSource::parse(source)?;
        match &parsed {
            PackageSource::Npm { .. } => {
                let root = self.install_root(&parsed, scope);
                Ok(InstallPlan {
                    source: source.to_string(),
                    scope,
                    command: self.npm_command().command,
                    args: {
                        let mut args = self.npm_command().args;
                        args.extend(self.npm_install_args(
                            &[match &parsed {
                                PackageSource::Npm { spec, .. } => spec.clone(),
                                _ => String::new(),
                            }],
                            &root,
                        ));
                        args
                    },
                    cwd: None,
                })
            }
            PackageSource::Git { repo, .. } => Ok(InstallPlan {
                source: source.to_string(),
                scope,
                command: "git".to_string(),
                args: vec![
                    "clone".to_string(),
                    repo.clone(),
                    self.install_path(&parsed, scope)?
                        .to_string_lossy()
                        .into_owned(),
                ],
                cwd: None,
            }),
            PackageSource::Local { .. } => Ok(InstallPlan {
                source: source.to_string(),
                scope,
                command: String::new(),
                args: Vec::new(),
                cwd: None,
            }),
        }
    }

    /// Check whether a package is installed under the lockfile, returning a
    /// structured plan when it is not. No command is executed.
    pub fn ensure_installed(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<InstallResult, String> {
        fs::create_dir_all(&self.agent_dir).map_err(|error| error.to_string())?;
        let _guard = lockfile::lock_sync(&self.lock_path(), &self.lock_options())?;
        let resolved = self.resolve(source)?;
        let plan = if resolved.state == InstallState::Installed {
            None
        } else {
            Some(self.install_plan(source, scope)?)
        };
        Ok(InstallResult {
            source: resolved.source,
            state: resolved.state,
            plan,
        })
    }

    /// Structured update check. Offline managers report nothing; online
    /// managers report unchecked packages (no registry access is performed).
    pub fn check_for_updates(&self) -> Vec<UpdateInfo> {
        if self.is_offline() {
            return Vec::new();
        }
        self.configured_packages()
            .into_iter()
            .filter_map(|entry| {
                let parsed = PackageSource::parse(&entry.source).ok()?;
                if matches!(
                    &parsed,
                    PackageSource::Npm { pinned: true, .. }
                        | PackageSource::Git { pinned: true, .. }
                ) {
                    return None;
                }
                let display_name = parsed.display_name();
                let package_type = parsed.kind();
                let state = UpdateState::Unchecked;
                Some(UpdateInfo {
                    source: entry.source,
                    display_name,
                    package_type,
                    scope: entry.scope,
                    state,
                })
            })
            .collect()
    }

    /// Add a source to in-memory global or project settings. Returns `true`
    /// when the settings changed. Local paths are stored relative to the
    /// corresponding settings base directory.
    pub fn add_source_to_settings(&mut self, source: &str, local: bool) -> bool {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        let base = self.base_dir(scope);
        let stored = self.store_source(source, scope);
        let new_identity = PackageSource::parse(&stored)
            .map(|parsed| parsed.identity(&base))
            .unwrap_or_else(|_| stored.clone());
        let new_ref = PackageSource::parse(&stored)
            .ok()
            .and_then(|parsed| match parsed {
                PackageSource::Git { git_ref, .. } => git_ref,
                _ => None,
            });
        let settings = if local {
            &mut self.project_settings
        } else {
            &mut self.global_settings
        };
        if !settings.is_object() {
            *settings = Value::Object(serde_json::Map::new());
        }
        let Some(object) = settings.as_object_mut() else {
            return false;
        };
        let entry = object
            .entry("packages")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(packages) = entry.as_array_mut() else {
            return false;
        };
        for existing in packages.iter_mut() {
            let existing_source = match existing {
                Value::String(source) => source.clone(),
                Value::Object(object) => object
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                _ => continue,
            };
            let identity = PackageSource::parse(&existing_source)
                .map(|parsed| parsed.identity(&base))
                .unwrap_or_else(|_| existing_source.clone());
            if identity == new_identity {
                let old_ref = PackageSource::parse(&existing_source)
                    .ok()
                    .and_then(|parsed| match parsed {
                        PackageSource::Git { git_ref, .. } => git_ref,
                        _ => None,
                    });
                if old_ref == new_ref {
                    return false;
                }
                match existing {
                    Value::String(value) => *value = stored,
                    Value::Object(object) => {
                        object.insert("source".to_string(), Value::String(stored));
                    }
                    _ => {}
                }
                return true;
            }
        }
        packages.push(Value::String(stored));
        true
    }

    /// Remove a source from in-memory settings, accepting equivalent path forms.
    pub fn remove_source_from_settings(&mut self, source: &str, local: bool) -> bool {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        let base = self.base_dir(scope);
        let identity = PackageSource::parse(source)
            .map(|parsed| parsed.identity(&base))
            .unwrap_or_else(|_| source.to_string());
        let settings = if local {
            &mut self.project_settings
        } else {
            &mut self.global_settings
        };
        let Some(packages) = settings
            .as_object_mut()
            .and_then(|object| object.get_mut("packages"))
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let before = packages.len();
        packages.retain(|entry| {
            let entry_source = match entry {
                Value::String(source) => source.clone(),
                Value::Object(object) => object
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                _ => return true,
            };
            PackageSource::parse(&entry_source)
                .map(|parsed| parsed.identity(&base))
                .unwrap_or_else(|_| entry_source.clone())
                != identity
        });
        before != packages.len()
    }

    fn store_source(&self, source: &str, scope: PackageScope) -> String {
        let Ok(PackageSource::Local { path }) = PackageSource::parse(source) else {
            return source.trim().to_string();
        };
        let base = self.base_dir(scope);
        let resolved = normalize_path(&path, &self.cwd);
        pathdiff(&base, &resolved)
    }

    /// Self-update command derived from the detected install method.
    pub fn get_self_update_command(&self, target: Option<&str>) -> Option<SelfUpdateCommand> {
        let method = config::detect_install_method().unwrap_or_else(|_| "unknown".to_string());
        let spec = target
            .map(str::to_string)
            .unwrap_or_else(|| config::PACKAGE_NAME.to_string());
        self_update_command_for_method(
            &method,
            &self.npm_command().argv(),
            &spec,
            config::PACKAGE_NAME,
        )
    }
}

/// Relative path from `base` to `target`, prefixed with `./` when needed.
fn pathdiff(base: &Path, target: &Path) -> String {
    let base_components: Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let common = base_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = Vec::new();
    for _ in common..base_components.len() {
        parts.push("..".to_string());
    }
    for component in &target_components[common..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    let joined = parts.join("/");
    if joined.is_empty() {
        ".".to_string()
    } else if joined.starts_with('.') {
        joined
    } else {
        format!("./{joined}")
    }
}

/// Build a self-update command for an install method without running anything.
pub fn self_update_command_for_method(
    method: &str,
    npm_command: &[String],
    install_spec: &str,
    installed_package_name: &str,
) -> Option<SelfUpdateCommand> {
    let step = |command: String, args: &[&str]| SelfUpdateStep {
        command,
        args: args.iter().map(|arg| arg.to_string()).collect(),
    };
    // Upstream compares package names, not the full `name@version` install spec.
    let spec_name = parse_npm_spec(install_spec).0;
    let cleanup = |command: &str, args: &[&str]| {
        if spec_name == installed_package_name {
            None
        } else {
            Some(step(command.to_string(), args))
        }
    };
    match method {
        "pnpm" => Some(SelfUpdateCommand {
            install: step(
                "pnpm".to_string(),
                &[
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--config.minimumReleaseAge=0",
                    install_spec,
                ],
            ),
            cleanup: cleanup("pnpm", &["remove", "-g", installed_package_name]),
        }),
        "yarn" => Some(SelfUpdateCommand {
            install: step(
                "yarn".to_string(),
                &["global", "add", "--ignore-scripts", install_spec],
            ),
            cleanup: cleanup("yarn", &["global", "remove", installed_package_name]),
        }),
        "bun" => Some(SelfUpdateCommand {
            install: step(
                "bun".to_string(),
                &[
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--minimum-release-age=0",
                    install_spec,
                ],
            ),
            cleanup: cleanup("bun", &["uninstall", "-g", installed_package_name]),
        }),
        "npm" => {
            let (command, leading) = match npm_command.split_first() {
                Some((command, args)) => (command.clone(), args.to_vec()),
                None => ("npm".to_string(), Vec::new()),
            };
            let mut args = leading;
            args.extend(
                [
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--min-release-age=0",
                    install_spec,
                ]
                .iter()
                .map(|arg| arg.to_string()),
            );
            let mut cleanup_args = Vec::new();
            cleanup_args.extend(npm_command.iter().skip(1).cloned());
            cleanup_args.extend(
                ["uninstall", "-g", installed_package_name]
                    .iter()
                    .map(|arg| arg.to_string()),
            );
            Some(SelfUpdateCommand {
                install: SelfUpdateStep {
                    command: command.clone(),
                    args,
                },
                cleanup: if spec_name == installed_package_name {
                    None
                } else {
                    Some(SelfUpdateStep {
                        command,
                        args: cleanup_args,
                    })
                },
            })
        }
        _ => None,
    }
}

/// User-configured self-update argv: `PI_SELF_UPDATE_COMMAND` (whitespace
/// separated) or the `selfUpdateCommand` setting.
pub fn configured_self_update_argv(settings: &Value) -> Option<Vec<String>> {
    if let Ok(command) = std::env::var("PI_SELF_UPDATE_COMMAND") {
        let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if !argv.is_empty() {
            return Some(argv);
        }
    }
    settings
        .get("selfUpdateCommand")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|argv: &Vec<String>| !argv.is_empty())
}

/// Expand the `{packageName}`, `{version}` and `{installSpec}` placeholders of
/// a configured self-update command.
pub fn make_configured_self_update(
    argv: &[String],
    package_name: &str,
    version: &str,
) -> SelfUpdateCommand {
    let values = [
        ("{packageName}", package_name),
        ("{version}", version),
        ("{installSpec}", &format!("{package_name}@{version}")),
    ];
    let expanded: Vec<String> = argv
        .iter()
        .map(|arg| {
            let mut result = arg.clone();
            for (placeholder, value) in values {
                result = result.replace(placeholder, value);
            }
            result
        })
        .collect();
    let (command, args) = expanded
        .split_first()
        .map(|(command, args)| (command.clone(), args.to_vec()))
        .unwrap_or_default();
    SelfUpdateCommand {
        install: SelfUpdateStep { command, args },
        cleanup: None,
    }
}

// ---------------------------------------------------------------------------
// Resource discovery (offline filesystem scan)
// ---------------------------------------------------------------------------

fn collect_files(dir: &Path, suffixes: &[&str], recursive: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(kind) = entry.file_type() {
            if kind.is_dir() && recursive {
                files.extend(collect_files(&path, suffixes, recursive));
            } else if kind.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| suffixes.contains(&extension))
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn is_excluded(relative: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches('!');
        let mut count = 0;
        let mut rest = pattern;
        if let Some(stripped) = pattern.strip_prefix("**/") {
            rest = stripped;
            count = 1;
        }
        if count == 1 {
            relative.ends_with(rest) || relative.contains(&format!("/{rest}"))
        } else {
            relative == rest
        }
    })
}

impl PackageManager {
    /// Scan explicit local sources for extensions, skills, prompts and themes.
    /// Manifest `pi` entries and `!` exclusion patterns are honored.
    pub fn resolve_extension_sources(&self, sources: &[String]) -> ResolveResult {
        let mut result = ResolveResult::default();
        for source in sources {
            let parsed = match PackageSource::parse(source) {
                Ok(parsed) => parsed,
                Err(message) => {
                    result.failures.push(PackageInstallFailure {
                        source: source.clone(),
                        scope: PackageScope::User,
                        message,
                    });
                    continue;
                }
            };
            let root = match &parsed {
                PackageSource::Local { path } => normalize_path(path, &self.cwd),
                _ => match self.install_path(&parsed, PackageScope::User) {
                    Ok(path) => path,
                    Err(message) => {
                        result.failures.push(PackageInstallFailure {
                            source: source.clone(),
                            scope: PackageScope::User,
                            message,
                        });
                        continue;
                    }
                },
            };
            if !root.is_dir() {
                continue;
            }
            let manifest = fs::read_to_string(root.join("package.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("pi").cloned());
            let mut exclusions = Vec::new();
            if let Some(manifest) = &manifest {
                for key in ["extensions", "skills", "prompts", "themes"] {
                    if let Some(entries) = manifest.get(key).and_then(Value::as_array) {
                        for entry in entries.iter().filter_map(Value::as_str) {
                            if let Some(pattern) = entry.strip_prefix('!') {
                                exclusions.push(pattern.to_string());
                            }
                        }
                    }
                }
            }
            let add = |kind: ResourceKind, path: PathBuf, result: &mut ResolveResult| {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_excluded(&relative, &exclusions) {
                    return;
                }
                result.resources.push(ResolvedResource {
                    kind,
                    path,
                    enabled: true,
                    source: source.clone(),
                    scope: PackageScope::User,
                });
            };
            let defaults = [
                (ResourceKind::Extension, "extensions", vec!["ts", "js"]),
                (ResourceKind::Skill, "skills", vec!["md"]),
                (ResourceKind::Prompt, "prompts", vec!["md"]),
                (ResourceKind::Theme, "themes", vec!["json"]),
            ];
            for (kind, folder, suffixes) in defaults {
                let dir = root.join(folder);
                if dir.is_dir() {
                    for file in collect_files(&dir, &suffixes, true) {
                        if kind == ResourceKind::Skill
                            && file.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
                            && file.parent() == Some(dir.as_path())
                        {
                            // Root markdown files are skills; nested dirs need SKILL.md.
                            add(kind, file, &mut result);
                        } else {
                            add(kind, file, &mut result);
                        }
                    }
                }
            }
            if let Some(manifest) = manifest {
                for (key, kind) in [
                    ("extensions", ResourceKind::Extension),
                    ("skills", ResourceKind::Skill),
                    ("prompts", ResourceKind::Prompt),
                    ("themes", ResourceKind::Theme),
                ] {
                    let Some(entries) = manifest.get(key).and_then(Value::as_array) else {
                        continue;
                    };
                    for entry in entries.iter().filter_map(Value::as_str) {
                        if entry.starts_with('!') || entry.starts_with('+') {
                            continue;
                        }
                        let path = normalize_path(Path::new(entry), &root);
                        if path.is_dir() {
                            let suffixes: &[&str] = match kind {
                                ResourceKind::Extension => &["ts", "js"],
                                ResourceKind::Skill => &["md"],
                                ResourceKind::Prompt => &["md"],
                                ResourceKind::Theme => &["json"],
                            };
                            for file in collect_files(&path, suffixes, true) {
                                add(kind, file, &mut result);
                            }
                        } else if path.is_file() {
                            add(kind, path, &mut result);
                        }
                    }
                }
            }
        }
        result
            .resources
            .sort_by(|left, right| left.path.cmp(&right.path));
        result
            .resources
            .dedup_by(|left, right| left.path == right.path);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("pi-pm-")
            .tempdir()
            .expect("temp dir")
    }

    #[test]
    fn parses_npm_sources_and_pinning() {
        let PackageSource::Npm {
            name,
            version,
            pinned,
            ..
        } = PackageSource::parse("npm:@scope/pkg@1.2.3").expect("npm source")
        else {
            panic!("expected npm source");
        };
        assert_eq!(name, "@scope/pkg");
        assert_eq!(version.as_deref(), Some("1.2.3"));
        assert!(pinned);

        let parsed = PackageSource::parse("npm:@scope/pkg@^1.2.3").expect("npm source");
        assert!(matches!(parsed, PackageSource::Npm { pinned: false, .. }));
        let parsed = PackageSource::parse("npm:pkg").expect("npm source");
        assert!(matches!(
            parsed,
            PackageSource::Npm {
                pinned: false,
                version: None,
                ..
            }
        ));
        assert!(PackageSource::parse("npm:").is_err());
    }

    #[test]
    fn parses_git_sources_in_all_supported_forms() {
        for source in [
            "git:github.com/user/repo@v1",
            "https://github.com/user/repo@v1",
            "git:git@github.com:user/repo@v1",
            "ssh://git@github.com/user/repo@v1",
            "https://gitlab.com/user/repo.git",
            "https://bitbucket.org/user/repo",
            "https://codeberg.org/user/repo",
        ] {
            assert!(
                matches!(PackageSource::parse(source), Ok(PackageSource::Git { .. })),
                "expected git source: {source}",
            );
        }
        let parsed = PackageSource::parse("https://github.com/user/repo").expect("git source");
        assert_eq!(
            parsed.identity(Path::new("/tmp")),
            "git:github.com/user/repo",
        );
        assert_eq!(
            PackageSource::parse("git:github.com/user/repo")
                .expect("git source")
                .identity(Path::new("/tmp")),
            "git:github.com/user/repo",
        );
    }

    #[test]
    fn dot_relative_and_absolute_paths_are_local() {
        for source in [
            "./packages/agent-timers",
            "../packages/agent-timers",
            "/absolute/path/to/package",
            "github.com/user/repo",
        ] {
            assert!(
                matches!(
                    PackageSource::parse(source),
                    Ok(PackageSource::Local { .. })
                ),
                "expected local source: {source}",
            );
        }
        let parsed = PackageSource::parse("./packages/agent-timers").expect("local source");
        assert_eq!(parsed.kind(), "local");
    }

    #[test]
    fn npm_command_and_package_manager_detection() {
        let root = temp_dir();
        let settings = serde_json::json!({
            "npmCommand": ["mise", "exec", "node@20", "--", "npm"],
        });
        let manager =
            PackageManager::with_settings(root.path(), root.path(), settings, Value::Null);
        assert_eq!(
            manager.npm_command(),
            CommandSpec {
                command: "mise".to_string(),
                args: ["exec", "node@20", "--", "npm"]
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect(),
            },
        );
        assert_eq!(manager.package_manager_name(), "npm");

        let pnpm = PackageManager::with_settings(
            root.path(),
            root.path(),
            serde_json::json!({"npmCommand": ["corepack", "pnpm"]}),
            Value::Null,
        );
        assert_eq!(pnpm.package_manager_name(), "pnpm");
        assert_eq!(
            pnpm.git_dependency_install_args(),
            [
                "install",
                "--prod",
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]
            .iter()
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn separator_overrides_outer_executable() {
        let root = temp_dir();
        let manager = PackageManager::with_settings(
            root.path(),
            root.path(),
            serde_json::json!({"npmCommand": ["npm", "exec", "--", "pnpm"]}),
            Value::Null,
        );
        assert_eq!(manager.package_manager_name(), "pnpm");
    }

    #[test]
    fn npm_install_args_use_realpath_and_disable_peers() {
        let root = temp_dir();
        let real = root.path().join("real-npm");
        fs::create_dir_all(&real).expect("mkdir");
        let link = root.path().join("linked-npm");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(not(unix))]
        let link = real.clone();

        let manager = PackageManager::new(root.path(), root.path());
        let args = manager.npm_install_args(&["pkg".to_string()], &link);
        assert_eq!(args[0], "install");
        assert_eq!(args[1], "pkg");
        assert_eq!(args[2], "--prefix");
        assert_eq!(
            args[3],
            fs::canonicalize(&real)
                .expect("realpath")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(args.contains(&"--legacy-peer-deps".to_string()));
        assert!(args.contains(&"--no-audit".to_string()));
    }

    #[test]
    fn managed_paths_reject_traversal() {
        let root = temp_dir();
        let manager = PackageManager::new(root.path(), root.path().join("agent"));
        let source = PackageSource::Git {
            repo: "git@evil.example:../../victim/repo".to_string(),
            host: "evil.example".to_string(),
            path: "../../victim/repo".to_string(),
            git_ref: None,
            pinned: false,
        };
        assert!(manager.install_path(&source, PackageScope::User).is_err());
        let npm_source = PackageSource::parse("npm:pkg").expect("npm source");
        let path = manager
            .install_path(&npm_source, PackageScope::User)
            .expect("valid path");
        assert!(path.ends_with("npm/node_modules/pkg"));
        assert!(manager.managed_root(PackageScope::Project).ends_with(".pi"));
    }

    #[test]
    fn configured_packages_dedupe_with_project_winning() {
        let root = temp_dir();
        let manager = PackageManager::with_settings(
            root.path(),
            root.path().join("agent"),
            serde_json::json!({"packages": ["npm:foo", "git:github.com/u/r"]}),
            serde_json::json!({"packages": ["npm:foo@1.0.0"]}),
        );
        let configured = manager.configured_packages();
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0].scope, PackageScope::Project);
        assert_eq!(configured[1].source, "git:github.com/u/r");
    }

    #[test]
    fn resolve_reports_offline_state() {
        let root = temp_dir();
        let manager =
            PackageManager::new(root.path(), root.path().join("agent")).with_offline(true);
        let resolved = manager.resolve("npm:missing-pkg").expect("resolve");
        assert_eq!(resolved.state, InstallState::SkippedOffline);
        assert!(resolved.path.ends_with("npm/node_modules/missing-pkg"));

        fs::create_dir_all(&resolved.path).expect("mkdir");
        let installed = manager.resolve("npm:missing-pkg").expect("resolve");
        assert_eq!(installed.state, InstallState::Installed);
    }

    #[test]
    fn ensure_installed_returns_a_structured_plan() {
        let root = temp_dir();
        let manager = PackageManager::new(root.path(), root.path().join("agent"));
        let result = manager
            .ensure_installed("npm:foo", PackageScope::User)
            .expect("install");
        assert_eq!(result.state, InstallState::NeedsInstall);
        let plan = result.plan.expect("plan");
        assert_eq!(plan.command, "npm");
        assert!(plan.display().contains("--prefix"));
        assert!(!manager.lock_path().exists());
    }

    #[test]
    fn lock_contention_returns_elocked() {
        let root = temp_dir();
        let manager = PackageManager::new(root.path(), root.path().join("agent"));
        fs::create_dir_all(manager.agent_dir()).expect("agent dir");
        let guard =
            lockfile::lock_sync(&manager.lock_path(), &manager.lock_options()).expect("first lock");
        let result = manager.ensure_installed("npm:foo", PackageScope::User);
        assert_eq!(result.err(), Some("ELOCKED".to_string()));
        drop(guard);
        assert!(
            manager
                .ensure_installed("npm:foo", PackageScope::User)
                .is_ok()
        );
    }

    #[test]
    fn check_for_updates_is_offline_and_skips_pinned() {
        let root = temp_dir();
        let manager = PackageManager::with_settings(
            root.path(),
            root.path(),
            serde_json::json!({"packages": ["npm:foo@1.0.0", "git:github.com/u/r@v1"]}),
            Value::Null,
        );
        assert!(manager.check_for_updates().is_empty());

        let manager = manager.with_offline(false);
        let updates = manager.check_for_updates();
        assert!(updates.is_empty(), "pinned packages are never checked");

        let manager = PackageManager::with_settings(
            root.path(),
            root.path(),
            serde_json::json!({"packages": ["npm:foo", "git:github.com/u/r"]}),
            Value::Null,
        )
        .with_offline(false);
        let updates = manager.check_for_updates();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].state, UpdateState::Unchecked);
        assert_eq!(updates[0].display_name, "foo");
    }

    #[test]
    fn self_update_commands_cover_supported_install_methods() {
        let root = temp_dir();
        let manager = PackageManager::new(root.path(), root.path());
        let npm =
            self_update_command_for_method("npm", &[], "pkg@2.0.0", "pkg").expect("npm command");
        assert_eq!(
            npm.display(),
            "npm install -g --ignore-scripts --min-release-age=0 pkg@2.0.0",
        );
        let pnpm = self_update_command_for_method("pnpm", &[], "new-pkg@2.0.0", "old-pkg")
            .expect("pnpm command");
        assert!(pnpm.display().contains("pnpm remove -g old-pkg"));
        assert!(self_update_command_for_method("bun-binary", &[], "pkg", "pkg").is_none());
        assert!(self_update_command_for_method("unknown", &[], "pkg", "pkg").is_none());
        let _ = manager.get_self_update_command(None);
    }

    #[test]
    fn configured_self_update_argv_expands_placeholders() {
        let argv = vec![
            "my-tool".to_string(),
            "update".to_string(),
            "{installSpec}".to_string(),
        ];
        let command = make_configured_self_update(&argv, "pkg", "2.0.0");
        assert_eq!(command.display(), "my-tool update pkg@2.0.0");

        let root = temp_dir();
        let settings = serde_json::json!({"selfUpdateCommand": ["a", "b"]});
        assert_eq!(
            configured_self_update_argv(&settings),
            Some(vec!["a".to_string(), "b".to_string()]),
        );
        let _ = root;
    }

    #[test]
    fn settings_source_add_and_remove() {
        let root = temp_dir();
        let mut manager = PackageManager::new(root.path(), root.path().join("agent"));
        assert!(manager.add_source_to_settings("git:github.com/user/repo@v1", false));
        assert!(!manager.add_source_to_settings("git:github.com/user/repo@v1", false));
        assert!(manager.add_source_to_settings("git:github.com/user/repo@v2", false));
        assert_eq!(manager.configured_packages().len(), 1);
        assert!(manager.remove_source_from_settings("git:github.com/user/repo@v2", false));
        assert!(manager.configured_packages().is_empty());
    }

    #[test]
    fn resolve_extension_sources_scans_directories_and_manifest() {
        let root = temp_dir();
        let pkg = root.path().join("pkg");
        fs::create_dir_all(pkg.join("extensions")).expect("mkdir");
        fs::create_dir_all(pkg.join("skills").join("demo")).expect("mkdir");
        fs::write(pkg.join("extensions").join("main.ts"), "").expect("write");
        fs::write(pkg.join("extensions").join("skip.ts"), "").expect("write");
        fs::write(pkg.join("skills").join("demo").join("SKILL.md"), "").expect("write");
        fs::write(
            pkg.join("package.json"),
            r#"{"pi":{"extensions":["extensions","!**/skip.ts"],"skills":["skills"]}}"#,
        )
        .expect("write");
        let source = pkg.to_string_lossy().into_owned();
        let manager = PackageManager::new(root.path(), root.path());
        let result = manager.resolve_extension_sources(&[source]);
        let extensions: Vec<_> = result
            .resources
            .iter()
            .filter(|resource| resource.kind == ResourceKind::Extension)
            .collect();
        assert_eq!(extensions.len(), 1);
        assert!(extensions[0].path.ends_with("main.ts"));
        assert!(
            result
                .resources
                .iter()
                .any(|resource| resource.kind == ResourceKind::Skill)
        );
    }
}
