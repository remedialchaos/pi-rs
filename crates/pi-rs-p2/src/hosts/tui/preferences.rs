use pi_rs_p2_tui::{
    KeybindingsConfig, MAX_CUSTOM_THEMES, MAX_PREFERENCE_FILE, ResolveThemeOptions, TerminalState,
    Theme, ValidatedThemeJson, detect_terminal_theme, is_theme_setting, resolve_theme,
    resolve_theme_setting, sanitize_keybindings, validate_theme_json,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct Preferences {
    pub(super) mouse: bool,
    pub(super) theme_path: Option<PathBuf>,
    pub(super) keys: KeybindingsConfig,
    pub(super) theme: Theme,
    config_dir: PathBuf,
    agent_dir: PathBuf,
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
    Some((serde_json::from_slice(&bytes).ok()?, bytes.len()))
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
        let settings = read(&agent.join("settings.json"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let raw_keys = read(&agent.join("keybindings.json"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let mut keys = sanitize_keybindings(&raw_keys).config;
        keys.extend(sanitize_keybindings(&prefs["keybindings"]).config);
        let (paths, themes) = load_custom_themes(&agent, no_themes);
        let setting = [
            setting.as_deref(),
            prefs["theme"].as_str(),
            settings["theme"].as_str(),
        ]
        .into_iter()
        .flatten()
        .find(|s| is_theme_setting(&Value::String((*s).into())))
        .map(str::to_owned);
        let terminal = TerminalState::default();
        let color_mode = pi_rs_tui::get_terminal_color_mode(pi_rs_tui::get_capabilities());
        let selected = resolve_theme_setting(setting.as_deref(), detect_terminal_theme(&terminal));
        let theme_path = selected.and_then(|name| paths.get(&name).cloned());
        let current_theme = setting
            .clone()
            .unwrap_or_else(|| pi_rs_p2_tui::theme::SYSTEM_THEME_NAME.to_owned());
        let theme = resolve_theme(ResolveThemeOptions {
            setting,
            terminal: terminal.clone(),
            custom_themes: themes.clone(),
            mode: color_mode,
        })
        .theme;
        Self {
            keys,
            theme,
            theme_path,
            mouse: prefs["mouse"].as_str() != Some("off"),
            config_dir: config.to_owned(),
            agent_dir: agent,
            no_themes,
            current_theme,
            custom_themes: themes,
            custom_theme_paths: paths,
            color_mode,
            terminal,
        }
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
        client.set_theme(theme_ref, resolved.theme);
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
        if let Err(error) = save_theme_preference(&self.config_dir, name) {
            return Err(format!("Theme switched, but saving prefs failed: {error}"));
        }
        Ok(name.to_owned())
    }
}

fn save_theme_preference(config_dir: &Path, theme_name: &str) -> std::io::Result<()> {
    fs::create_dir_all(config_dir)?;
    let prefs_path = config_dir.join("prefs.json");
    let mut obj = read(&prefs_path)
        .and_then(|(v, _)| v.as_object().cloned())
        .unwrap_or_default();
    obj.insert("theme".to_owned(), Value::String(theme_name.to_owned()));
    let bytes = serde_json::to_vec_pretty(&Value::Object(obj)).map_err(std::io::Error::other)?;
    let tmp_path = config_dir.join("prefs.json.tmp");
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, &prefs_path)?;
    Ok(())
}
