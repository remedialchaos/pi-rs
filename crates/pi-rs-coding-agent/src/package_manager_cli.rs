//! Package, model catalog, and self-update commands for the `pi` executable.

use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use node_semver::{Range, Version};
use serde_json::Value;
use tokio::process::Command;

use crate::config::{self, PACKAGE_NAME, VERSION};
use crate::core::package_manager::{
    PackageManager, PackageScope, PackageSource, SelfUpdateCommand, configured_self_update_argv,
    make_configured_self_update, self_update_command_for_method,
};
use crate::core::project_trust::{
    DefaultProjectTrust, ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted,
};
use crate::core::settings_manager::{CreateOptions, SettingsManager, SettingsScope};
use crate::lockfile::{LockGuard, LockOptions, lock_sync};
use crate::utils::paths::resolve_path;
use crate::utils::text::strip_bom;
use crate::utils::version_check::{
    LatestPiRelease, VersionCheckOptions, get_latest_pi_release, is_newer_package_version,
};

#[cfg(any(windows, test))]
#[path = "utils/windows_self_update.rs"]
mod windows_self_update;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MODEL_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
const INSTALLER_API_BASE: &str = "https://pi.dev/api/installer/releases";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Remove,
    List,
    Update,
    Config,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::List => "list",
            Self::Update => "update",
            Self::Config => "config",
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Self::Install => "pi install <source> [-l] [--approve|--no-approve]",
            Self::Remove => "pi remove <source> [-l] [--approve|--no-approve]",
            Self::List => "pi list [--approve|--no-approve]",
            Self::Update => {
                "pi update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
            }
            Self::Config => "pi config [-l] [--approve|--no-approve]",
        }
    }

    fn print_help(self) {
        let details = match self {
            Self::Install => {
                "Install a package and add it to settings.

Options:
  -l, --local       Install project-locally (.pi/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  pi install npm:@foo/bar
  pi install git:github.com/user/repo
  pi install git:git@github.com:user/repo
  pi install https://github.com/user/repo
  pi install ssh://git@github.com/user/repo
  pi install ./local/path"
            }
            Self::Remove => {
                "Remove a package and its source from settings.
Alias: pi uninstall <source> [-l]

Options:
  -l, --local       Remove from project settings (.pi/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  pi remove npm:@foo/bar
  pi uninstall npm:@foo/bar"
            }
            Self::List => {
                "List installed packages from user and project settings.

Options:
  -a, --approve      Trust project-local files for this command
  -na, --no-approve  Ignore project-local files for this command"
            }
            Self::Update => {
                "Update pi, installed packages, or model catalogs.

Options:
  --self                  Update pi only (default when no target is given)
  --extensions            Update installed packages only
  --models                Refresh model catalogs only
  --all                   Update pi and installed packages
  --extension <source>    Update one package only
  -a, --approve           Trust project-local files for this command
  -na, --no-approve       Ignore project-local files for this command
  --force                 Reinstall pi even if the current version is latest

Short forms:
  pi update                Update pi only
  pi update --all          Update pi and all extensions
  pi update --models       Refresh model catalogs only
  pi update <source>       Update one package
  pi update pi             Update pi only (self works as alias to pi)"
            }
            Self::Config => {
                "Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/.pi/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides (.pi/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l"
            }
        };
        println!("Usage:\n  {}\n\n{details}\n", self.usage());
    }
}

#[derive(Debug, PartialEq, Eq)]
enum UpdateTarget<'a> {
    SelfOnly,
    All,
    Extensions(Option<&'a str>),
    Models,
}

struct Options<'a> {
    action: Action,
    source: Option<&'a str>,
    target: UpdateTarget<'a>,
    local: bool,
    force: bool,
    trust: Option<bool>,
    implicit_self: bool,
}

fn parse_options(action: Action, args: &[String]) -> Result<Options<'_>, String> {
    let mut options = Options {
        action,
        source: None,
        target: UpdateTarget::SelfOnly,
        local: false,
        force: false,
        trust: None,
        implicit_self: false,
    };
    let mut invalid = None;
    let mut extra = None;
    let mut missing_extension = false;
    let mut conflict = None;
    let mut self_flag = false;
    let mut extensions = false;
    let mut models = false;
    let mut all = false;
    let mut extension = None;
    let mut args = args.iter().map(String::as_str).peekable();
    while let Some(arg) = args.next() {
        match arg {
            "-l" | "--local"
                if matches!(action, Action::Install | Action::Remove | Action::Config) =>
            {
                options.local = true
            }
            "-a" | "--approve" => options.trust = Some(true),
            "-na" | "--no-approve" => options.trust = Some(false),
            "--self" if action == Action::Update => self_flag = true,
            "--extensions" if action == Action::Update => extensions = true,
            "--models" if action == Action::Update => models = true,
            "--all" if action == Action::Update => all = true,
            "--force" if action == Action::Update => options.force = true,
            "--extension" if action == Action::Update => {
                if args
                    .peek()
                    .is_some_and(|next| !next.is_empty() && !next.starts_with('-'))
                {
                    let value = args.next();
                    if extension.is_some() {
                        conflict.get_or_insert("--extension can only be provided once");
                    } else {
                        extension = value;
                    }
                } else {
                    missing_extension = true;
                }
            }
            _ if arg.starts_with('-') => {
                invalid.get_or_insert(arg);
                if action == Action::Config {
                    break;
                }
            }
            _ if action == Action::Config => {
                extra = Some(arg);
                break;
            }
            _ if options.source.is_none() => options.source = (!arg.is_empty()).then_some(arg),
            _ => {
                extra.get_or_insert(arg);
            }
        }
    }
    let usage = action.usage();
    if let Some(flag) = invalid {
        return Err(format!(
            "Unknown option {flag} for \"{}\".\nUse \"pi --help\" or \"{usage}\".",
            action.name()
        ));
    }
    if missing_extension {
        return Err(format!("Missing value for --extension.\nUsage: {usage}"));
    }
    if let Some(arg) = extra {
        return Err(format!("Unexpected argument {arg}.\nUsage: {usage}"));
    }
    if action == Action::Update {
        let source = options.source;
        if all && (self_flag || extensions || models || extension.is_some()) {
            conflict.get_or_insert(
                "--all cannot be combined with --self, --extensions, --models, or --extension",
            );
        }
        if all && source.is_some() {
            conflict.get_or_insert("--all cannot be combined with a positional source");
        }
        options.target = if models {
            if self_flag || extensions || all || extension.is_some() {
                conflict.get_or_insert(
                    "--models cannot be combined with --self, --extensions, --all, or --extension",
                );
            }
            if source.is_some() {
                conflict.get_or_insert("--models cannot be combined with a positional source");
            }
            UpdateTarget::Models
        } else if let Some(extension) = extension {
            if self_flag || extensions || all {
                conflict.get_or_insert(
                    "--extension cannot be combined with --self, --extensions, or --all",
                );
            }
            if source.is_some() {
                conflict.get_or_insert("--extension cannot be combined with a positional source");
            }
            UpdateTarget::Extensions(Some(extension))
        } else if let Some(source) = source {
            if source == "self" || source == "pi" {
                if extensions {
                    UpdateTarget::All
                } else {
                    UpdateTarget::SelfOnly
                }
            } else {
                if self_flag || extensions || all {
                    conflict.get_or_insert("positional update targets cannot be combined with --self, --extensions, or --all");
                }
                UpdateTarget::Extensions(Some(source))
            }
        } else if all || (self_flag && extensions) {
            UpdateTarget::All
        } else if extensions {
            UpdateTarget::Extensions(None)
        } else {
            options.implicit_self = !self_flag;
            UpdateTarget::SelfOnly
        };
    }
    if let Some(conflict) = conflict {
        return Err(format!("{conflict}\nUsage: {usage}"));
    }
    if matches!(action, Action::Install | Action::Remove) && options.source.is_none() {
        return Err(format!("Missing {} source.\nUsage: {usage}", action.name()));
    }
    Ok(options)
}

/// Run a package or config command, returning `None` if argv belongs to another command.
pub async fn run(args: &[String], cwd: &Path, agent_dir: &Path) -> Option<u8> {
    let (command, args) = args.split_first()?;
    let action = match command.as_str() {
        "install" => Action::Install,
        "remove" | "uninstall" => Action::Remove,
        "list" => Action::List,
        "update" => Action::Update,
        "config" => Action::Config,
        _ => return None,
    };
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        action.print_help();
        return Some(0);
    }
    let options = match parse_options(action, args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return Some(1);
        }
    };
    Some(match execute(options, cwd, agent_dir).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    })
}

async fn execute(options: Options<'_>, cwd: &Path, agent_dir: &Path) -> Result<u8, String> {
    let cwd = std::path::absolute(cwd).map_err(|error| error.to_string())?;
    let agent_dir = resolve_path(&agent_dir.to_string_lossy(), &cwd, false);
    let cwd = cwd.as_path();
    let agent_dir = agent_dir.as_path();
    let mut settings = SettingsManager::create_with_dirs(
        cwd,
        agent_dir,
        CreateOptions {
            project_trusted: false,
        },
    );
    if options.target == UpdateTarget::Models {
        refresh_models(agent_dir, &settings).await?;
        return Ok(0);
    }
    let global = settings.get_global_settings();
    let trust = ProjectTrustStore::new(agent_dir);
    let decision = if options.action == Action::Update {
        match options.trust {
            Some(decision) => Ok(decision),
            None => trust.get(cwd).map(|saved| saved == Some(true)),
        }
    } else {
        let default_project_trust = match global.get("defaultProjectTrust").and_then(Value::as_str)
        {
            Some("always") => Some(DefaultProjectTrust::Always),
            Some("never") => Some(DefaultProjectTrust::Never),
            _ => None,
        };
        resolve_project_trusted(
            &trust,
            &ResolveProjectTrustedOptions {
                cwd: Some(cwd),
                trust_override: options.trust,
                default_project_trust,
                ..ResolveProjectTrustedOptions::default()
            },
        )
    };
    let trusted = decision.unwrap_or_else(|error| {
        eprintln!("Warning: {error}");
        false
    });
    if options.local && !trusted {
        let what = if options.action == Action::Config {
            "resource"
        } else {
            "package"
        };
        eprintln!("Project is not trusted. Use --approve to modify local {what} config.");
        return Ok(1);
    }
    settings.set_project_trusted(trusted);
    let context = if options.action == Action::Config {
        "config"
    } else {
        "package"
    };
    for error in settings.drain_errors() {
        eprintln!(
            "Warning ({context} command, {} settings): {}",
            error.scope.as_str(),
            error.error
        );
    }
    if options.action == Action::Config {
        let scope = if options.local {
            SettingsScope::Project
        } else {
            SettingsScope::Global
        };
        let packages = PackageManager::with_settings(
            cwd,
            agent_dir,
            settings.get_global_settings(),
            settings.get_project_settings(),
        );
        crate::cli::config_selector::select_config(
            packages.resolve_all(),
            &mut settings,
            cwd,
            agent_dir,
            scope,
            trusted,
        )
        .await?;
        return Ok(0);
    }
    let project = settings.get_project_settings();
    let packages = PackageManager::with_settings(cwd, agent_dir, global, Value::Null);
    // For CLI listing and updates, both scopes are independent installations. The manager's
    // resource resolution view deduplicates them, which is appropriate only for loading resources.
    let global = settings.get_global_settings();
    let user_entries = package_entries(&global);
    let project_entries = package_entries(&project);
    match (options.action, options.source) {
        (Action::Install | Action::Remove, Some(source)) => {
            let scope = if options.local {
                PackageScope::Project
            } else {
                PackageScope::User
            };
            let entries = if options.local {
                project_entries
            } else {
                user_entries
            };
            let base = packages.managed_root(scope);
            let parsed = parse_source(source, cwd)?;
            if options.action == Action::Install {
                println!("Installing {source}...");
                install(&packages, source, &parsed, scope).await?;
            } else {
                println!("Removing {source}...");
                remove(&packages, &parsed, scope).await?;
            }
            let mut next = entries.to_vec();
            if !edit_sources(
                &mut next,
                source,
                &parsed,
                cwd,
                &base,
                options.action == Action::Install,
            ) {
                if options.action == Action::Remove {
                    eprintln!("No matching package found for {source}");
                    return Ok(1);
                }
            } else {
                let settings_scope = if options.local {
                    SettingsScope::Project
                } else {
                    SettingsScope::Global
                };
                settings.set_value_scoped(
                    settings_scope,
                    &["packages".to_owned()],
                    Value::Array(next),
                )?;
            }
            if options.action == Action::Install {
                println!("Installed {source}");
            } else {
                println!("Removed {source}");
            }
        }
        (Action::List, _) => list(&packages, user_entries, project_entries).await?,
        (Action::Update, _) => {
            if options.implicit_self {
                println!(
                    "Extensions are skipped. Run pi update --extensions to update extensions."
                );
            }
            match options.target {
                UpdateTarget::All => {
                    update_packages(&packages, user_entries, project_entries, None).await?;
                    return update_self(&packages, &global, options.force).await;
                }
                UpdateTarget::SelfOnly => {
                    return update_self(&packages, &global, options.force).await;
                }
                UpdateTarget::Extensions(source) => {
                    update_packages(&packages, user_entries, project_entries, source).await?
                }
                UpdateTarget::Models => {}
            }
        }
        _ => {}
    }
    Ok(0)
}

fn package_entries(settings: &Value) -> &[Value] {
    settings
        .get("packages")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn entry_source(entry: &Value) -> Option<&str> {
    entry
        .as_str()
        .or_else(|| entry.get("source").and_then(Value::as_str))
}

fn parse_source(source: &str, base: &Path) -> Result<PackageSource, String> {
    match PackageSource::parse(source) {
        Ok(PackageSource::Local { .. }) => Ok(PackageSource::Local {
            path: resolve_path(source, base, false),
        }),
        Err(_) if !source.starts_with("npm:") => Ok(PackageSource::Local {
            path: resolve_path(source, base, false),
        }),
        result => result,
    }
}

fn relative_path(base: &Path, target: &Path) -> String {
    for (parents, ancestor) in base.ancestors().enumerate() {
        if let Ok(tail) = target.strip_prefix(ancestor) {
            let mut result = PathBuf::new();
            for _ in 0..parents {
                result.push("..");
            }
            if !tail.as_os_str().is_empty() {
                result.push(tail);
            }
            return if result.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                result.to_string_lossy().into_owned()
            };
        }
    }
    target.to_string_lossy().into_owned()
}

fn edit_sources(
    entries: &mut Vec<Value>,
    source: &str,
    parsed: &PackageSource,
    cwd: &Path,
    base: &Path,
    install: bool,
) -> bool {
    let identity = parsed.identity(cwd);
    let read = |entry: &Value| entry_source(entry).and_then(|value| parse_source(value, base).ok());
    if !install {
        let before = entries.len();
        entries.retain(|entry| {
            read(entry).is_none_or(|candidate| candidate.identity(base) != identity)
        });
        return before != entries.len();
    }
    let stored = match parsed {
        PackageSource::Local { path } => relative_path(base, path),
        _ => source.to_owned(),
    };
    for entry in entries.iter_mut() {
        let Some(existing) = read(entry) else {
            continue;
        };
        if existing.identity(base) != identity {
            continue;
        }
        let changed = entry_source(entry) != Some(stored.as_str());
        if changed {
            match entry {
                Value::Object(object) => {
                    object.insert("source".to_owned(), Value::String(stored));
                }
                value => *value = Value::String(stored),
            }
        }
        return changed;
    }
    entries.push(Value::String(stored));
    true
}

async fn list(manager: &PackageManager, user: &[Value], project: &[Value]) -> Result<(), String> {
    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }
    let mut legacy_root = None;
    let mut pnpm_paths = None;
    for (entries, scope, title) in [
        (user, PackageScope::User, "User packages:"),
        (project, PackageScope::Project, "Project packages:"),
    ] {
        if entries.is_empty() {
            continue;
        }
        if scope == PackageScope::Project && !user.is_empty() {
            println!();
        }
        println!("{title}");
        for entry in entries {
            let Some(source) = entry_source(entry) else {
                continue;
            };
            let parsed = parse_source(source, &manager.managed_root(scope))?;
            let mut path = manager.install_path(&parsed, scope)?;
            if !path.exists()
                && scope == PackageScope::User
                && let PackageSource::Npm { name, .. } = &parsed
            {
                if pnpm_paths.is_none() {
                    pnpm_paths = Some(if manager.package_manager_name() == "pnpm" {
                        legacy_pnpm_paths(manager).await
                    } else {
                        Value::Null
                    });
                }
                let pnpm_path = pnpm_paths
                    .as_ref()
                    .and_then(Value::as_array)
                    .and_then(|entries| {
                        entries
                            .iter()
                            .find_map(|entry| entry["dependencies"][name]["path"].as_str())
                            .map(PathBuf::from)
                    })
                    .filter(|path| path.exists());
                if let Some(pnpm_path) = pnpm_path {
                    path = pnpm_path;
                } else {
                    if legacy_root.is_none() {
                        legacy_root = Some(legacy_npm_root(manager).await);
                    }
                    if let Some(Some(root)) = &legacy_root {
                        let legacy = manager.managed_path(root, &[name])?;
                        if legacy.exists() {
                            path = legacy;
                        }
                    }
                }
            }
            let filtered = if entry.is_object() { " (filtered)" } else { "" };
            println!("  {source}{filtered}");
            if path.exists() {
                println!("    {}", path.display());
            }
        }
    }
    Ok(())
}

fn command(program: &str, cwd: &Path) -> Command {
    #[cfg(windows)]
    let program = windows_self_update::resolve_command(program);
    let mut command = Command::new(program);
    command.current_dir(cwd).kill_on_drop(true);
    command
}

async fn captured(mut command: Command) -> Result<Output, String> {
    tokio::time::timeout(COMMAND_TIMEOUT, command.stdin(Stdio::null()).output())
        .await
        .map_err(|_| "Command timed out".to_owned())?
        .map_err(|error| error.to_string())
}

async fn stdout(mut command: Command) -> Option<String> {
    // Unlike tokio::process::Command::output, an explicit pipe also overrides any
    // standard stream policy inherited from callers.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = captured(command).await.ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

async fn run_captured(
    program: &str,
    args: &[impl AsRef<std::ffi::OsStr>],
    cwd: &Path,
) -> Result<(), String> {
    let mut cmd = command(program, cwd);
    cmd.args(args);
    let output = captured(cmd).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "command failed"
    };
    let display_args = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    Err(format!("{program} {display_args}: {detail}"))
}

async fn inherited(
    program: &str,
    args: &[impl AsRef<std::ffi::OsStr>],
    cwd: &Path,
) -> Result<(), String> {
    let status = command(program, cwd)
        .args(args)
        .status()
        .await
        .map_err(|error| format!("{program}: {error}"))?;
    if status.success() {
        return Ok(());
    }
    let display_args = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(code) = status.code() {
        return Err(format!("{program} {display_args} exited with code {code}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal()
            && let Ok(signal) = nix::sys::signal::Signal::try_from(signal)
        {
            return Err(format!(
                "{program} {display_args} terminated by signal {signal}"
            ));
        }
    }
    Err(format!("{program} {display_args} exited with code unknown"))
}

fn npm_capture(manager: &PackageManager, args: &[&str]) -> Command {
    let base = manager.npm_command();
    let mut child = command(&base.command, manager.cwd());
    child.args(base.args).args(args);
    child
}

async fn npm_run(manager: &PackageManager, cwd: &Path, args: Vec<String>) -> Result<(), String> {
    let mut base = manager.npm_command();
    base.args.extend(args);
    inherited(&base.command, &base.args, cwd).await
}

async fn legacy_npm_root(manager: &PackageManager) -> Option<PathBuf> {
    if manager.package_manager_name() == "bun" {
        let bin = PathBuf::from(stdout(npm_capture(manager, &["pm", "bin", "-g"])).await?);
        return Some(
            bin.parent()?
                .join("install")
                .join("global")
                .join("node_modules"),
        );
    }
    stdout(npm_capture(manager, &["root", "-g"]))
        .await
        .map(PathBuf::from)
}

async fn legacy_pnpm_paths(manager: &PackageManager) -> Value {
    stdout(npm_capture(
        manager,
        &["list", "-g", "--depth", "0", "--json"],
    ))
    .await
    .and_then(|output| serde_json::from_str(&output).ok())
    .unwrap_or(Value::Null)
}

fn ensure_ignore(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    if !root.join(".gitignore").exists() {
        fs::write(root.join(".gitignore"), "*\n!.gitignore\n")?;
    }
    Ok(())
}

fn ensure_npm_root(root: &Path) -> io::Result<()> {
    ensure_ignore(root)?;
    if !root.join("package.json").exists() {
        fs::write(
            root.join("package.json"),
            "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}",
        )?;
    }
    Ok(())
}

pub(crate) async fn install_package_captured(
    manager: &PackageManager,
    source: &str,
    parsed: &PackageSource,
    scope: PackageScope,
) -> Result<(), String> {
    match parsed {
        PackageSource::Local { .. } => {
            let target = manager.install_path(parsed, scope)?;
            if !target.exists() {
                return Err(format!("Path does not exist: {}", target.display()));
            }
            Ok(())
        }
        PackageSource::Npm { spec, .. } => {
            manager.install_path(parsed, scope)?;
            if spec.starts_with('-') {
                return Err(format!("Invalid npm package spec: {spec}"));
            }
            ensure_npm_root(&manager.install_root(parsed, scope))
                .map_err(|error| error.to_string())?;
            let plan = manager.install_plan(source, scope)?;
            run_captured(&plan.command, &plan.args, manager.cwd()).await
        }
        PackageSource::Git { git_ref, .. } => {
            let root = manager.install_root(parsed, scope);
            let target = manager.install_path(parsed, scope)?;
            ensure_managed_parent(&root, &target)?;
            if let Some(reference) = git_ref
                .as_deref()
                .filter(|reference| reference.starts_with('-'))
            {
                return Err(format!("Invalid git ref: {reference}"));
            }
            if target.exists() {
                return Ok(());
            }
            ensure_ignore(&root).map_err(|error| error.to_string())?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            remove_file_if_present(&git_marker(&target)).map_err(|error| error.to_string())?;
            let plan = manager.install_plan(source, scope)?;
            let result = async {
                run_captured(&plan.command, &plan.args, manager.cwd()).await?;
                if let Some(reference) = git_ref {
                    run_captured("git", &["checkout", reference], &target).await?;
                }
                if target.join("package.json").exists() {
                    let npm = manager.npm_command();
                    let mut full_args = npm.args;
                    full_args.extend(manager.git_dependency_install_args());
                    run_captured(&npm.command, &full_args, &target).await?;
                }
                Ok(())
            }
            .await;
            if result.is_err() {
                let _ = remove_dir_if_present(&target);
                prune_git_parents(&root, &target);
            }
            result
        }
    }
}

async fn install(
    manager: &PackageManager,
    source: &str,
    parsed: &PackageSource,
    scope: PackageScope,
) -> Result<(), String> {
    match parsed {
        PackageSource::Local { path } if !path.exists() => {
            Err(format!("Path does not exist: {}", path.display()))
        }
        PackageSource::Local { .. } => Ok(()),
        PackageSource::Npm { spec, .. } => {
            // Validate the managed destination before allowing a package manager to write to it.
            manager.install_path(parsed, scope)?;
            if spec.starts_with('-') {
                return Err(format!("Invalid npm package spec: {spec}"));
            }
            ensure_npm_root(&manager.install_root(parsed, scope))
                .map_err(|error| error.to_string())?;
            let plan = manager.install_plan(source, scope)?;
            inherited(&plan.command, &plan.args, manager.cwd()).await
        }
        PackageSource::Git { .. } => install_git(manager, source, parsed, scope)
            .await
            .map(|_| ()),
    }
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

async fn remove(
    manager: &PackageManager,
    parsed: &PackageSource,
    scope: PackageScope,
) -> Result<(), String> {
    match parsed {
        PackageSource::Local { .. } => Ok(()),
        PackageSource::Npm { name, .. } => {
            manager.install_path(parsed, scope)?;
            if name.starts_with('-') {
                return Err(format!("Invalid npm package name: {name}"));
            }
            let root = manager.install_root(parsed, scope);
            if !root.exists() {
                return Ok(());
            }
            let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
            let kind = manager.package_manager_name();
            let prefix = if kind == "bun" { "--cwd" } else { "--prefix" };
            let mut args = vec![
                "uninstall".to_owned(),
                name.clone(),
                prefix.to_owned(),
                root.to_string_lossy().into_owned(),
            ];
            if kind != "bun" && kind != "pnpm" {
                args.extend(["--legacy-peer-deps", "--no-audit", "--no-fund"].map(str::to_owned));
            }
            npm_run(manager, manager.cwd(), args).await
        }
        PackageSource::Git { .. } => {
            let root = manager.install_root(parsed, scope);
            let target = manager.install_path(parsed, scope)?;
            ensure_managed_parent(&root, &target)?;
            remove_dir_if_present(&target).map_err(|error| error.to_string())?;
            remove_file_if_present(&git_marker(&target)).map_err(|error| error.to_string())?;
            prune_git_parents(&root, &target);
            Ok(())
        }
    }
}

fn ensure_managed_parent(root: &Path, target: &Path) -> Result<(), String> {
    // Prevent an existing symlink in a cache path from making a git operation
    // modify a repository outside its managed scope.
    if let Ok(root) = fs::canonicalize(root)
        && let Some(parent) = target.parent()
        && let Some(existing) = parent
            .ancestors()
            .find_map(|path| fs::canonicalize(path).ok())
        && !existing.starts_with(root)
    {
        return Err(format!(
            "Refusing to use path outside package install root: {}",
            target.display()
        ));
    }
    Ok(())
}

fn git_marker(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.pi-update-incomplete"))
}

fn prune_git_parents(root: &Path, target: &Path) {
    if let Some(parent) = target.parent() {
        for candidate in parent
            .ancestors()
            .take_while(|candidate| *candidate != root && candidate.starts_with(root))
        {
            match fs::remove_dir(candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => break,
            }
        }
    }
}

async fn git_read(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut child = command("git", cwd);
    child.args(args);
    stdout(child).await
}

async fn install_git(
    manager: &PackageManager,
    source: &str,
    parsed: &PackageSource,
    scope: PackageScope,
) -> Result<bool, String> {
    let PackageSource::Git { git_ref, .. } = parsed else {
        return Err(format!("Unsupported install source: {source}"));
    };
    let root = manager.install_root(parsed, scope);
    let target = manager.install_path(parsed, scope)?;
    ensure_managed_parent(&root, &target)?;
    if let Some(reference) = git_ref
        .as_deref()
        .filter(|reference| reference.starts_with('-'))
    {
        return Err(format!("Invalid git ref: {reference}"));
    }
    if target.exists() {
        let git_dir = target.join(".git");
        if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_symlink())
            || !fs::symlink_metadata(git_dir).is_ok_and(|meta| meta.is_dir())
        {
            return Err(format!("Not a managed git checkout: {}", target.display()));
        }
        return refresh_git(manager, &target, git_ref.as_deref()).await;
    }
    ensure_ignore(&root).map_err(|error| error.to_string())?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    remove_file_if_present(&git_marker(&target)).map_err(|error| error.to_string())?;
    let plan = manager.install_plan(source, scope)?;
    let result = async {
        inherited(&plan.command, &plan.args, manager.cwd()).await?;
        if let Some(reference) = git_ref {
            inherited("git", &["checkout", reference], &target).await?;
        }
        if target.join("package.json").exists() {
            npm_run(manager, &target, manager.git_dependency_install_args()).await?;
        }
        Ok(false)
    }
    .await;
    if result.is_err() {
        let _ = remove_dir_if_present(&target);
        prune_git_parents(&root, &target);
    }
    result
}

async fn refresh_git(
    manager: &PackageManager,
    target: &Path,
    pinned: Option<&str>,
) -> Result<bool, String> {
    let commit = if let Some(reference) = pinned {
        inherited("git", &["fetch", "origin", reference], target).await?;
        "FETCH_HEAD"
    } else {
        let upstream = git_read(target, &["rev-parse", "--abbrev-ref", "@{upstream}"]).await;
        if let Some(branch) = upstream
            .as_deref()
            .and_then(|name| name.strip_prefix("origin/"))
            .filter(|name| !name.is_empty())
        {
            let spec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
            inherited(
                "git",
                &["fetch", "--prune", "--no-tags", "origin", &spec],
                target,
            )
            .await?;
            "@{upstream}"
        } else {
            let _ = inherited("git", &["remote", "set-head", "origin", "-a"], target).await;
            let branch = git_read(target, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await;
            let spec = branch
                .as_deref()
                .and_then(|reference| reference.strip_prefix("refs/remotes/origin/"))
                .map_or_else(
                    || "+HEAD:refs/remotes/origin/HEAD".to_owned(),
                    |branch| format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                );
            inherited(
                "git",
                &["fetch", "--prune", "--no-tags", "origin", &spec],
                target,
            )
            .await?;
            "origin/HEAD"
        }
    };
    let marker = git_marker(target);
    let target_ref = format!("{commit}^{{commit}}");
    let head = git_read(target, &["rev-parse", "HEAD"]).await;
    let next = git_read(target, &["rev-parse", &target_ref])
        .await
        .ok_or_else(|| format!("Could not resolve {target_ref}"))?;
    let changed = head.as_deref() != Some(next.as_str());
    if changed {
        fs::write(&marker, "").map_err(|error| error.to_string())?;
        inherited("git", &["reset", "--hard", &target_ref], target).await?;
    }
    if changed || marker.exists() {
        let cleaned = match inherited("git", &["clean", "-fdx"], target).await {
            Ok(()) => true,
            Err(error) => {
                println!(
                    "Could not remove all untracked files in {} ({error}). Files may be in use by another pi session; close it and run the update again.",
                    target.display()
                );
                false
            }
        };
        if target.join("package.json").exists() {
            npm_run(manager, target, manager.git_dependency_install_args()).await?;
        }
        if cleaned {
            remove_file_if_present(&marker).map_err(|error| error.to_string())?;
        }
        return Ok(!cleaned);
    }
    if let Some(manifest) = read_json(&target.join("package.json")).and_then(|manifest| {
        manifest
            .get("dependencies")
            .and_then(Value::as_object)
            .cloned()
    }) && manifest.keys().any(|name| {
        let modules = target.join("node_modules");
        manager
            .managed_path(&modules, &[name])
            .is_ok_and(|path| !path.exists())
    }) {
        npm_run(manager, target, manager.git_dependency_install_args()).await?;
    }
    Ok(false)
}

fn read_json(path: &Path) -> Option<Value> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(strip_bom(&contents)).ok()
}

fn npm_version(path: &Path) -> Option<String> {
    read_json(&path.join("package.json"))?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

async fn latest_npm_version(
    manager: &PackageManager,
    spec: &str,
    range: Option<&str>,
) -> Option<String> {
    let response = stdout(npm_capture(manager, &["view", spec, "version", "--json"])).await?;
    let mut value: Value = serde_json::from_str(&response).ok()?;
    if let Value::String(version) = value {
        return Some(version);
    }
    let range = range.and_then(|range| Range::parse(range).ok());
    value
        .as_array_mut()?
        .drain(..)
        .filter_map(|value| {
            let version = value.as_str()?;
            let parsed = Version::parse(version).ok()?;
            (range.as_ref().is_none_or(|range| range.satisfies(&parsed)))
                .then(|| (parsed, version.to_owned()))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, version)| version)
}

struct NpmCandidate<'a> {
    source: &'a str,
    path: PathBuf,
    spec: String,
    before: Option<String>,
    target: Option<String>,
}

async fn update_packages(
    manager: &PackageManager,
    user: &[Value],
    project: &[Value],
    only: Option<&str>,
) -> Result<(), String> {
    let selected = only
        .map(|source| {
            parse_source(source, manager.cwd()).map(|source| source.identity(manager.cwd()))
        })
        .transpose()?;
    let mut found = false;
    let mut suggestion = None;
    let mut warnings = false;
    let mut npm_updates = Vec::new();
    let mut git_updates = Vec::new();
    let mut git_candidates = Vec::new();
    let offline = manager.is_offline();
    for (scope, entries) in [(PackageScope::User, user), (PackageScope::Project, project)] {
        let base = manager.managed_root(scope);
        let mut npm_candidates = Vec::new();
        for source in entries.iter().filter_map(entry_source) {
            let parsed = parse_source(source, &base)?;
            if let Some(identity) = &selected
                && *identity != parsed.identity(&base)
            {
                if suggestion.is_none()
                    && only.is_some_and(|only| matches_shorthand(&parsed, only.trim()))
                {
                    suggestion = Some(source);
                }
                continue;
            }
            found = true;
            if offline {
                continue;
            }
            match &parsed {
                PackageSource::Npm {
                    spec,
                    name,
                    version,
                    ..
                } if !version
                    .as_deref()
                    .is_some_and(|version| Version::parse(version.trim()).is_ok()) =>
                {
                    if spec.starts_with('-') {
                        return Err(format!("Invalid npm package spec: {spec}"));
                    }
                    let path = manager.install_path(&parsed, scope)?;
                    let before = npm_version(&path);
                    let target = if before.is_some() {
                        latest_npm_version(
                            manager,
                            if version.is_some() { spec } else { name },
                            version.as_deref(),
                        )
                        .await
                    } else {
                        None
                    };
                    if let (Some(target), Some(before)) = (&target, &before)
                        && crate::utils::version_check::compare_package_versions(target, before)
                            .is_some_and(|order| order <= 0)
                    {
                        continue;
                    }
                    let spec = if version.is_some() {
                        spec.clone()
                    } else {
                        format!("{name}@latest")
                    };
                    npm_candidates.push(NpmCandidate {
                        source,
                        path,
                        spec,
                        before,
                        target,
                    });
                }
                PackageSource::Git { .. } => git_candidates.push((source, parsed, scope)),
                _ => {}
            }
        }
        if !npm_candidates.is_empty() {
            let root = base.join("npm");
            ensure_npm_root(&root).map_err(|error| error.to_string())?;
            if npm_candidates.len() == 1 {
                println!("Updating {}...", npm_candidates[0].source);
            } else {
                println!("Updating {} npm packages...", scope.as_str());
            }
            let specs: Vec<_> = npm_candidates
                .iter_mut()
                .map(|entry| std::mem::take(&mut entry.spec))
                .collect();
            npm_run(
                manager,
                manager.cwd(),
                manager.npm_install_args(&specs, &root),
            )
            .await?;
            for entry in npm_candidates {
                let after = npm_version(&entry.path);
                if entry.before != after {
                    npm_updates.push(update_message(
                        entry.source,
                        scope,
                        false,
                        entry.before.as_deref(),
                        after.as_deref(),
                    ));
                } else if let (Some(before), Some(target)) = (entry.before, entry.target) {
                    println!(
                        "{} is still {before} (registry reports {target}); npm registry metadata may be stale, try again in a few minutes",
                        entry.source
                    );
                    warnings = true;
                }
            }
        }
    }
    if let Some(source) = only
        && !found
    {
        return Err(match suggestion {
            Some(suggestion) => {
                format!("No matching package found for {source}. Did you mean {suggestion}?")
            }
            None => format!("No matching package found for {source}"),
        });
    }
    for (source, parsed, scope) in git_candidates {
        let path = manager.install_path(&parsed, scope)?;
        let before = if path.is_dir() {
            git_read(&path, &["rev-parse", "HEAD"]).await
        } else {
            None
        };
        println!("Updating {source}...");
        warnings |= install_git(manager, source, &parsed, scope).await?;
        let after = git_read(&path, &["rev-parse", "HEAD"]).await;
        if before != after {
            git_updates.push(update_message(
                source,
                scope,
                true,
                before.as_deref(),
                after.as_deref(),
            ));
        }
    }
    let count = npm_updates.len() + git_updates.len();
    if offline {
        println!("Skipped package updates (offline mode)");
    } else if count > 0 {
        let suffix = if count == 1 { "" } else { "s" };
        println!("Updated {count} package{suffix}:");
        for message in npm_updates.into_iter().chain(git_updates) {
            println!("  {message}");
        }
    } else if warnings {
        if let Some(source) = only {
            println!("{source} was not updated, see the warnings above");
        } else {
            println!("No packages were updated, see the warnings above");
        }
    } else if let Some(source) = only {
        println!("{source} is already up to date");
    } else {
        println!("Packages are already up to date");
    }
    Ok(())
}

fn matches_shorthand(parsed: &PackageSource, input: &str) -> bool {
    match parsed {
        PackageSource::Npm { name, spec, .. } => input == name || input == spec,
        PackageSource::Git {
            host,
            path,
            git_ref,
            ..
        } => {
            let name = format!("{host}/{path}");
            input == name
                || git_ref.as_deref().is_some_and(|reference| {
                    input
                        .strip_prefix(&name)
                        .and_then(|ending| ending.strip_prefix('@'))
                        == Some(reference)
                })
        }
        PackageSource::Local { .. } => false,
    }
}

fn update_message(
    source: &str,
    scope: PackageScope,
    git: bool,
    from: Option<&str>,
    to: Option<&str>,
) -> String {
    fn label(value: Option<&str>, git: bool) -> &str {
        let value = value.unwrap_or("(none)");
        if git {
            value.get(..7).unwrap_or(value)
        } else {
            value
        }
    }
    let scope = if scope == PackageScope::Project {
        " (project)"
    } else {
        ""
    };
    format!("{source}{scope} {} -> {}", label(from, git), label(to, git))
}

const PI_RS_MANAGED_MARKER: &str = "pi-rs-install.json";

fn active_pi_rs_managed_root() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    pi_rs_managed_root_for_exe(&exe)
}

fn pi_rs_managed_root_for_exe(exe: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    for ancestor in canonical.ancestors().skip(1).take(5) {
        let marker_path = ancestor.join(PI_RS_MANAGED_MARKER);
        let Some(marker) = read_json(&marker_path) else {
            continue;
        };
        let valid = marker["kind"] == "pi-rs-managed-install"
            && marker["schemaVersion"] == 1
            && marker["layout"] == "releases-v1";
        if !valid {
            continue;
        }
        let releases = ancestor.join("releases");
        let releases = fs::canonicalize(&releases).unwrap_or(releases);
        if canonical.starts_with(&releases) {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn cleanup_pi_rs_staging(root: &Path) {
    if let Ok(entries) = fs::read_dir(root.join("staging")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("install-") {
                let _ = remove_dir_if_present(&entry.path());
            }
        }
    }
}

fn pi_rs_self_update_command_for_marker(marker: Option<&Value>, windows: bool) -> String {
    let side_by_side = marker
        .and_then(|value| value["installMode"].as_str())
        .is_some_and(|mode| mode == "side-by-side");
    if windows {
        if side_by_side {
            "$env:PI_AS_PI_RS = \"1\"; powershell -c \"irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex\""
                .to_owned()
        } else {
            "powershell -c \"irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex\""
                .to_owned()
        }
    } else if side_by_side {
        "curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- --as-pi-rs"
            .to_owned()
    } else {
        "curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh"
            .to_owned()
    }
}

/// Remove staging directories left by failed managed updates and, on Windows,
/// native-addon quarantine left by an earlier npm-distributed Pi.
pub fn cleanup_installation() {
    #[cfg(windows)]
    if let Ok(package_dir) = config::get_package_dir() {
        windows_self_update::cleanup(&package_dir);
    }
    if let Some(root) = active_pi_rs_managed_root() {
        cleanup_pi_rs_staging(&root);
    }
    if let Ok(Some(root)) = active_managed_root()
        && let Ok(_guard) = managed_lock(&root)
    {
        cleanup_staging(&root);
    }
}

fn active_managed_root() -> Result<Option<PathBuf>, String> {
    let Ok(value) = env::var("PI_MANAGED_INSTALL_ROOT") else {
        return Ok(None);
    };
    let root = value.trim();
    if root.is_empty() {
        return Ok(None);
    }
    managed_root_at(
        &std::path::absolute(root).map_err(|error| error.to_string())?,
        &config::get_package_dir()?,
    )
}

fn managed_root_at(root: &Path, package_dir: &Path) -> Result<Option<PathBuf>, String> {
    let releases = root.join("releases");
    let releases = fs::canonicalize(&releases).unwrap_or(releases);
    let package = fs::canonicalize(package_dir).unwrap_or_else(|_| package_dir.to_path_buf());
    if !package.starts_with(releases) {
        return Ok(None);
    }
    let marker = root.join("managed-install.json");
    let marker_json = read_json(&marker).filter(|marker| {
        marker["kind"] == "pi-managed-install"
            && marker["schemaVersion"] == 1
            && marker["layout"] == "releases-v1"
    });
    let Some(marker_json) = marker_json else {
        return Err(format!(
            "Managed install marker is missing or invalid: {}",
            marker.display()
        ));
    };
    #[cfg(not(windows))]
    if let Some(entrypoint) = marker_json
        .pointer("/entrypoint/path")
        .and_then(Value::as_str)
        && ((entrypoint.len() >= 3
            && entrypoint.as_bytes()[0].is_ascii_alphabetic()
            && entrypoint.as_bytes()[1] == b':'
            && entrypoint.as_bytes()[2] == b'\\')
            || entrypoint.starts_with("\\\\"))
    {
        return Err(format!(
            "This is the Windows installation of {}, run from WSL. Running the Windows installation from WSL is unsupported, so it cannot update itself here. Update it from Windows, or install {} in WSL.",
            config::APP_NAME,
            config::APP_NAME
        ));
    }
    let _ = marker_json;
    Ok(Some(root.to_path_buf()))
}

fn valid_managed_version(version: &str) -> bool {
    let valid_suffix = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
    };
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(core, build)| (core, Some(build)));
    let (version, pre) = version
        .split_once('-')
        .map_or((version, None), |(core, pre)| (core, Some(pre)));
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    let mut parts = version.split('.');
    (&mut parts).take(3).filter(|part| is_number(part)).count() == 3
        && parts.next().is_none()
        && pre.is_none_or(valid_suffix)
        && build.is_none_or(valid_suffix)
}

fn cleanup_staging(root: &Path) {
    if let Ok(entries) = fs::read_dir(root.join("staging")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("update-") {
                let _ = remove_dir_if_present(&entry.path());
            }
        }
    }
}

fn managed_lock(root: &Path) -> Result<LockGuard, String> {
    lock_sync(
        &root.join("update"),
        &LockOptions {
            realpath: false,
            ..LockOptions::default()
        },
    )
    .map_err(|error| {
        if error == "ELOCKED" {
            "Another managed Pi update is already running.".to_owned()
        } else {
            error
        }
    })
}

async fn managed_update(root: &Path, version: &str) -> Result<(), String> {
    if !valid_managed_version(version) {
        return Err(format!("Invalid managed release version: {version}"));
    }
    let guard = managed_lock(root)?;
    let update = stage_managed_release(root, version);
    tokio::pin!(update);
    loop {
        tokio::select! {
            result = &mut update => return result,
            () = tokio::time::sleep(Duration::from_secs(2)) => {
                let heartbeat = guard.lock_path().join(".pi-update-heartbeat");
                fs::write(&heartbeat, "").and_then(|()| fs::remove_file(heartbeat)).map_err(|error| error.to_string())?;
            }
        }
    }
}

struct StagedRelease(PathBuf);

impl Drop for StagedRelease {
    fn drop(&mut self) {
        let _ = remove_dir_if_present(&self.0);
    }
}

async fn stage_managed_release(root: &Path, version: &str) -> Result<(), String> {
    cleanup_staging(root);
    let releases = root.join("releases");
    fs::create_dir_all(&releases).map_err(|error| error.to_string())?;
    let release = releases.join(version);
    if release.exists() {
        verify_managed_release(&release, version).await?;
    } else {
        let staging = root.join("staging");
        fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
        let stage = staging.join(format!("update-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&stage).map_err(|error| error.to_string())?;
        let stage = StagedRelease(stage);
        let installer = env::var("PI_INSTALLER_API_BASE").ok();
        let api = installer
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .unwrap_or(INSTALLER_API_BASE);
        let base = format!(
            "{}/{}",
            api.trim_end_matches('/'),
            version.replace('+', "%2B")
        );
        let client = reqwest::Client::builder()
            .user_agent(pi_user_agent())
            .build()
            .map_err(|error| error.to_string())?;
        let (manifest, lock) = tokio::try_join!(
            fetch_artifact(&client, &base, "package.json"),
            fetch_artifact(&client, &base, "package-lock.json")
        )?;
        fs::write(stage.0.join("package.json"), manifest).map_err(|error| error.to_string())?;
        fs::write(stage.0.join("package-lock.json"), lock).map_err(|error| error.to_string())?;
        inherited(
            "npm",
            &[
                "ci",
                "--ignore-scripts",
                "--omit=dev",
                "--include=optional",
                "--no-fund",
                "--no-audit",
                "--loglevel=error",
                "--progress=false",
            ],
            &stage.0,
        )
        .await?;
        verify_managed_release(&stage.0, version).await?;
        fs::rename(&stage.0, &release).map_err(|error| error.to_string())?;
    }
    activate_managed_release(root, version).map_err(|error| error.to_string())?;
    if let Ok(entries) = fs::read_dir(releases) {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string()
                && name != version
                && name != VERSION
                && valid_managed_version(&name)
            {
                let _ = remove_dir_if_present(&entry.path());
            }
        }
    }
    Ok(())
}

async fn fetch_artifact(
    client: &reqwest::Client,
    base: &str,
    name: &str,
) -> Result<String, String> {
    let url = format!("{base}/{name}");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download managed installer {name} from {url}: HTTP {}",
            response.status().as_u16()
        ));
    }
    response.text().await.map_err(|error| error.to_string())
}

fn activate_managed_release(root: &Path, version: &str) -> io::Result<()> {
    let temporary = root.join(format!(
        "current-version.tmp.{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = fs::write(&temporary, format!("{version}\n"))
        .and_then(|()| fs::rename(&temporary, root.join("current-version")));
    if result.is_err() {
        let _ = remove_file_if_present(&temporary);
    }
    result
}

async fn verify_managed_release(release: &Path, version: &str) -> Result<(), String> {
    let shim = if cfg!(windows) { "pi.cmd" } else { "pi" };
    let binary = release.join("node_modules").join(".bin").join(shim);
    let mut child = command(&binary.to_string_lossy(), release);
    child.arg("--version").env_remove("PI_PACKAGE_DIR");
    let output = captured(child)
        .await
        .map_err(|error| format!("Could not verify managed Pi {version}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = if stderr.trim().is_empty() {
            format!(
                "exit code {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            )
        } else {
            stderr.trim().to_owned()
        };
        return Err(format!("Could not verify managed Pi {version}: {reason}"));
    }
    let installed = String::from_utf8_lossy(&output.stdout);
    if installed.trim() != version {
        return Err(format!(
            "Managed Pi smoke test returned version {}; expected {version}.",
            installed.trim()
        ));
    }
    Ok(())
}

fn release_is_update(release: &LatestPiRelease, force: bool) -> bool {
    force
        || release
            .package_name
            .as_deref()
            .is_some_and(|name| name != PACKAGE_NAME)
        || is_newer_package_version(&release.version, VERSION)
}

fn print_update_note(note: Option<&str>) {
    use pi_rs_tui::{Component, DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
    let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) else {
        return;
    };
    let plain: pi_rs_tui::components::TextStyleFn = std::rc::Rc::new(str::to_owned);
    let theme = MarkdownTheme {
        bold: plain.clone(),
        italic: plain.clone(),
        strikethrough: plain.clone(),
        underline: plain,
        ..MarkdownTheme::default()
    };
    let width = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80)
        .max(20);
    let note = Markdown::new(
        note,
        0,
        0,
        theme,
        DefaultTextStyle::default(),
        MarkdownOptions {
            render_latex: false,
            ..Default::default()
        },
    );
    println!("\nUpdate note");
    for line in note.render(width) {
        println!("{}", line.trim_end());
    }
    println!();
}

async fn execute_self_update(command: &SelfUpdateCommand, cwd: &Path) -> Result<(), String> {
    println!("Updating pi with {}...", command.display());
    for step in std::iter::once(&command.install).chain(command.cleanup.as_ref()) {
        inherited(&step.command, &step.args, cwd).await?;
    }
    Ok(())
}

fn is_below(path: &Path, root: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path != root && path.starts_with(root),
        _ => false,
    }
}

fn writable_dir(path: &Path) -> bool {
    let temporary = path.join(format!(".pi-update-access-{}", uuid::Uuid::new_v4()));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .is_ok()
        && fs::remove_file(temporary).is_ok()
}

fn inferred_npm_root(package_dir: &Path) -> Option<&Path> {
    let parent = package_dir.parent()?;
    let root = if parent.file_name()?.to_string_lossy().starts_with('@') {
        parent.parent()?
    } else {
        parent
    };
    (root.file_name()? == "node_modules" && root.parent()?.file_name()? == "lib").then_some(root)
}

fn inferred_pnpm_root(package_dir: &Path) -> Option<&Path> {
    let store = package_dir
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == ".pnpm"))?;
    let root = store.parent()?;
    (root.parent()?.file_name()? == "global").then_some(root)
}

async fn global_self_update_roots(
    method: &str,
    manager: &PackageManager,
    package_dir: &Path,
    configured_npm: bool,
) -> Result<Vec<PathBuf>, String> {
    let cwd = manager.cwd();
    let mut roots = Vec::new();
    match method {
        "npm" if manager.package_manager_name() != "bun" => {
            let output = captured(npm_capture(manager, &["root", "-g"])).await;
            match output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if !stdout.trim().is_empty() {
                        roots.push(PathBuf::from(stdout.trim()));
                    }
                }
                failed if configured_npm => {
                    let reason = match failed {
                        Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                        Err(error) => error,
                    };
                    return Err(format!(
                        "Failed to run {} root -g: {reason}",
                        manager.npm_command().argv().join(" ")
                    ));
                }
                _ => {}
            }
            if !configured_npm && let Some(root) = inferred_npm_root(package_dir) {
                roots.push(root.to_path_buf());
            }
        }
        "pnpm" => {
            let mut child = command("pnpm", cwd);
            child.args(["root", "-g"]);
            if let Some(output) = stdout(child).await {
                let root = PathBuf::from(output);
                if let Some(parent) = root.parent() {
                    roots.push(parent.to_path_buf());
                }
                roots.push(root);
            } else if let Some(root) = inferred_pnpm_root(package_dir) {
                roots.push(root.to_path_buf());
            }
        }
        "yarn" => {
            let mut child = command("yarn", cwd);
            child.args(["global", "dir"]);
            if let Some(output) = stdout(child).await {
                let root = PathBuf::from(output);
                roots.push(root.join("node_modules"));
                roots.push(root);
            }
        }
        "bun" | "npm" => {
            if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
                roots.push(
                    PathBuf::from(home)
                        .join(".bun")
                        .join("install/global/node_modules"),
                );
            }
            let output = if method == "npm" {
                stdout(npm_capture(manager, &["pm", "bin", "-g"])).await
            } else {
                let mut child = command("bun", cwd);
                child.args(["pm", "bin", "-g"]);
                stdout(child).await
            };
            if let Some(bin) = output
                && let Some(parent) = Path::new(&bin).parent()
            {
                roots.push(parent.join("install/global/node_modules"));
            }
        }
        _ => {}
    }
    Ok(roots)
}

async fn update_self(manager: &PackageManager, global: &Value, force: bool) -> Result<u8, String> {
    let configured = configured_self_update_argv(global);
    if configured.is_none()
        && let Some(pi_rs_root) = active_pi_rs_managed_root()
    {
        let marker = read_json(&pi_rs_root.join(PI_RS_MANAGED_MARKER));
        let installer_cmd = pi_rs_self_update_command_for_marker(marker.as_ref(), cfg!(windows));
        eprintln!(
            "This pi-rs installation is managed natively in {}.",
            pi_rs_root.display()
        );
        eprintln!("Update or reinstall pi-rs with:\n\n  {installer_cmd}\n");
        eprintln!(
            "To make `pi update` run your own update command, set PI_SELF_UPDATE_COMMAND or the selfUpdateCommand setting."
        );
        return Ok(1);
    }
    let managed = if configured.is_some() {
        None
    } else {
        active_managed_root()?
    };
    if managed.is_some() && force {
        eprintln!(
            "Managed pi installations do not support --force; rerun the installer to repair this installation."
        );
        return Ok(1);
    }
    let latest = get_latest_pi_release(
        VERSION,
        VersionCheckOptions {
            retry: true,
            ..VersionCheckOptions::default()
        },
    )
    .await
    .map_err(|error| format!("Could not determine latest pi version: {error}"))?
    .ok_or_else(|| "Could not determine latest pi version.".to_owned())?;
    if !release_is_update(&latest, force) {
        println!("pi is already up to date (v{VERSION})");
        return Ok(0);
    }
    let version = &latest.version;
    let package_name = latest.package_name.as_deref().unwrap_or(PACKAGE_NAME);
    if let Some(argv) = configured {
        let command = make_configured_self_update(&argv, package_name, version);
        print_update_note(latest.note.as_deref());
        execute_self_update(&command, manager.cwd()).await?;
        let installed = match config::detect_install_change().ok().flatten() {
            Some(config::InstallChange::Updated { version: installed })
                if installed == *version =>
            {
                Some(installed)
            }
            _ => {
                let mut command = self::command("pi", manager.cwd());
                command.arg("--version").env_remove("PI_PACKAGE_DIR");
                stdout(command).await
            }
        };
        if installed.as_deref() != Some(version.as_str()) {
            println!(
                "Ran {}, but `pi --version` reports {}, not {version}.",
                command.display(),
                installed.as_deref().unwrap_or("no version")
            );
            return Ok(0);
        }
        println!("Updated pi from {VERSION} to {version}");
        return Ok(0);
    }
    if let Some(root) = managed {
        print_update_note(latest.note.as_deref());
        println!("Updating managed pi installation...");
        managed_update(&root, version).await?;
        println!("Updated pi from {VERSION} to {version}");
        return Ok(0);
    }
    let method = config::detect_install_method()?;
    if cfg!(windows) && method != "npm" && method != "pnpm" {
        eprintln!("pi self-update on Windows is only supported for npm and pnpm installs.");
        eprintln!("Detected install method: {method}. Update pi manually.");
        return Ok(1);
    }
    let install_spec = format!("{package_name}@{version}");
    let package_dir = config::get_package_dir()?;
    let configured_npm = global
        .get("npmCommand")
        .and_then(Value::as_array)
        .is_some_and(|args| !args.is_empty());
    let mut npm = manager.npm_command().argv();
    if method == "npm"
        && !configured_npm
        && let Some(prefix) = inferred_npm_root(&package_dir)
            .and_then(Path::parent)
            .and_then(Path::parent)
    {
        npm.extend(["--prefix".to_owned(), prefix.to_string_lossy().into_owned()]);
    }
    let command = self_update_command_for_method(&method, &npm, &install_spec, PACKAGE_NAME);
    let roots = global_self_update_roots(&method, manager, &package_dir, configured_npm).await?;
    let globally_managed = roots.iter().any(|root| is_below(&package_dir, root));
    let is_writable = globally_managed
        && writable_dir(&package_dir)
        && package_dir.parent().is_some_and(writable_dir);
    if let Some(command) = &command
        && is_writable
    {
        print_update_note(latest.note.as_deref());
        #[cfg(windows)]
        if method == "npm" {
            // The native Rust executable does not load Node add-ons; only a
            // leftover quarantine from a previous npm-distributed Pi can exist.
            windows_self_update::cleanup(&package_dir);
        }
        if let Err(error) = execute_self_update(command, manager.cwd()).await {
            eprintln!("Error: {error}");
            if method == "pnpm" {
                eprintln!(
                    "If pnpm reports missing package versions, its cached registry metadata may be stale."
                );
                eprintln!("Run `pnpm store prune` and retry `pi update --self`.");
            }
            eprintln!(
                "If this keeps failing, run this command yourself: {}",
                command.display()
            );
            return Ok(1);
        }
        println!("Updated pi from {VERSION} to {version}");
        if method == "npm" {
            let installer = if cfg!(windows) {
                "powershell -c \"irm https://pi.dev/install.ps1 | iex\""
            } else {
                "curl -fsSL https://pi.dev/install.sh | sh"
            };
            println!(
                "\nThis npm installation of pi does not pin its dependencies.\nRun the installer to migrate to a managed installation that does:\n\n  {installer}"
            );
        }
        return Ok(0);
    }
    eprintln!("error: pi cannot self-update this installation.");
    if let Some(command) = command.filter(|_| globally_managed) {
        eprintln!(
            "This installation is managed by a global {method} install, but the install path is not writable. Update it yourself with: {}",
            command.display()
        );
    } else {
        eprintln!(
            "Update {install_spec} using the package manager, wrapper, or source checkout that provides this installation."
        );
    }
    eprintln!(
        "To make `pi update` run your own update command, set PI_SELF_UPDATE_COMMAND or the selfUpdateCommand setting."
    );
    if let Ok(executable) = env::current_exe() {
        eprintln!("\nLocation of pi executable: {}", executable.display());
    }
    Ok(1)
}

fn pi_user_agent() -> String {
    use crate::utils::pi_user_agent::{PiUserAgentOptions, get_pi_user_agent};
    get_pi_user_agent(VERSION, &PiUserAgentOptions::default())
}

type CatalogStore = indexmap::IndexMap<String, pi_rs_ai::models_store::ModelsStoreEntry>;

fn read_model_catalogs(path: &Path) -> Result<CatalogStore, String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            serde_json::from_str(strip_bom(&contents)).map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(CatalogStore::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn merge_models(
    builtin: impl IntoIterator<Item = pi_rs_ai::AnyModel>,
    updated: impl IntoIterator<Item = pi_rs_ai::AnyModel>,
) -> Vec<pi_rs_ai::AnyModel> {
    let mut merged = indexmap::IndexMap::new();
    for model in builtin.into_iter().chain(updated) {
        merged.insert((model.model_type(), model.id().to_owned()), model);
    }
    merged.into_values().collect()
}

/// Restore refreshed catalogs before applying `models.json` overrides.
pub fn restore_model_catalogs(models: &pi_rs_ai::Models, agent_dir: &Path) -> Result<(), String> {
    for (id, entry) in read_model_catalogs(&agent_dir.join("models-store.json"))? {
        let Some(mut provider) = models.get_provider(&id) else {
            continue;
        };
        let remote = entry
            .models
            .into_iter()
            .filter(|model| model.provider() == id);
        let selected: Vec<_> = if id == "openrouter" || id == "radius" {
            remote.collect()
        } else if entry.last_modified != Some(0) {
            merge_models(provider.models.iter().cloned(), remote)
        } else {
            continue;
        };
        if !selected.is_empty() {
            provider.models = selected.into();
            models.set_provider(provider);
        }
    }
    Ok(())
}

fn persist_model_catalogs(path: &Path, entries: CatalogStore) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let _guard = lock_sync(
        path,
        &LockOptions {
            stale_ms: 30_000,
            realpath: false,
            retries: 7,
            ..LockOptions::default()
        },
    )?;
    let mut stored = read_model_catalogs(path)?;
    stored.extend(entries);
    let temp = path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
    use std::io::Write;
    let result = (|| -> Result<(), String> {
        let mut permission = fs::OpenOptions::new();
        permission.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            permission.mode(0o600);
        }
        let mut file = permission.open(&temp).map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(&mut file, &stored).map_err(|error| error.to_string())?;
        file.flush().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temp, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = remove_file_if_present(&temp);
    }
    result
}

fn decode_catalog(id: &str, value: Value) -> Result<Vec<pi_rs_ai::AnyModel>, pi_rs_ai::Error> {
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(mut entries) => match entries.shift_remove("models") {
            Some(Value::Array(models)) => models,
            _ => entries.into_iter().map(|(_, model)| model).collect(),
        },
        _ => {
            return Err(pi_rs_ai::Error::ModelSource(format!(
                "Invalid model catalog for provider \"{id}\""
            )));
        }
    };
    let mut models = Vec::new();
    for entry in entries {
        if let Value::Object(mut entry) = entry
            && entry.get("id").is_some_and(Value::is_string)
            && entry
                .get("type")
                .is_none_or(|kind| matches!(kind.as_str(), Some("chat" | "image" | "classifier")))
        {
            entry.insert("provider".to_owned(), Value::String(id.to_owned()));
            models.push(serde_json::from_value(Value::Object(entry))?);
        }
    }
    Ok(models)
}

async fn request_catalog(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    etag: Option<&str>,
) -> Result<reqwest::Response, pi_rs_ai::Error> {
    let mut url = url::Url::parse(base)?;
    url.path_segments_mut()
        .map_err(|()| pi_rs_ai::Error::ModelSource("Invalid catalog URL".to_owned()))?
        .clear()
        .extend(["api", "models", "providers", id]);
    url.query_pairs_mut()
        .clear()
        .append_pair("types", "chat,image,classifier");
    let mut request = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(4));
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    for _ in 0..2 {
        let Some(next) = request.try_clone() else {
            break;
        };
        if let Ok(response) = next.send().await
            && !crate::utils::management_http::RETRYABLE_STATUS_CODES
                .contains(&response.status().as_u16())
        {
            return Ok(response);
        }
    }
    request.send().await.map_err(|error| {
        pi_rs_ai::Error::ModelSource(format!(
            "Model catalog request failed: {}",
            error.without_url()
        ))
    })
}

async fn refresh_pi_catalog(
    client: &reqwest::Client,
    id: &str,
    cached: Option<&pi_rs_ai::models_store::ModelsStoreEntry>,
) -> Result<pi_rs_ai::models_store::ModelsStoreEntry, pi_rs_ai::Error> {
    use pi_rs_ai::models_store::ModelsStoreEntry;
    let validator = cached
        .filter(|entry| !entry.models.is_empty())
        .and_then(|entry| entry.etag.as_deref());
    let response = request_catalog(client, "https://pi.dev", id, validator).await?;
    let checked_at = Some(unix_millis());
    match response.status().as_u16() {
        304 if cached.is_some() => {
            let mut entry = cached.cloned().unwrap_or_default();
            entry.checked_at = checked_at;
            Ok(entry)
        }
        404 | 501 => Ok(ModelsStoreEntry {
            checked_at,
            last_modified: Some(0),
            ..ModelsStoreEntry::default()
        }),
        status if !response.status().is_success() => Err(pi_rs_ai::Error::ModelSource(format!(
            "Model catalog request failed for {id}: {status}"
        ))),
        _ => {
            let headers = response.headers();
            let last_modified = headers
                .get("Last-Modified")
                .and_then(|header| header.to_str().ok())
                .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
                .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
                .unwrap_or_default();
            let etag = headers
                .get("ETag")
                .and_then(|header| header.to_str().ok())
                .map(str::to_owned);
            let body = response.json().await.map_err(|error| {
                pi_rs_ai::Error::ModelSource(format!(
                    "Invalid model catalog response: {}",
                    error.without_url()
                ))
            })?;
            let models = decode_catalog(id, body)?;
            Ok(ModelsStoreEntry {
                models,
                checked_at,
                last_modified: Some(last_modified),
                etag,
            })
        }
    }
}

async fn personalize_openrouter(
    client: &reqwest::Client,
    provider: &pi_rs_ai::Provider,
    models: Vec<pi_rs_ai::AnyModel>,
    key: Option<&str>,
) -> Result<Vec<pi_rs_ai::AnyModel>, pi_rs_ai::Error> {
    use pi_rs_ai::{AnyModel, Error};
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return Ok(models);
    };
    let base = provider
        .base_url
        .as_deref()
        .unwrap_or("https://openrouter.ai/api/v1");
    let response = client
        .get(format!("{}/models/user", base.trim_end_matches('/')))
        .header("Accept", "application/json")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| {
            Error::ModelSource(format!(
                "OpenRouter model list request failed: {}",
                error.without_url()
            ))
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return Ok(models);
    }
    if !status.is_success() {
        return Err(Error::ModelSource(format!(
            "OpenRouter model list request failed: {}",
            status.as_u16()
        )));
    }
    let mut response: Value = response.json().await.map_err(|error| {
        Error::ModelSource(format!(
            "Invalid OpenRouter model list response: {}",
            error.without_url()
        ))
    })?;
    let allowed = response
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::ModelSource("Invalid OpenRouter model list response".to_owned()))?;
    let allowed: std::collections::HashMap<_, _> = allowed
        .iter()
        .filter_map(|item| Some((item.get("id")?.as_str()?, item)))
        .collect();
    Ok(models
        .into_iter()
        .filter_map(|mut model| {
            if let AnyModel::Chat(chat) = &mut model {
                let entry = allowed.get(chat.id.as_str())?;
                let nonzero = |value: &Value| value.as_u64().filter(|value| *value > 0);
                if let Some(context) = nonzero(&entry["top_provider"]["context_length"])
                    .or_else(|| nonzero(&entry["context_length"]))
                {
                    chat.context_window = context;
                }
                if let Some(maximum) = nonzero(&entry["top_provider"]["max_completion_tokens"]) {
                    chat.max_tokens = maximum;
                }
                if let Some(pricing) = entry.get("pricing").filter(|pricing| !pricing.is_null()) {
                    chat.cost = pi_rs_ai::providers::openrouter_cost(pricing);
                }
            }
            Some(model)
        })
        .collect())
}

async fn refresh_one_catalog(
    client: &reqwest::Client,
    models: &pi_rs_ai::Models,
    provider: &pi_rs_ai::Provider,
    stored: Option<&pi_rs_ai::models_store::ModelsStoreEntry>,
    auth: &crate::core::auth_storage::AuthStorage,
    live_models: &pi_rs_ai::models_store::InMemoryModelsStore,
) -> Result<Option<pi_rs_ai::models_store::ModelsStoreEntry>, pi_rs_ai::Error> {
    use pi_rs_ai::{models_store::ModelsStore, registry::RefreshOptions};
    let id = provider.id.as_str();
    if provider.source.is_some() {
        let options = RefreshOptions {
            providers: Some(vec![id.to_owned()]),
            ..RefreshOptions::default()
        };
        let signal = options.signal.clone();
        if let Some(stored) = stored {
            live_models.write(id, stored.clone(), &signal).await?;
        }
        if let Some(error) = models.refresh(options).await.errors.shift_remove(id) {
            return Err(error);
        }
        return live_models.read(id, &signal).await;
    }
    if id != "openrouter" {
        return refresh_pi_catalog(client, id, stored).await.map(Some);
    }
    let openrouter = auth.get(id);
    let credential = match openrouter {
        Some(pi_rs_ai::auth::Credential::ApiKey(key)) => key.key,
        _ => models
            .get_auth(id, &pi_rs_ai::auth::AuthOverrides::default())
            .await?
            .and_then(|auth| auth.auth.api_key),
    };
    let remote = refresh_pi_catalog(client, id, None).await?;
    let remote = if remote.last_modified == Some(0) {
        Vec::new()
    } else {
        remote.models
    };
    let available = merge_models(provider.models.iter().cloned(), remote);
    let models = personalize_openrouter(client, provider, available, credential.as_deref()).await?;
    Ok(Some(pi_rs_ai::models_store::ModelsStoreEntry {
        models,
        checked_at: Some(unix_millis()),
        ..Default::default()
    }))
}

fn unix_millis() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}

pub(crate) async fn refresh_model_catalogs_background(
    agent_dir: &Path,
    settings: &SettingsManager,
) -> Result<(), String> {
    refresh_model_catalogs_inner(agent_dir, settings, false).await
}

async fn refresh_models(agent_dir: &Path, settings: &SettingsManager) -> Result<(), String> {
    refresh_model_catalogs_inner(agent_dir, settings, true).await
}

async fn refresh_model_catalogs_inner(
    agent_dir: &Path,
    settings: &SettingsManager,
    emit_output: bool,
) -> Result<(), String> {
    use crate::core::{auth_storage::AuthStorage, model_registry::ModelRegistry};
    use pi_rs_ai::{auth::FileCredentialStore, models_store::InMemoryModelsStore};
    use std::sync::Arc;

    let catalog_path = agent_dir.join("models-store.json");
    let store = read_model_catalogs(&catalog_path)?;
    let auth_path = agent_dir.join("auth.json");
    let auth = AuthStorage::create(auth_path.clone());
    let client = crate::core::http_dispatcher::client(settings)?;
    let live_models = Arc::new(InMemoryModelsStore::default());
    let options = pi_rs_ai::ModelsOptions {
        credentials: Arc::new(FileCredentialStore::new(auth_path)),
        store: live_models.clone(),
        ..Default::default()
    };
    let models = pi_rs_ai::Models::builtin(options).map_err(|error| error.to_string())?;
    let builtin = models.get_providers();
    let configured = ModelRegistry::new(models, Some(agent_dir.join("models.json")));
    let models = configured.models();
    // A configuration overlay can alter endpoint and auth without replacing the
    // public remote catalog or saving configured headers into the shared cache.
    let requests = builtin.into_iter().map(|mut provider| {
        let client = &client;
        let store = &store;
        let auth = &auth;
        let live_models = &live_models;
        async move {
            if let Some(configured) = models.get_provider(&provider.id) {
                provider.base_url = configured.base_url;
            }
            let id = provider.id.clone();
            let result =
                refresh_one_catalog(client, models, &provider, store.get(&id), auth, live_models)
                    .await;
            (id, result)
        }
    });
    let result = tokio::time::timeout(
        MODEL_REFRESH_TIMEOUT,
        futures_util::future::join_all(requests),
    )
    .await
    .map_err(|_| "Model catalog refresh timed out.".to_owned())?;
    let mut updated = CatalogStore::new();
    let mut failures = Vec::new();
    for (id, result) in result {
        match result {
            Ok(Some(entry)) => {
                updated.insert(id, entry);
            }
            Ok(None) => {}
            Err(pi_rs_ai::Error::OAuth(error)) => {
                if emit_output {
                    eprintln!(
                        "Warning: skipped model catalog for {id}: {error}\nLog in again with `pi auth login {id}` to refresh it."
                    );
                }
            }
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    persist_model_catalogs(&catalog_path, updated)?;
    if !failures.is_empty() {
        return Err(format!(
            "Could not refresh model catalogs: {}",
            failures.join("; ")
        ));
    }
    if emit_output {
        println!("Model catalogs refreshed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_target_parser_preserves_upstream_conflict_precedence() -> Result<(), String> {
        let parse = |args: &[&str]| {
            let args = args.iter().copied().map(str::to_owned).collect::<Vec<_>>();
            let options = parse_options(Action::Update, &args)?;
            Ok::<_, String>((format!("{:?}", options.target), options.implicit_self))
        };
        assert_eq!(parse(&[])?, ("SelfOnly".to_owned(), true));
        assert_eq!(parse(&["--self"])?, ("SelfOnly".to_owned(), false));
        assert_eq!(parse(&["self", "--extensions"])?, ("All".to_owned(), false));
        assert_eq!(
            parse(&["--self", "--extensions"])?,
            ("All".to_owned(), false)
        );
        assert_eq!(parse(&["--models"])?, ("Models".to_owned(), false));
        assert_eq!(
            parse(&["--extension", "npm:one"])?,
            ("Extensions(Some(\"npm:one\"))".to_owned(), false)
        );
        assert!(
            parse(&["--all", "--models", "one"])
                .is_err_and(|error| error.starts_with("--all cannot be combined with --self"))
        );
        assert!(
            parse(&["--all", "--models", "--extension", "--unknown"])
                .is_err_and(|error| error.starts_with("Unknown option --unknown"))
        );
        assert!(
            parse(&["--extension", "--all"])
                .is_err_and(|error| error.starts_with("Missing value for --extension."))
        );
        Ok(())
    }

    #[test]
    fn self_update_plan_compares_versions_and_package_renames() {
        let mut release = LatestPiRelease {
            version: VERSION.to_owned(),
            package_name: None,
            note: None,
        };
        assert!(!release_is_update(&release, false));
        assert!(release_is_update(&release, true));
        release.package_name = Some("@example/pi".to_owned());
        assert!(release_is_update(&release, false));
        release.package_name = None;
        release.version = "0.9.0".to_owned();
        assert!(!release_is_update(&release, false));
        release.version = "999.0.0".to_owned();
        assert!(release_is_update(&release, false));
    }

    #[test]
    fn managed_marker_and_lock_protect_release_activation() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("managed");
        let package = root.join("releases/1.6.0/node_modules/pi");
        let outside = dir.path().join("checkout");
        fs::create_dir_all(&package)?;
        fs::create_dir_all(&outside)?;
        assert_eq!(managed_root_at(&root, &outside)?, None);
        assert!(managed_root_at(&root, &package).is_err());
        fs::write(
            root.join("managed-install.json"),
            r#"{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1"}"#,
        )?;
        assert_eq!(managed_root_at(&root, &package)?, Some(root.clone()));

        #[cfg(not(windows))]
        {
            fs::write(
                root.join("managed-install.json"),
                r#"{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1","entrypoint":{"type":"script","path":"C:\\Users\\user\\.pi\\agent\\bin\\pi.cmd"}}"#,
            )?;
            let err = managed_root_at(&root, &package).unwrap_err();
            assert!(
                err.contains("Running the Windows installation from WSL is unsupported"),
                "{err}"
            );
            fs::write(
                root.join("managed-install.json"),
                r#"{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1"}"#,
            )?;
        }
        let stashed = root.join("staging/update-old");
        fs::create_dir_all(&stashed)?;
        let guard = managed_lock(&root)?;
        assert!(managed_lock(&root).is_err_and(|error| error.contains("already running")));
        drop(guard);
        let _guard = managed_lock(&root)?;
        cleanup_staging(&root);
        assert!(!stashed.exists());
        assert!(package.exists());
        assert!(valid_managed_version("2.0.0-rc.1+local"));
        assert!(!valid_managed_version("../../2.0.0"));
        assert!(!valid_managed_version("2.0.0+"));
        activate_managed_release(&root, VERSION)?;
        activate_managed_release(&root, "2.0.0")?;
        assert_eq!(fs::read_to_string(root.join("current-version"))?, "2.0.0\n");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_update_smoke_tests_before_switching_and_prunes_old_releases()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir()?;
        let releases = root.path().join("releases");
        let active = "2.0.0";
        let shim = releases.join(active).join("node_modules/.bin");
        fs::create_dir_all(&shim)?;
        fs::create_dir_all(releases.join(VERSION))?;
        fs::create_dir_all(releases.join("0.0.1"))?;
        fs::create_dir_all(releases.join("not-a-release"))?;
        activate_managed_release(root.path(), VERSION)?;
        let script = shim.join("pi");
        fs::write(&script, "#!/bin/sh\nprintf '0.0.1\\n'\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        assert!(
            managed_update(root.path(), active)
                .await
                .is_err_and(|error| error.contains("expected 2.0.0"))
        );
        assert_eq!(
            fs::read_to_string(root.path().join("current-version"))?,
            format!("{VERSION}\n")
        );
        fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = --version ] && printf '2.0.0\\n'\n",
        )?;
        managed_update(root.path(), active).await?;
        assert_eq!(
            fs::read_to_string(root.path().join("current-version"))?,
            "2.0.0\n"
        );
        assert!(releases.join(VERSION).is_dir());
        assert!(!releases.join("0.0.1").exists());
        assert!(releases.join("not-a-release").is_dir());
        Ok(())
    }

    #[test]
    fn remote_catalog_is_scoped_to_provider_and_restored_from_disk()
    -> Result<(), Box<dyn std::error::Error>> {
        use pi_rs_ai::{AnyModel, Models, ModelsOptions, Provider, models_store::ModelsStoreEntry};
        let root = tempfile::tempdir()?;
        let models = Models::new(ModelsOptions::default());
        let builtin = pi_rs_ai::Models::builtin(ModelsOptions::default())?;
        let builtin = builtin
            .get_models(Some("anthropic"))
            .into_iter()
            .next()
            .ok_or("missing embedded fixture")?;
        let mut remote = builtin.clone();
        remote.base.id = "remote-test".into();
        remote.base.provider = "not-anthropic".into();
        let remote = decode_catalog(
            "anthropic",
            serde_json::json!({
                "models": [remote, {"type":"unknown","id":"ignore"}]
            }),
        )?;
        assert_eq!(remote.len(), 1);
        assert_eq!(remote[0].provider(), "anthropic");
        models.set_provider(
            Provider::new("anthropic", "Fixture")
                .with_models(vec![AnyModel::Chat(builtin.clone())]),
        );
        persist_model_catalogs(
            &root.path().join("models-store.json"),
            CatalogStore::from([(
                "anthropic".to_owned(),
                ModelsStoreEntry {
                    models: remote,
                    last_modified: Some(1),
                    ..ModelsStoreEntry::default()
                },
            )]),
        )?;
        restore_model_catalogs(&models, root.path())?;
        assert!(models.get_model("anthropic", builtin.id.as_str()).is_some());
        assert!(models.get_model("anthropic", "remote-test").is_some());
        Ok(())
    }

    #[test]
    fn pi_rs_managed_root_detects_marker_and_cleans_only_own_staging()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let rs_root = root.path().join("install-rs");
        let upstream_root = root.path().join("install");
        let bin_path = rs_root
            .join("releases")
            .join("v1.6.0")
            .join("bin")
            .join("pi");
        fs::create_dir_all(bin_path.parent().ok_or("missing parent")?)?;
        fs::write(&bin_path, b"")?;
        fs::write(
            rs_root.join(PI_RS_MANAGED_MARKER),
            r#"{"kind":"pi-rs-managed-install","schemaVersion":1,"layout":"releases-v1","installMode":"side-by-side"}"#,
        )?;
        fs::create_dir_all(rs_root.join("staging").join("install-999"))?;
        fs::create_dir_all(upstream_root.join("staging").join("update-123"))?;
        fs::write(
            upstream_root.join("managed-install.json"),
            r#"{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1"}"#,
        )?;

        let detected = pi_rs_managed_root_for_exe(&bin_path).ok_or("expected pi-rs root")?;
        assert_eq!(detected, fs::canonicalize(&rs_root)?);

        // Upstream managed-install.json alone must not be mistaken for a pi-rs install.
        let upstream_bin = upstream_root
            .join("releases")
            .join("1.6.0")
            .join("bin")
            .join("pi");
        fs::create_dir_all(upstream_bin.parent().ok_or("missing parent")?)?;
        fs::write(&upstream_bin, b"")?;
        assert!(pi_rs_managed_root_for_exe(&upstream_bin).is_none());

        cleanup_pi_rs_staging(&detected);
        assert!(!rs_root.join("staging").join("install-999").exists());
        assert!(upstream_root.join("staging").join("update-123").exists());

        let marker = read_json(&rs_root.join(PI_RS_MANAGED_MARKER));
        assert!(
            pi_rs_self_update_command_for_marker(marker.as_ref(), false).contains("--as-pi-rs")
        );
        assert!(
            pi_rs_self_update_command_for_marker(marker.as_ref(), true).contains("PI_AS_PI_RS")
        );
        assert!(
            !pi_rs_self_update_command_for_marker(
                Some(&serde_json::json!({"installMode": "drop-in"})),
                false,
            )
            .contains("--as-pi-rs")
        );
        Ok(())
    }
}
