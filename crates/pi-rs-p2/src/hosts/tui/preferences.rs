use pi_rs_p2_tui::{
    KeybindingsConfig, MAX_CUSTOM_THEMES, MAX_PREFERENCE_FILE, ResolveThemeOptions, TerminalState,
    Theme, TuiSettings, ValidatedThemeJson, detect_terminal_theme, is_privileged, is_theme_setting,
    resolve_theme, resolve_theme_setting, sanitize_keybindings, sanitize_settings,
    validate_theme_json,
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub(super) struct Preferences {
    pub(super) mouse: bool,
    pub(super) theme_path: Option<PathBuf>,
    pub(super) keys: KeybindingsConfig,
    pub(super) settings: TuiSettings,
    pub(super) raw_settings: Value,
    pub(super) theme: Theme,
    pub(super) editor_cmd: Option<String>,
    home_dir: PathBuf,
    config_dir: PathBuf,
    agent_dir: PathBuf,
    theme_arg: Option<String>,
    no_themes: bool,
    current_theme: String,
    custom_themes: HashMap<String, ValidatedThemeJson>,
    custom_theme_paths: HashMap<String, PathBuf>,
    color_mode: pi_rs_tui::TerminalColorMode,
    terminal: TerminalState,
}

fn read(path: &Path) -> Option<(Value, usize)> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_PREFERENCE_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_PREFERENCE_FILE {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Some((serde_json::from_str(trimmed).ok()?, trimmed.len()))
}

fn load_custom_themes(
    agent: &Path,
    no_themes: bool,
) -> (
    HashMap<String, PathBuf>,
    HashMap<String, ValidatedThemeJson>,
) {
    let mut paths = HashMap::new();
    let mut themes = HashMap::new();
    if !no_themes && let Ok(entries) = fs::read_dir(agent.join("themes")) {
        let mut files: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        let mut total = 0;
        for p in files {
            if themes.len() >= MAX_CUSTOM_THEMES {
                break;
            }
            if let Some((v, n)) = read(&p)
                && total + n <= 1024 * 1024
                && is_theme_setting(&v["name"])
                && let Ok(theme) = validate_theme_json("local theme", &v)
                && !matches!(theme.name.as_str(), "system" | "dark" | "light")
            {
                total += n;
                paths.insert(theme.name.clone(), p);
                themes.insert(theme.name.clone(), theme);
            }
        }
    }
    (paths, themes)
}

impl Preferences {
    pub(super) fn load(
        home: &Path,
        config: &Path,
        setting: Option<String>,
        no_themes: bool,
    ) -> Self {
        Self::load_with_terminal(home, config, setting, no_themes, TerminalState::default())
    }

    pub(super) fn load_with_terminal(
        home: &Path,
        config: &Path,
        setting: Option<String>,
        no_themes: bool,
        terminal: TerminalState,
    ) -> Self {
        let agent = match env::var("PI_CODING_AGENT_DIR")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(s) if s == "~" => home.to_owned(),
            Some(s) if s.starts_with("~/") => home.join(&s[2..]),
            Some(s) => PathBuf::from(s),
            None => home.join(".pi/agent"),
        };
        let prefs = read(&config.join("prefs.json"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let settings_doc = read(&agent.join("settings.json"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let raw_keys = read(&agent.join("keybindings.json"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let mut keys = sanitize_keybindings(&raw_keys).config;
        keys.extend(sanitize_keybindings(&prefs["keybindings"]).config);

        let agent_settings = sanitize_settings(&settings_doc).settings;
        let prefs_settings = sanitize_settings(&prefs["settings"]).settings;
        let settings = pi_rs_p2_tui::merge_settings(vec![agent_settings, prefs_settings]);
        let raw_settings =
            serde_json::to_value(&settings).unwrap_or_else(|_| Value::Object(Map::new()));

        let (paths, themes) = load_custom_themes(&agent, no_themes);
        let resolved_setting = [
            setting.as_deref(),
            prefs["theme"].as_str(),
            settings_doc["theme"].as_str(),
        ]
        .into_iter()
        .flatten()
        .find(|s| is_theme_setting(&Value::String((*s).into())))
        .map(str::to_owned);
        let color_mode = pi_rs_tui::get_terminal_color_mode(pi_rs_tui::get_capabilities());
        let selected = resolve_theme_setting(
            resolved_setting.as_deref(),
            detect_terminal_theme(&terminal),
        );
        let theme_path = selected.and_then(|name| paths.get(&name).cloned());
        let current_theme = resolved_setting
            .clone()
            .unwrap_or_else(|| pi_rs_p2_tui::theme::SYSTEM_THEME_NAME.to_owned());
        let theme = resolve_theme(ResolveThemeOptions {
            setting: resolved_setting,
            terminal: terminal.clone(),
            custom_themes: themes.clone(),
            mode: color_mode,
        })
        .theme;
        Self {
            keys,
            settings,
            raw_settings,
            theme,
            theme_path,
            mouse: prefs["mouse"].as_str() != Some("off"),
            editor_cmd: prefs["editor"]
                .as_str()
                .or_else(|| settings_doc["externalEditor"].as_str())
                .map(str::to_owned),
            home_dir: home.to_owned(),
            config_dir: config.to_owned(),
            agent_dir: agent,
            theme_arg: setting,
            no_themes,
            current_theme,
            custom_themes: themes,
            custom_theme_paths: paths,
            color_mode,
            terminal,
        }
    }

    pub(super) fn reload(&self) -> Self {
        Self::load_with_terminal(
            &self.home_dir,
            &self.config_dir,
            self.theme_arg.clone(),
            self.no_themes,
            self.terminal.clone(),
        )
    }

    pub(super) fn update_terminal(
        &mut self,
        terminal: TerminalState,
        theme_ref: &pi_rs_p2_tui::ThemeRef,
        client: &mut pi_rs_p2_tui::host::TuiClient<pi_rs_tui::TuiAltScreen>,
    ) {
        self.terminal = terminal;
        let setting = if self.current_theme == pi_rs_p2_tui::theme::SYSTEM_THEME_NAME {
            self.theme_arg.clone()
        } else {
            Some(self.current_theme.clone())
        };
        let resolved = resolve_theme(ResolveThemeOptions {
            setting,
            terminal: self.terminal.clone(),
            custom_themes: self.custom_themes.clone(),
            mode: self.color_mode,
        });
        client.set_theme(theme_ref, resolved.theme.clone());
        self.theme = resolved.theme;
    }

    pub(super) fn current_theme_name(&self) -> &str {
        &self.current_theme
    }

    pub(super) fn available_themes(&self) -> Vec<String> {
        let mut names = vec!["dark".to_owned(), "light".to_owned(), "system".to_owned()];
        let mut custom: Vec<String> = self.custom_themes.keys().cloned().collect();
        custom.sort();
        names.extend(custom);
        names
    }

    pub(super) fn save_setting_path(
        &mut self,
        path: &[String],
        value: &Value,
    ) -> Result<Vec<String>, String> {
        let mut patch = Value::Object(Map::new());
        let mut cursor = &mut patch;
        for segment in path.iter().take(path.len().saturating_sub(1)) {
            cursor[segment] = Value::Object(Map::new());
            cursor = &mut cursor[segment];
        }
        if let Some(last) = path.last() {
            cursor[last] = value.clone();
        }
        let doc = serde_json::json!({ "settings": patch });
        let diff = save_prefs_patch(&self.config_dir, &doc)?;
        let refreshed = self.reload();
        self.settings = refreshed.settings;
        self.raw_settings = refreshed.raw_settings;
        Ok(diff)
    }

    pub(super) fn save_keybinding_override(
        &mut self,
        id: &str,
        value: Option<&pi_rs_p2_tui::KeyIdValue>,
    ) -> Result<Vec<String>, String> {
        if is_privileged(id) {
            return Err(format!(
                "launcher-owned keys are edited only in your own files ({id})"
            ));
        }
        let val = match value {
            Some(v) => serde_json::to_value(v).map_err(|e| e.to_string())?,
            None => Value::Null,
        };
        let doc = serde_json::json!({ "keybindings": { id: val } });
        let diff = save_prefs_patch(&self.config_dir, &doc)?;
        let refreshed = self.reload();
        self.keys = refreshed.keys;
        Ok(diff)
    }

    pub(super) fn quiet_tools_enabled(&self) -> bool {
        pi_rs_p2_tui::tools::plugin_tool_calls_is_quiet(Some(&self.settings))
    }

    pub(super) fn show_hardware_cursor(&self) -> bool {
        self.settings
            .values
            .get("showHardwareCursor")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub(super) fn editor_padding_x(&self) -> usize {
        self.settings
            .values
            .get("editorPaddingX")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize
    }

    pub(super) fn autocomplete_max_visible(&self) -> usize {
        self.settings
            .values
            .get("autocompleteMaxVisible")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize
    }

    pub(super) fn output_pad(&self) -> usize {
        self.settings
            .values
            .get("outputPad")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as usize
    }

    pub(super) fn double_escape_action(&self) -> &str {
        self.settings
            .values
            .get("doubleEscapeAction")
            .and_then(|v| v.as_str())
            .unwrap_or("tree")
    }

    pub(super) fn switch_theme(
        &mut self,
        name: &str,
        theme_ref: &pi_rs_p2_tui::ThemeRef,
        client: &mut pi_rs_p2_tui::host::TuiClient<pi_rs_tui::TuiAltScreen>,
    ) -> Result<String, String> {
        let name = name.trim();
        if !is_theme_setting(&Value::String(name.to_owned())) {
            return Err("Invalid theme name".to_owned());
        }
        let (paths, themes) = load_custom_themes(&self.agent_dir, self.no_themes);
        let resolved = resolve_theme(ResolveThemeOptions {
            setting: Some(name.to_owned()),
            terminal: self.terminal.clone(),
            custom_themes: themes.clone(),
            mode: self.color_mode,
        });
        if let Some(error) = resolved.error {
            return Err(error);
        }
        client.set_theme(theme_ref, resolved.theme.clone());
        self.theme = resolved.theme;
        let selected = resolve_theme_setting(Some(name), detect_terminal_theme(&self.terminal));
        let next_path = selected.and_then(|concrete| paths.get(&concrete).cloned());
        if let Some(path) = &next_path {
            if client
                .watch_theme(
                    path,
                    theme_ref.clone(),
                    self.color_mode,
                    self.terminal.clone(),
                )
                .is_err()
            {
                client.clear_theme_watch();
            }
        } else {
            client.clear_theme_watch();
        }
        self.theme_path = next_path;
        self.custom_themes = themes;
        self.custom_theme_paths = paths;
        self.current_theme = name.to_owned();
        if let Err(error) =
            save_prefs_patch(&self.config_dir, &serde_json::json!({ "theme": name }))
        {
            return Err(format!("Theme switched, but saving prefs failed: {error}"));
        }
        Ok(name.to_owned())
    }
}

/// Apply a partial patch to `<config_dir>/prefs.json` atomically (`0600` temp + rename)
/// and return leaf-by-leaf diff lines (`- path: old`, `+ path: new`).
pub fn save_prefs_patch(config_dir: &Path, patch: &Value) -> Result<Vec<String>, String> {
    let mut dir_builder = fs::DirBuilder::new();
    dir_builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }
    dir_builder
        .create(config_dir)
        .map_err(|e| format!("Failed to create config dir: {e}"))?;
    let prefs_path = config_dir.join("prefs.json");
    let before = read(&prefs_path)
        .filter(|(v, _)| v.is_object())
        .map(|(v, _)| v)
        .unwrap_or_else(|| Value::Object(Map::new()));
    let after = apply_prefs_patch(&before, patch);
    let diff = prefs_diff(&before, &after);

    let tmp_path = config_dir.join(format!(".prefs-{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write_res = (|| -> Result<(), String> {
        let mut f = opts
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp prefs file: {e}"))?;
        let text = serde_json::to_string_pretty(&after).map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        f.write_all(b"\n").map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&tmp_path, &prefs_path).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res?;
    Ok(diff)
}

pub fn apply_prefs_patch(current: &Value, patch: &Value) -> Value {
    let Some(patch_obj) = patch.as_object() else {
        return current.clone();
    };
    let mut next = current.as_object().cloned().unwrap_or_default();
    for (k, v) in patch_obj {
        if v.is_null() {
            next.remove(k);
        } else if v.is_object() {
            let base = next
                .get(k)
                .filter(|b| b.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let merged = apply_prefs_patch(&base, v);
            if merged.as_object().is_some_and(|m| !m.is_empty()) {
                next.insert(k.clone(), merged);
            } else {
                next.remove(k);
            }
        } else {
            next.insert(k.clone(), v.clone());
        }
    }
    Value::Object(next)
}

pub fn prefs_diff(before: &Value, after: &Value) -> Vec<String> {
    fn collect_leaves(value: &Value, prefix: &str, out: &mut HashMap<String, String>) {
        if let Some(obj) = value.as_object()
            && !obj.is_empty()
        {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            for key in keys {
                let path = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_leaves(&obj[key], &path, out);
            }
        } else if !prefix.is_empty() {
            out.insert(
                prefix.to_string(),
                serde_json::to_string(value).unwrap_or_default(),
            );
        }
    }
    let mut old = HashMap::new();
    let mut new = HashMap::new();
    collect_leaves(before, "", &mut old);
    collect_leaves(after, "", &mut new);
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut lines = Vec::new();
    for path in keys {
        let a = old.get(path);
        let b = new.get(path);
        if a == b {
            continue;
        }
        if let Some(prev) = a {
            lines.push(format!("- {path}: {prev}"));
        }
        if let Some(next) = b {
            lines.push(format!("+ {path}: {next}"));
        }
    }
    lines
}

type FileStamp = Option<(SystemTime, u64)>;

/// Polls preference files (`prefs.json`, `settings.json`, `keybindings.json`) for edits on disk.
pub struct PrefsWatcher {
    files: Vec<PathBuf>,
    stamps: Vec<FileStamp>,
    last_check: std::time::Instant,
}

impl PrefsWatcher {
    pub fn new(config_dir: &Path, agent_dir: &Path) -> Self {
        let files = vec![
            config_dir.join("prefs.json"),
            agent_dir.join("settings.json"),
            agent_dir.join("keybindings.json"),
        ];
        let stamps = files.iter().map(|p| stamp(p)).collect();
        Self {
            files,
            stamps,
            last_check: std::time::Instant::now(),
        }
    }

    pub fn from_prefs(prefs: &Preferences) -> Self {
        Self::new(&prefs.config_dir, &prefs.agent_dir)
    }

    pub fn poll_changed(&mut self) -> bool {
        if self.last_check.elapsed() < std::time::Duration::from_millis(250) {
            return false;
        }
        self.last_check = std::time::Instant::now();
        let next: Vec<FileStamp> = self.files.iter().map(|p| stamp(p)).collect();
        if next != self.stamps {
            self.stamps = next;
            return true;
        }
        false
    }
}

fn stamp(path: &Path) -> FileStamp {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some((modified, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_prefs_patch_merges_nested_objects_removes_nulls_and_returns_diff()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let diff1 = save_prefs_patch(
            dir.path(),
            &serde_json::json!({
                "theme": "dark",
                "settings": { "outputPad": 1, "markdown": { "mermaid": "final" } }
            }),
        )?;
        assert!(diff1.contains(&"+ theme: \"dark\"".to_string()));
        assert!(diff1.contains(&"+ settings.markdown.mermaid: \"final\"".to_string()));

        let diff2 = save_prefs_patch(
            dir.path(),
            &serde_json::json!({
                "theme": "light",
                "settings": { "outputPad": null }
            }),
        )?;
        assert!(diff2.contains(&"- theme: \"dark\"".to_string()));
        assert!(diff2.contains(&"+ theme: \"light\"".to_string()));
        assert!(diff2.contains(&"- settings.outputPad: 1".to_string()));
        let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("prefs.json"))?)?;
        assert_eq!(saved["settings"]["markdown"]["mermaid"], "final");
        assert!(saved["settings"].get("outputPad").is_none());
        Ok(())
    }

    #[test]
    fn prefs_watcher_detects_on_disk_changes() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = dir.path().join("cfg");
        let agent = dir.path().join("agent");
        fs::create_dir_all(&cfg)?;
        fs::create_dir_all(&agent)?;
        fs::write(cfg.join("prefs.json"), b"{}")?;
        let mut watcher = PrefsWatcher::new(&cfg, &agent);
        watcher.last_check = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(!watcher.poll_changed());
        fs::write(cfg.join("prefs.json"), b"{\"theme\":\"light\"}")?;
        watcher.last_check = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(watcher.poll_changed());
        Ok(())
    }
}
