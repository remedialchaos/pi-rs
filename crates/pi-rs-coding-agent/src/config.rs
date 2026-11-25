//! Port of `packages/coding-agent/src/config.ts` (package detection, install-method
//! detection, and the `~/.pi/agent/*` path helpers).
//!
//! Path getters return `Result<PathBuf, String>` because the agent directory and the
//! package directory are derived from the environment (`PI_CODING_AGENT_DIR`,
//! `PI_PACKAGE_DIR`, `HOME`) and from the running executable. Errors carry a String
//! so callers can display them without an error-type dependency at this foundation
//! layer. The pure helpers underneath (`agent_dir_from`, `normalize_path_with_home`,
//! `package_dir_from`, `detect_install_change_at`) take their inputs explicitly so
//! they can be tested without mutating process-global environment variables.

use std::path::{Path, PathBuf};

/// Upstream `package.json` `name`; used for self-update commands and telemetry.
pub const PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
/// Upstream `piConfig.name` fallback (`piConfig` only sets `configDir`).
pub const APP_NAME: &str = "pi";
/// Upstream `APP_TITLE` (`piConfig.name ? APP_NAME : "π"`).
pub const APP_TITLE: &str = "π";
/// Upstream `piConfig.configDir`.
pub const CONFIG_DIR_NAME: &str = ".pi";
/// Upstream `package.json` `version` for this fork.
pub const VERSION: &str = "1.6.0";
/// `PI_CODING_AGENT_DIR`: overrides the user config directory when non-empty.
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
/// `PI_CODING_AGENT_SESSION_DIR`: session directory override used by downstream units.
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

const DEFAULT_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";

/// Result of `detectInstallChange`: the package this process loaded from changed on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallChange {
    /// The installed `package.json` now reports a different version.
    Updated { version: String },
    /// The installed `package.json` no longer exists.
    Removed,
}

/// Remove a leading UTF-8 byte order mark from decoded text.
fn strip_bom(content: &str) -> &str {
    match content.strip_prefix('\u{feff}') {
        Some(rest) => rest,
        None => content,
    }
}

/// `dirs::home_dir` equivalent without the `dirs` dependency: `HOME`, then
/// `USERPROFILE` on Windows hosts, then an error.
fn home_dir() -> Result<PathBuf, String> {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            return Ok(PathBuf::from(value));
        }
    }
    Err("HOME environment variable is not set".to_string())
}

/// Core of `normalizePath`: trim, `file://` conversion, and `~` expansion against
/// an explicitly supplied home directory.
fn normalize_path_with_home(input: &str, trim: bool, home: Option<&Path>) -> String {
    let normalized = if trim { input.trim() } else { input };

    if normalized.starts_with("file://")
        && let Ok(url) = url::Url::parse(normalized)
        && let Ok(path) = url.to_file_path()
    {
        return path.to_string_lossy().into_owned();
    }

    if normalized == "~" {
        return match home {
            Some(home) => home.to_string_lossy().into_owned(),
            None => normalized.to_string(),
        };
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        return match home {
            Some(home) => home.join(rest).to_string_lossy().into_owned(),
            None => normalized.to_string(),
        };
    }

    normalized.to_string()
}

/// Normalize a path string: optional whitespace trim, `file://` URL conversion,
/// and leading `~` expansion using `HOME` (no-op when `HOME` is unavailable).
pub fn normalize_path(input: &str, trim: bool) -> String {
    normalize_path_with_home(input, trim, home_dir().ok().as_deref())
}

/// Expand a leading `~` in `path`. Equivalent to upstream `expandTildePath`.
pub fn expand_tilde_path(path: &str) -> String {
    normalize_path(path, false)
}

/// Resolve `input` against `base` (defaulting to the current directory upstream),
/// expanding `~` first. Unlike `canonicalize`, this does not require the path to
/// exist, matching `node:path.resolve`.
pub fn resolve_path(input: &str, base: impl AsRef<Path>, trim: bool) -> Result<PathBuf, String> {
    let normalized = normalize_path(input, trim);
    let candidate = Path::new(&normalized);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.as_ref().join(candidate)
    };
    std::path::absolute(joined).map_err(|error| error.to_string())
}

/// Core of `getAgentDir`: an explicit environment value (tilde-expanded) wins
/// over `<home>/.pi/agent`, and an empty value counts as unset.
fn agent_dir_from(env_dir: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    match env_dir {
        Some(dir) if !dir.is_empty() => {
            Ok(PathBuf::from(normalize_path_with_home(dir, false, home)))
        }
        _ => {
            let home = home.ok_or_else(|| "HOME environment variable is not set".to_string())?;
            Ok(home.join(CONFIG_DIR_NAME).join("agent"))
        }
    }
}

/// Get the agent config directory: `PI_CODING_AGENT_DIR` if non-empty (tilde
/// expanded), otherwise `HOME/.pi/agent`.
pub fn get_agent_dir() -> Result<PathBuf, String> {
    let env_dir = std::env::var(ENV_AGENT_DIR).ok();
    let home = home_dir().ok();
    agent_dir_from(env_dir.as_deref(), home.as_deref())
}

fn agent_path(name: &str) -> Result<PathBuf, String> {
    Ok(get_agent_dir()?.join(name))
}

/// Path to the user's custom themes directory.
pub fn get_custom_themes_dir() -> Result<PathBuf, String> {
    agent_path("themes")
}

/// Path to `models.json`.
pub fn get_models_path() -> Result<PathBuf, String> {
    agent_path("models.json")
}

/// Path to `auth.json`.
pub fn get_auth_path() -> Result<PathBuf, String> {
    agent_path("auth.json")
}

/// Path to `settings.json`.
pub fn get_settings_path() -> Result<PathBuf, String> {
    agent_path("settings.json")
}

/// Path to the tools directory (legacy location of managed binaries).
pub fn get_tools_dir() -> Result<PathBuf, String> {
    agent_path("tools")
}

/// Path to the managed binaries directory (`fd`, `rg`).
pub fn get_bin_dir() -> Result<PathBuf, String> {
    agent_path("bin")
}

/// Path to the prompt templates directory.
pub fn get_prompts_dir() -> Result<PathBuf, String> {
    agent_path("prompts")
}

/// Path to the sessions directory.
pub fn get_sessions_dir() -> Result<PathBuf, String> {
    agent_path("sessions")
}

/// Path to the debug log file, e.g. `~/.pi/agent/pi-debug.log`.
pub fn get_debug_log_path() -> Result<PathBuf, String> {
    agent_path(&format!("{APP_NAME}-debug.log"))
}

/// Walk up from `start_dir` looking for a `package.json`. A `dist/` directory
/// that also has a `package.json` is skipped when its parent has one, because
/// build:binary copies Bun metadata into `dist/`.
pub fn find_node_package_dir(start_dir: impl AsRef<Path>) -> PathBuf {
    let start = start_dir.as_ref().to_path_buf();
    let mut dir = start.clone();
    loop {
        if dir.join("package.json").is_file() {
            if dir.file_name().and_then(|name| name.to_str()) == Some("dist")
                && let Some(parent) = dir.parent()
                && parent.join("package.json").is_file()
            {
                return parent.to_path_buf();
            }
            return dir;
        }
        let Some(parent) = dir.parent() else {
            return start;
        };
        if parent == dir {
            return start;
        }
        dir = parent.to_path_buf();
    }
}

/// Core of `getPackageDir`: `PI_PACKAGE_DIR` override or discovery from the
/// executable's directory.
fn package_dir_from(env_dir: Option<&str>) -> Result<PathBuf, String> {
    if let Some(dir) = env_dir
        && !dir.is_empty()
    {
        let normalized = PathBuf::from(normalize_path_with_home(
            dir,
            false,
            home_dir().ok().as_deref(),
        ));
        return std::path::absolute(normalized).map_err(|error| error.to_string());
    }
    let start = match std::env::current_exe() {
        Ok(exe) => match exe.parent() {
            Some(parent) => parent.to_path_buf(),
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        },
        Err(_) => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    Ok(find_node_package_dir(start))
}

/// Get the package root containing `package.json`. `PI_PACKAGE_DIR` overrides
/// discovery (useful for Nix/Guix store paths).
pub fn get_package_dir() -> Result<PathBuf, String> {
    package_dir_from(std::env::var("PI_PACKAGE_DIR").ok().as_deref())
}

/// Path to the shipped `package.json`.
pub fn get_package_json_path() -> Result<PathBuf, String> {
    Ok(get_package_dir()?.join("package.json"))
}

/// Path to the shipped `README.md`.
pub fn get_readme_path() -> Result<PathBuf, String> {
    resolve_path("README.md", get_package_dir()?, false)
}

/// Path to the shipped `docs/` directory.
pub fn get_docs_path() -> Result<PathBuf, String> {
    resolve_path("docs", get_package_dir()?, false)
}

/// Path to the shipped `examples/` directory.
pub fn get_examples_path() -> Result<PathBuf, String> {
    resolve_path("examples", get_package_dir()?, false)
}

/// Path to the shipped `CHANGELOG.md`.
pub fn get_changelog_path() -> Result<PathBuf, String> {
    resolve_path("CHANGELOG.md", get_package_dir()?, false)
}

/// Detect the package manager that installed this executable, using the same
/// path-shape rules as upstream's `detectInstallMethod`.
pub fn detect_install_method() -> Result<String, String> {
    let package_dir = get_package_dir()?;
    let executable = std::env::current_exe().unwrap_or_default();
    let combined = format!("{}\0{}", package_dir.display(), executable.display())
        .to_lowercase()
        .replace('\\', "/");

    if combined.contains("/pnpm/") || combined.contains("/.pnpm/") {
        return Ok("pnpm".to_string());
    }
    if combined.contains("/yarn/") || combined.contains("/.yarn/") {
        return Ok("yarn".to_string());
    }
    if combined.contains("/install/global/node_modules/") {
        return Ok("bun".to_string());
    }
    if combined.contains("/npm/") || combined.contains("/node_modules/") {
        return Ok("npm".to_string());
    }
    Ok("unknown".to_string())
}

/// Core of `detectInstallChange`, taking the `package.json` path and the
/// version baked into this binary explicitly so it is testable in isolation.
fn detect_install_change_at(
    package_json_path: &Path,
    version: &str,
) -> Result<Option<InstallChange>, String> {
    let bytes = match std::fs::read(package_json_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(InstallChange::Removed));
        }
        Err(_) => return Ok(None),
    };
    let text = String::from_utf8_lossy(&bytes);
    let value: serde_json::Value = match serde_json::from_str(strip_bom(&text)) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if let Some(installed) = value.get("version").and_then(serde_json::Value::as_str)
        && !installed.is_empty()
        && installed != version
    {
        return Ok(Some(InstallChange::Updated {
            version: installed.to_string(),
        }));
    }
    Ok(None)
}

/// Detect that the package this process runs from changed on disk (for example
/// after `pi update` in another terminal), by re-reading its `package.json`.
pub fn detect_install_change() -> Result<Option<InstallChange>, String> {
    detect_install_change_at(&get_package_json_path()?, VERSION)
}

fn share_viewer_url_with(gist_id: &str, configured: Option<String>) -> String {
    let base = configured
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_VIEWER_URL.to_string());
    format!("{base}#{gist_id}")
}

/// Get the share viewer URL for a gist ID; `PI_SHARE_VIEWER_URL` overrides the
/// default base URL.
pub fn get_share_viewer_url(gist_id: &str) -> String {
    share_viewer_url_with(gist_id, std::env::var("PI_SHARE_VIEWER_URL").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> Result<tempfile::TempDir, String> {
        tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .map_err(|e| e.to_string())
    }

    #[test]
    fn agent_dir_uses_env_override_with_tilde() -> Result<(), String> {
        let home = Path::new("/home/test-user");
        let dir = agent_dir_from(Some("~/custom/agent"), Some(home))?;
        assert_eq!(dir, home.join("custom/agent"));
        Ok(())
    }

    #[test]
    fn agent_dir_defaults_to_home_pi_agent() -> Result<(), String> {
        let home = Path::new("/home/test-user");
        assert_eq!(agent_dir_from(None, Some(home))?, home.join(".pi/agent"));
        assert_eq!(
            agent_dir_from(Some(""), Some(home))?,
            home.join(".pi/agent")
        );
        Ok(())
    }

    #[test]
    fn agent_dir_requires_home_when_no_override() {
        let result = agent_dir_from(None, None);
        assert!(result.is_err());
    }

    #[test]
    fn normalize_path_expands_tilde_and_trims() {
        let home = Path::new("/home/test-user");
        assert_eq!(
            normalize_path_with_home("  ~/x  ", true, Some(home)),
            "/home/test-user/x"
        );
        assert_eq!(
            normalize_path_with_home("~", false, Some(home)),
            "/home/test-user"
        );
        assert_eq!(normalize_path_with_home("~/a", false, None), "~/a");
        assert_eq!(
            normalize_path_with_home(" plain ", false, Some(home)),
            " plain "
        );
    }

    #[test]
    fn resolve_path_handles_relative_and_absolute_inputs() -> Result<(), String> {
        let base = Path::new("/work/project");
        assert_eq!(
            resolve_path("a/b", base, false)?,
            PathBuf::from("/work/project/a/b")
        );
        assert_eq!(
            resolve_path("/abs/b", base, false)?,
            PathBuf::from("/abs/b")
        );
        Ok(())
    }

    #[test]
    fn find_node_package_dir_skips_dist_metadata() -> Result<(), String> {
        let root = temp_dir("pi-config-package-")?;
        let dist = root.path().join("dist");
        let bundle = dist.join("bundle");
        std::fs::create_dir_all(&bundle).map_err(|e| e.to_string())?;
        std::fs::write(root.path().join("package.json"), "{}").map_err(|e| e.to_string())?;
        std::fs::write(dist.join("package.json"), "{}").map_err(|e| e.to_string())?;
        assert_eq!(find_node_package_dir(&bundle), root.path());
        Ok(())
    }

    #[test]
    fn package_dir_from_explicit_env_value() -> Result<(), String> {
        let root = temp_dir("pi-config-pkgdir-")?;
        let dir = package_dir_from(Some(root.path().to_str().ok_or("non-UTF-8 temp path")?))?;
        assert_eq!(dir, root.path());
        Ok(())
    }

    #[test]
    fn share_viewer_url_uses_default_and_override() {
        assert_eq!(
            share_viewer_url_with("abc", None),
            "https://pi.dev/session/#abc"
        );
        assert_eq!(
            share_viewer_url_with("abc", Some("https://example.test/view/".to_string())),
            "https://example.test/view/#abc"
        );
        assert_eq!(
            share_viewer_url_with("abc", Some(String::new())),
            "https://pi.dev/session/#abc"
        );
    }

    #[test]
    fn detect_install_change_handles_updated_removed_and_bom() -> Result<(), String> {
        let root = temp_dir("pi-config-change-")?;
        let path = root.path().join("package.json");

        std::fs::write(&path, "\u{feff}{\"version\":\"9.9.9\"}").map_err(|e| e.to_string())?;
        assert_eq!(
            detect_install_change_at(&path, VERSION)?,
            Some(InstallChange::Updated {
                version: "9.9.9".to_string()
            })
        );

        std::fs::write(&path, format!("{{\"version\":\"{VERSION}\"}}"))
            .map_err(|e| e.to_string())?;
        assert_eq!(detect_install_change_at(&path, VERSION)?, None);

        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        assert_eq!(
            detect_install_change_at(&path, VERSION)?,
            Some(InstallChange::Removed)
        );
        Ok(())
    }
}
