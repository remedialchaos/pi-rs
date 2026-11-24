use pi_rs_p2_tui::{
    KeybindingsConfig, MAX_CUSTOM_THEMES, MAX_PREFERENCE_FILE, ResolveThemeOptions, Theme,
    is_theme_setting, resolve_theme, sanitize_keybindings, validate_theme_json,
};
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct Preferences {
    pub(super) mouse: bool,
    pub(super) theme_path: Option<PathBuf>,
    pub(super) keys: KeybindingsConfig,
    pub(super) theme: Theme,
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
        let mut paths = std::collections::HashMap::new();
        let mut themes = std::collections::HashMap::new();
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
        let setting = [
            setting.as_deref(),
            prefs["theme"].as_str(),
            settings["theme"].as_str(),
        ]
        .into_iter()
        .flatten()
        .find(|s| is_theme_setting(&Value::String((*s).into())))
        .map(str::to_owned);
        let terminal = Default::default();
        let selected = pi_rs_p2_tui::resolve_theme_setting(
            setting.as_deref(),
            pi_rs_p2_tui::detect_terminal_theme(&terminal),
        );
        let theme_path = selected.and_then(|name| paths.remove(&name));
        let theme = resolve_theme(ResolveThemeOptions {
            setting,
            terminal,
            custom_themes: themes,
            mode: pi_rs_tui::get_terminal_color_mode(pi_rs_tui::get_capabilities()),
        })
        .theme;
        Self {
            keys,
            theme,
            theme_path,
            mouse: prefs["mouse"].as_str() != Some("off"),
        }
    }
}
