//! Port of `packages/coding-agent/src/migrations.ts`: one-time startup migrations.
//!
//! Every migration swallows its own I/O and JSON errors; callers only receive
//! migrated provider names and deprecation warnings, matching upstream.

use std::path::Path;

use serde_json::{Map, Value};

/// Outcome of `runMigrations`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationResult {
    /// Provider names moved from `oauth.json` / `settings.json` into `auth.json`.
    pub migrated_auth_providers: Vec<String>,
    /// Human-readable warnings about deprecated `hooks/` and `tools/` directories.
    pub deprecation_warnings: Vec<String>,
}

/// Full legacy-to-namespaced keybinding table from
/// `packages/coding-agent/src/core/keybindings.ts` (`KEYBINDING_NAME_MIGRATIONS`).
const KEYBINDING_MIGRATIONS: &[(&str, &str)] = &[
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

/// Remove a leading UTF-8 byte order mark from decoded text.
fn strip_bom(content: &str) -> &str {
    match content.strip_prefix('\u{feff}') {
        Some(rest) => rest,
        None => content,
    }
}

/// Read a JSON object, stripping a leading BOM like upstream's `stripBom`.
fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let value: Value = serde_json::from_str(strip_bom(&text)).map_err(|error| error.to_string())?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("JSON value is not an object".to_string()),
    }
}

/// `JSON.stringify(value, null, 2)` with an optional trailing newline.
fn write_json(path: &Path, value: &Value, trailing_newline: bool) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    if trailing_newline {
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn set_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) {}

/// Migrate legacy `oauth.json` and `settings.json` `apiKeys` into `auth.json`.
/// Returns the names of migrated providers.
fn migrate_auth_to_auth_json(agent_dir: &Path) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    if auth_path.exists() {
        return Vec::new();
    }

    let mut migrated: Map<String, Value> = Map::new();
    let mut providers: Vec<String> = Vec::new();

    let oauth_path = agent_dir.join("oauth.json");
    if oauth_path.exists()
        && let Ok(oauth) = read_json_object(&oauth_path)
    {
        for (provider, credential) in oauth {
            let mut entry = Map::new();
            entry.insert("type".to_string(), Value::String("oauth".to_string()));
            if let Value::Object(fields) = credential {
                for (field, value) in fields {
                    entry.insert(field, value);
                }
            }
            migrated.insert(provider.clone(), Value::Object(entry));
            providers.push(provider);
        }
        let renamed = format!("{}.migrated", oauth_path.display());
        let _ = std::fs::rename(&oauth_path, renamed);
    }

    let settings_path = agent_dir.join("settings.json");
    if settings_path.exists()
        && let Ok(mut settings) = read_json_object(&settings_path)
        && let Some(Value::Object(api_keys)) = settings.get("apiKeys").cloned()
    {
        for (provider, key) in api_keys {
            if let Some(key) = key.as_str()
                && !migrated.contains_key(&provider)
            {
                let mut entry = Map::new();
                entry.insert("type".to_string(), Value::String("api_key".to_string()));
                entry.insert("key".to_string(), Value::String(key.to_string()));
                migrated.insert(provider.clone(), Value::Object(entry));
                providers.push(provider);
            }
        }
        settings.remove("apiKeys");
        let _ = write_json(&settings_path, &Value::Object(settings), false);
    }

    if !migrated.is_empty() {
        let _ = std::fs::create_dir_all(agent_dir);
        if write_json(&auth_path, &Value::Object(migrated), false).is_ok() {
            set_private_mode(&auth_path);
        }
    }

    providers
}

/// Move v0.30.0-era `.jsonl` files from the agent root into the session
/// directory encoded in their header `cwd`.
fn migrate_sessions_from_agent_root(agent_dir: &Path) {
    let entries = match std::fs::read_dir(agent_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let first_line = content.lines().next().unwrap_or("");
        if first_line.trim().is_empty() {
            continue;
        }
        let Ok(header) = serde_json::from_str::<Value>(strip_bom(first_line)) else {
            continue;
        };
        if header.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let Some(cwd) = header.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if cwd.is_empty() {
            continue;
        }
        let without_root = cwd.trim_start_matches(['/', '\\']);
        let safe_path: String = without_root
            .chars()
            .map(|character| {
                if character == '/' || character == '\\' || character == ':' {
                    '-'
                } else {
                    character
                }
            })
            .collect();
        let correct_dir = agent_dir.join("sessions").join(format!("--{safe_path}--"));
        if std::fs::create_dir_all(&correct_dir).is_err() {
            continue;
        }
        let new_path = correct_dir.join(name);
        if new_path.exists() {
            continue;
        }
        let _ = std::fs::rename(&path, &new_path);
    }
}

/// Move `fd`/`rg` binaries from `tools/` to `bin/`.
fn migrate_tools_to_bin(agent_dir: &Path) {
    let tools_dir = agent_dir.join("tools");
    if !tools_dir.exists() {
        return;
    }
    let bin_dir = agent_dir.join("bin");
    for binary in ["fd", "rg", "fd.exe", "rg.exe"] {
        let old_path = tools_dir.join(binary);
        if !old_path.exists() {
            continue;
        }
        if !bin_dir.exists() {
            let _ = std::fs::create_dir_all(&bin_dir);
        }
        let new_path = bin_dir.join(binary);
        if new_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        } else {
            let _ = std::fs::rename(&old_path, &new_path);
        }
    }
}

/// Core of `migrateKeybindingsConfig`: rename legacy keys, drop legacy keys
/// shadowed by their namespaced form, order known keys, then sort extras.
fn migrate_keybindings_config(raw: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut config = Map::new();
    let mut migrated = false;

    for (key, value) in raw {
        let mapped = KEYBINDING_MIGRATIONS
            .iter()
            .find(|(legacy, _)| *legacy == key.as_str())
            .map(|(_, namespaced)| *namespaced)
            .unwrap_or(key.as_str());
        if mapped != key.as_str() {
            migrated = true;
        }
        if mapped != key.as_str() && raw.contains_key(mapped) {
            migrated = true;
            continue;
        }
        config.insert(mapped.to_string(), value.clone());
    }

    let mut ordered = Map::new();
    for (_, namespaced) in KEYBINDING_MIGRATIONS {
        if let Some(value) = config.remove(*namespaced) {
            ordered.insert(namespaced.to_string(), value);
        }
    }
    let mut extras: Vec<(String, Value)> = config.into_iter().collect();
    extras.sort_by(|left, right| left.0.cmp(&right.0));
    for (key, value) in extras {
        ordered.insert(key, value);
    }

    (ordered, migrated)
}

fn migrate_keybindings_config_file(agent_dir: &Path) {
    let config_path = agent_dir.join("keybindings.json");
    if !config_path.exists() {
        return;
    }
    if let Ok(raw) = read_json_object(&config_path) {
        let (config, migrated) = migrate_keybindings_config(&raw);
        if migrated {
            let _ = write_json(&config_path, &Value::Object(config), true);
        }
    }
}

fn migrate_commands_to_prompts(base_dir: &Path) {
    let commands_dir = base_dir.join("commands");
    let prompts_dir = base_dir.join("prompts");
    if commands_dir.exists() && !prompts_dir.exists() {
        let _ = std::fs::rename(&commands_dir, &prompts_dir);
    }
}

fn check_deprecated_extension_dirs(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();

    if base_dir.join("hooks").exists() {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions."
        ));
    }

    let tools_dir = base_dir.join("tools");
    if tools_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&tools_dir)
    {
        let has_custom_tools = entries.flatten().any(|entry| {
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => return true,
            };
            let lower = name.to_lowercase();
            lower != "fd"
                && lower != "rg"
                && lower != "fd.exe"
                && lower != "rg.exe"
                && !name.starts_with('.')
        });
        if has_custom_tools {
            warnings.push(format!(
                "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
            ));
        }
    }

    warnings
}

/// The migration sequence, parameterized by the agent directory so it can be
/// exercised without mutating process-global environment variables.
fn run_migrations_in(cwd: &Path, agent_dir: &Path) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json(agent_dir);
    migrate_sessions_from_agent_root(agent_dir);
    migrate_tools_to_bin(agent_dir);
    migrate_keybindings_config_file(agent_dir);

    let project_dir = cwd.join(crate::config::CONFIG_DIR_NAME);
    migrate_commands_to_prompts(agent_dir);
    migrate_commands_to_prompts(&project_dir);

    let mut deprecation_warnings = check_deprecated_extension_dirs(agent_dir, "Global");
    deprecation_warnings.extend(check_deprecated_extension_dirs(&project_dir, "Project"));

    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

/// Run all migrations once on startup.
pub fn run_migrations(cwd: &Path) -> MigrationResult {
    match crate::config::get_agent_dir() {
        Ok(agent_dir) => run_migrations_in(cwd, &agent_dir),
        Err(_) => MigrationResult::default(),
    }
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

    fn write(path: &Path, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| e.to_string())
    }

    #[test]
    fn migrates_legacy_oauth_and_settings_api_keys() -> Result<(), String> {
        let root = temp_dir("pi-migrate-auth-")?;
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        let oauth = format!(
            "\u{feff}{}",
            r#"{"anthropic":{"access":"token-a","refresh":"token-r"}}"#
        );
        write(&agent_dir.join("oauth.json"), &oauth)?;
        write(
            &agent_dir.join("settings.json"),
            r#"{"apiKeys":{"anthropic":"ignored","openai":"sk-openai","bad":42},"theme":"dark"}"#,
        )?;

        let result = run_migrations_in(root.path(), &agent_dir);

        assert_eq!(result.migrated_auth_providers, vec!["anthropic", "openai"]);
        let auth = read_json_object(&agent_dir.join("auth.json"))?;
        let anthropic = auth
            .get("anthropic")
            .and_then(Value::as_object)
            .ok_or("missing anthropic")?;
        assert_eq!(anthropic.get("type").and_then(Value::as_str), Some("oauth"));
        assert_eq!(
            anthropic.get("access").and_then(Value::as_str),
            Some("token-a")
        );
        let openai = auth
            .get("openai")
            .and_then(Value::as_object)
            .ok_or("missing openai")?;
        assert_eq!(openai.get("type").and_then(Value::as_str), Some("api_key"));
        assert_eq!(openai.get("key").and_then(Value::as_str), Some("sk-openai"));
        assert!(!auth.contains_key("bad"));
        assert!(agent_dir.join("oauth.json.migrated").exists());
        let settings = read_json_object(&agent_dir.join("settings.json"))?;
        assert!(!settings.contains_key("apiKeys"));
        assert_eq!(settings.get("theme").and_then(Value::as_str), Some("dark"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(agent_dir.join("auth.json"))
                .map_err(|e| e.to_string())?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        Ok(())
    }

    #[test]
    fn existing_auth_json_blocks_auth_migration() -> Result<(), String> {
        let root = temp_dir("pi-migrate-auth-exists-")?;
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        write(&agent_dir.join("auth.json"), r#"{"existing":true}"#)?;
        write(
            &agent_dir.join("oauth.json"),
            r#"{"github":{"access":"t"}}"#,
        )?;

        let result = run_migrations_in(root.path(), &agent_dir);

        assert!(result.migrated_auth_providers.is_empty());
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("auth.json")).map_err(|e| e.to_string())?,
            r#"{"existing":true}"#
        );
        assert!(agent_dir.join("oauth.json").exists());
        Ok(())
    }

    #[test]
    fn migrates_sessions_from_agent_root() -> Result<(), String> {
        let root = temp_dir("pi-migrate-sessions-")?;
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        let content = "{\"type\":\"session\",\"cwd\":\"/work/project\"}\n{\"type\":\"message\"}\n";
        write(&agent_dir.join("s.jsonl"), content)?;

        run_migrations_in(root.path(), &agent_dir);

        let migrated = agent_dir.join("sessions/--work-project--/s.jsonl");
        assert!(migrated.exists());
        assert_eq!(
            std::fs::read_to_string(&migrated).map_err(|e| e.to_string())?,
            content
        );
        Ok(())
    }

    #[test]
    fn migrates_tools_to_bin_and_warns_about_custom_tools() -> Result<(), String> {
        let root = temp_dir("pi-migrate-tools-")?;
        let agent_dir = root.path().join("agent");
        let tools_dir = agent_dir.join("tools");
        std::fs::create_dir_all(&tools_dir).map_err(|e| e.to_string())?;
        write(&tools_dir.join("fd"), "binary")?;
        write(&tools_dir.join("rg.exe"), "binary")?;
        write(&tools_dir.join("custom.js"), "tool")?;

        let result = run_migrations_in(root.path(), &agent_dir);

        assert!(agent_dir.join("bin/fd").exists());
        assert!(agent_dir.join("bin/rg.exe").exists());
        assert!(!tools_dir.join("fd").exists());
        assert!(tools_dir.join("custom.js").exists());
        assert!(
            result
                .deprecation_warnings
                .iter()
                .any(|warning| warning.contains("custom tools"))
        );
        Ok(())
    }

    #[test]
    fn migrates_keybindings_and_drops_shadowed_legacy_names() -> Result<(), String> {
        let root = temp_dir("pi-migrate-keys-")?;
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        write(
            &agent_dir.join("keybindings.json"),
            r#"{"$schema":"x","cursorUp":"ctrl+up","undo":"ctrl+z","submit":"enter","tui.input.copy":"ctrl+c"}"#,
        )?;

        run_migrations_in(root.path(), &agent_dir);

        let text = std::fs::read_to_string(agent_dir.join("keybindings.json"))
            .map_err(|e| e.to_string())?;
        assert!(text.ends_with('\n'));
        let migrated = read_json_object(&agent_dir.join("keybindings.json"))?;
        assert_eq!(
            migrated.get("tui.editor.cursorUp").and_then(Value::as_str),
            Some("ctrl+up")
        );
        assert_eq!(
            migrated.get("tui.editor.undo").and_then(Value::as_str),
            Some("ctrl+z")
        );
        assert_eq!(
            migrated.get("tui.input.submit").and_then(Value::as_str),
            Some("enter")
        );
        assert_eq!(
            migrated.get("tui.input.copy").and_then(Value::as_str),
            Some("ctrl+c")
        );
        assert!(!migrated.contains_key("cursorUp"));
        assert!(!migrated.contains_key("undo"));
        assert_eq!(migrated.get("$schema").and_then(Value::as_str), Some("x"));
        Ok(())
    }

    #[test]
    fn migrates_commands_and_reports_hooks() -> Result<(), String> {
        let root = temp_dir("pi-migrate-extensions-")?;
        let agent_dir = root.path().join("agent");
        let project_dir = root.path().join(crate::config::CONFIG_DIR_NAME);
        std::fs::create_dir_all(agent_dir.join("commands")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(agent_dir.join("hooks")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(project_dir.join("commands")).map_err(|e| e.to_string())?;

        let result = run_migrations_in(root.path(), &agent_dir);

        assert!(agent_dir.join("prompts").is_dir());
        assert!(project_dir.join("prompts").is_dir());
        assert!(!agent_dir.join("commands").exists());
        assert!(result.deprecation_warnings.len() == 1);
        assert!(result.deprecation_warnings[0].starts_with("Global hooks/"));
        Ok(())
    }

    #[test]
    fn malformed_json_is_swallowed() -> Result<(), String> {
        let root = temp_dir("pi-migrate-malformed-")?;
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        write(&agent_dir.join("oauth.json"), "{\"providers\":{\n")?;
        write(&agent_dir.join("settings.json"), "")?;
        write(&agent_dir.join("keybindings.json"), "\u{feff}{broken")?;

        let result = run_migrations_in(root.path(), &agent_dir);

        assert!(result.migrated_auth_providers.is_empty());
        assert!(!agent_dir.join("auth.json").exists());
        Ok(())
    }
}
