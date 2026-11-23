use crate::keybindings::KeybindingsConfig;
use crate::keybindings::{is_privileged, sanitize_keybindings};
use crate::theme::ValidatedThemeJson;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const MAX_PREFERENCE_FILE: usize = 256 * 1024;
pub const MAX_CUSTOM_THEMES: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuiLocalPreferences {
    pub keybindings: KeybindingsConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    pub settings: TuiSettings,
    pub themes: IndexMap<String, ValidatedThemeJson>,
    pub problems: Vec<String>,
}

impl Default for TuiLocalPreferences {
    fn default() -> Self {
        Self {
            keybindings: KeybindingsConfig::new(),
            theme: None,
            settings: TuiSettings::default(),
            themes: IndexMap::new(),
            problems: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TuiSettings {
    #[serde(flatten)]
    pub values: IndexMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub plugins: IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TerminalSettings {
    #[serde(flatten)]
    pub values: IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MarkdownSettings {
    #[serde(rename = "codeBlockIndent", skip_serializing_if = "Option::is_none")]
    pub indent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuiServerPreferences {
    pub theme: Option<String>,
    pub keybindings: KeybindingsConfig,
    pub settings: TuiSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedSettings {
    pub settings: TuiSettings,
    pub invalid: Vec<String>,
}

pub fn sanitize_settings(raw: &serde_json::Value) -> SanitizedSettings {
    let mut settings = TuiSettings::default();
    let mut invalid = Vec::new();

    let Some(obj) = raw.as_object() else {
        return SanitizedSettings { settings, invalid };
    };
    for (key, v) in obj {
        let one_of = |options: &[&str]| v.as_str().is_some_and(|s| options.contains(&s));
        let range = |a, b| v.as_u64().is_some_and(|x| (a..=b).contains(&x));
        let valid = match key.as_str() {
            "quietStartup" => v.is_boolean() || v == "header",
            "editorPaddingX" => range(0, 3),
            "outputPad" => range(0, 1),
            "autocompleteMaxVisible" => range(3, 20),
            "showHardwareCursor" | "hideThinkingBlock" | "fullscreenCopyOnSelect" => v.is_boolean(),
            "doubleEscapeAction" => one_of(&["tree", "fork", "none"]),
            "fullscreenExitOutput" => one_of(&["transcript", "resume-hint"]),
            "fullscreenScrollbar" => one_of(&["auto", "always", "hidden"]),
            "fullscreenWheelScrollLines" => v == "auto" || range(1, 100),
            "terminal" | "markdown" => {
                if let Some(children) = v.as_object() {
                    for (name, val) in children {
                        let ok = match (key.as_str(), name.as_str()) {
                            (
                                "terminal",
                                "clearOnShrink"
                                | "showTerminalProgress"
                                | "showImages"
                                | "hyperlinks"
                                | "trueColor",
                            ) => val.is_boolean(),
                            ("terminal", "imageWidthCells") => {
                                val.as_u64().is_some_and(|x| (1..=500).contains(&x))
                            }
                            ("terminal", "images") => {
                                val == false || val == "kitty" || val == "iterm2"
                            }
                            ("markdown", "mermaid") => val
                                .as_str()
                                .is_some_and(|s| matches!(s, "off" | "final" | "streaming")),
                            ("markdown", "codeBlockIndent") => val.as_str().is_some_and(|s| {
                                s.len() <= 8 && s.bytes().all(|b| matches!(b, b' ' | b'\t'))
                            }),
                            _ => continue,
                        };
                        if !ok {
                            invalid.push(format!("{key}.{name}"));
                            continue;
                        }
                        if key == "terminal" {
                            settings
                                .terminal
                                .get_or_insert_with(Default::default)
                                .values
                                .insert(name.clone(), val.clone());
                        } else {
                            let md = settings.markdown.get_or_insert_with(Default::default);
                            let field = if name == "mermaid" {
                                &mut md.mermaid
                            } else {
                                &mut md.indent
                            };
                            *field = val.as_str().map(str::to_owned);
                        }
                    }
                    continue;
                }
                false
            }
            "plugins" => {
                if let Some(map) = v.as_object().filter(|m| {
                    m.len() <= 64
                        && m.values().all(|v| {
                            v.as_object().is_some_and(|p| {
                                p.len() <= 32
                                    && p.values().all(|x| {
                                        x.is_number()
                                            || x.is_boolean()
                                            || x.as_str().is_some_and(|s| s.chars().count() <= 200)
                                    })
                            })
                        })
                }) {
                    settings.plugins.extend(map.clone());
                    continue;
                }
                false
            }
            _ => continue,
        };
        if valid {
            settings.values.insert(key.clone(), v.clone());
        } else {
            invalid.push(key.clone());
        }
    }
    SanitizedSettings { settings, invalid }
}

pub fn merge_settings(layers: Vec<TuiSettings>) -> TuiSettings {
    let mut merged = TuiSettings::default();
    for layer in layers {
        merged.values.extend(layer.values);
        if let Some(term) = layer.terminal {
            let target = merged
                .terminal
                .get_or_insert_with(TerminalSettings::default);
            target.values.extend(term.values);
        }
        if let Some(md) = layer.markdown {
            let target = merged.markdown.get_or_insert_with(Default::default);
            if md.mermaid.is_some() {
                target.mermaid = md.mermaid;
            }
            if md.indent.is_some() {
                target.indent = md.indent;
            }
        }
        for (k, v) in layer.plugins {
            if let Some(existing) = merged.plugins.get_mut(&k).and_then(|x| x.as_object_mut())
                && let Some(values) = v.as_object()
            {
                existing.extend(values.clone());
            } else {
                merged.plugins.insert(k, v);
            }
        }
    }
    merged
}

pub fn is_theme_setting(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.split('/').count() <= 2
            && s.split('/').all(|p| {
                (1..=64).contains(&p.encode_utf16().count()) && !p.chars().any(|c| c < ' ')
            })
    })
}

pub fn sanitize_server_preferences(raw: &serde_json::Value) -> TuiServerPreferences {
    let mut keybindings = KeybindingsConfig::new();
    let mut settings = TuiSettings::default();

    if let Some(obj) = raw.as_object() {
        if let Some(kb) = obj.get("keybindings") {
            let sanitized = sanitize_keybindings(kb);
            keybindings = sanitized.config;
            keybindings.retain(|id, _| !is_privileged(id));
        }
        if let Some(s) = obj.get("settings") {
            let sanitized = sanitize_settings(s);
            settings = sanitized.settings;
        }
    }

    TuiServerPreferences {
        theme: raw
            .get("theme")
            .filter(|v| is_theme_setting(v))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        keybindings,
        settings,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectivePreferences {
    pub keybindings: KeybindingsConfig,
    pub theme: Option<String>,
    pub settings: TuiSettings,
}

pub fn effective_preferences(
    server: Option<&TuiServerPreferences>,
    local: &TuiLocalPreferences,
) -> EffectivePreferences {
    let mut keybindings = server.map(|s| s.keybindings.clone()).unwrap_or_default();
    keybindings.retain(|id, _| !is_privileged(id));
    keybindings.extend(local.keybindings.clone());

    EffectivePreferences {
        keybindings,
        theme: local
            .theme
            .clone()
            .or_else(|| server.and_then(|s| s.theme.clone())),
        settings: merge_settings(
            server
                .iter()
                .map(|s| s.settings.clone())
                .chain(std::iter::once(local.settings.clone()))
                .collect(),
        ),
    }
}

pub fn parse_local_preferences(raw: &serde_json::Value) -> Result<TuiLocalPreferences, String> {
    let mut themes = IndexMap::new();
    if let Some(map) = raw["themes"].as_object() {
        for (name, value) in map.iter().take(MAX_CUSTOM_THEMES) {
            if !name.contains('/')
                && is_theme_setting(&serde_json::Value::String(name.clone()))
                && let Ok(theme) = crate::theme::validate_theme_json(name, value)
            {
                themes.insert(name.clone(), theme);
            }
        }
    }
    Ok(TuiLocalPreferences {
        keybindings: sanitize_keybindings(&raw["keybindings"]).config,
        settings: sanitize_settings(&raw["settings"]).settings,
        theme: raw
            .get("theme")
            .filter(|v| is_theme_setting(v))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        themes,
        problems: raw["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .take(50)
            .map(str::to_owned)
            .collect(),
    })
}

pub type TuiSettingField = serde_json::Value;
