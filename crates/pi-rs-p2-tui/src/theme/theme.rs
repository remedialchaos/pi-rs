use crate::theme::builtin_themes::{DARK_THEME, LIGHT_THEME};
use crate::theme::theme_schema::{ColorValue, ValidatedThemeJson};
use crate::theme::theme_tokens::THEME_TOKENS;
use pi_rs_tui::{
    Color, RgbColor, TerminalColorMode, TerminalColors, TextAttributes, background_ansi,
    foreground_ansi, parse_color, style_text_with_ansi,
};
use std::collections::HashMap;
use thiserror::Error;

pub const SYSTEM_THEME_NAME: &str = "system";

pub type TerminalTheme = String;
pub type ThemeAppearance = TerminalTheme;

#[derive(Debug, Clone, Default)]
pub struct TerminalState {
    pub colors: TerminalColors,
    pub scheme: Option<String>,
    pub pending: bool,
}

pub static BUILTIN_THEMES: std::sync::LazyLock<[(&str, &ValidatedThemeJson); 2]> =
    std::sync::LazyLock::new(|| [("dark", &*DARK_THEME), ("light", &*LIGHT_THEME)]);

#[derive(Debug, Clone, Default)]
pub struct ThemeStyle {
    pub attributes: TextAttributes,
    pub color: Option<String>,
}

fn resolve_var_refs(
    value: &ColorValue,
    vars: &HashMap<String, ColorValue>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<String, ThemeError> {
    match value {
        ColorValue::String(s) => {
            if let Some((var_name, _)) = vars.get_key_value(s) {
                if !visited.insert(var_name.to_string()) {
                    return Err(ThemeError::CyclicVariable(var_name.to_string()));
                }
                if let Some(var_value) = vars.get(var_name) {
                    return resolve_var_refs(var_value, vars, visited);
                }
                return Err(ThemeError::UndefinedVariable(var_name.to_string()));
            }
            Ok(s.clone())
        }
        ColorValue::Number(n) => Ok(n.to_string()),
        ColorValue::Array(_) => Err(ThemeError::InvalidColorValue),
        ColorValue::Object(_) => Err(ThemeError::InvalidColorValue),
    }
}

fn with_theme_token_fallbacks(values: &HashMap<String, ColorValue>) -> HashMap<String, ColorValue> {
    let mut resolved = HashMap::new();
    for (token, descriptor) in THEME_TOKENS.iter() {
        if let Some(value) = values.get(token) {
            resolved.insert(token.clone(), value.clone());
        } else if let Some(fallback) = &descriptor.fallback
            && let Some(fallback_value) = resolved.get(fallback)
        {
            resolved.insert(token.clone(), fallback_value.clone());
        }
    }
    resolved
}

fn split_theme_colors(
    colors: &HashMap<String, ColorValue>,
) -> (HashMap<String, ColorValue>, HashMap<String, ColorValue>) {
    let mut foreground = HashMap::new();
    let mut background = HashMap::new();
    for (token, descriptor) in THEME_TOKENS.iter() {
        if let Some(value) = colors.get(token) {
            match descriptor.slot {
                crate::theme::theme_tokens::ThemeTokenSlot::Foreground => {
                    foreground.insert(token.clone(), value.clone());
                }
                crate::theme::theme_tokens::ThemeTokenSlot::Background => {
                    background.insert(token.clone(), value.clone());
                }
            }
        }
    }
    (foreground, background)
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub appearance: ThemeAppearance,
    pub color_mode: TerminalColorMode,
    pub terminal: TerminalState,
    pub vars: HashMap<String, String>,
    pub colors: HashMap<String, String>,
    pub resolved_fg: HashMap<String, Color>,
    pub resolved_bg: HashMap<String, Color>,
}

#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("Undefined variable: {0}")]
    UndefinedVariable(String),
    #[error("Cyclic variable reference: {0}")]
    CyclicVariable(String),
    #[error("Invalid color value")]
    InvalidColorValue,
    #[error("Failed to parse color: {0}")]
    ParseColor(String),
    #[error("Unresolved variable: {0}")]
    UnresolvedVariable(String),
}

impl Theme {
    pub fn new(
        json: &ValidatedThemeJson,
        mode: TerminalColorMode,
        terminal: TerminalState,
    ) -> Result<Self, ThemeError> {
        let appearance = json.appearance.clone();
        let mut vars = HashMap::new();
        for (k, v) in &json.vars {
            vars.insert(
                k.clone(),
                resolve_var_refs(v, &json.vars, &mut std::collections::HashSet::new())?,
            );
        }

        let mut colors = HashMap::new();
        let with_fallbacks = with_theme_token_fallbacks(&json.colors);
        for (token, value) in &with_fallbacks {
            colors.insert(
                token.clone(),
                resolve_var_refs(value, &json.vars, &mut std::collections::HashSet::new())?,
            );
        }

        let (fg_colors, bg_colors) = split_theme_colors(&with_fallbacks);
        let mut resolved_fg = HashMap::new();
        let mut resolved_bg = HashMap::new();

        for (token, value) in &fg_colors {
            let color_str =
                resolve_var_refs(value, &json.vars, &mut std::collections::HashSet::new())?;
            if color_str.is_empty() {
                continue;
            }
            let color = color_str
                .parse::<u8>()
                .map(Color::Indexed)
                .or_else(|_| parse_color(&color_str))
                .map_err(ThemeError::ParseColor)?;
            resolved_fg.insert(token.clone(), color);
        }

        for (token, value) in &bg_colors {
            let color_str =
                resolve_var_refs(value, &json.vars, &mut std::collections::HashSet::new())?;
            if color_str.is_empty() {
                continue;
            }
            let color = color_str
                .parse::<u8>()
                .map(Color::Indexed)
                .or_else(|_| parse_color(&color_str))
                .map_err(ThemeError::ParseColor)?;
            resolved_bg.insert(token.clone(), color);
        }

        Ok(Self {
            appearance,
            color_mode: mode,
            terminal,
            vars,
            colors,
            resolved_fg,
            resolved_bg,
        })
    }

    pub fn get_color_mode(&self) -> TerminalColorMode {
        self.color_mode
    }

    pub fn fg(&self, token: &str, text: &str) -> String {
        if let Some(color) = self.resolved_fg.get(token) {
            style_text_with_ansi(
                text,
                Some(&foreground_ansi(color, self.color_mode)),
                None,
                &TextAttributes::default(),
            )
        } else {
            text.to_string()
        }
    }

    pub fn bg(&self, token: &str, text: &str) -> String {
        if let Some(color) = self.resolved_bg.get(token) {
            style_text_with_ansi(
                text,
                None,
                Some(&background_ansi(color, self.color_mode)),
                &TextAttributes::default(),
            )
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        let attrs = TextAttributes {
            bold: true,
            ..Default::default()
        };
        style_text_with_ansi(text, None, None, &attrs)
    }

    pub fn italic(&self, text: &str) -> String {
        let attrs = TextAttributes {
            italic: true,
            ..Default::default()
        };
        style_text_with_ansi(text, None, None, &attrs)
    }

    pub fn underline(&self, text: &str) -> String {
        let attrs = TextAttributes {
            underline: true,
            ..Default::default()
        };
        style_text_with_ansi(text, None, None, &attrs)
    }

    pub fn strikethrough(&self, text: &str) -> String {
        let attrs = TextAttributes {
            strikethrough: true,
            ..Default::default()
        };
        style_text_with_ansi(text, None, None, &attrs)
    }

    pub fn highlight(&self, code: &str, lang: &str) -> String {
        super::syntax::highlight(self, code, Some(lang)).join("\n")
    }
}

pub fn create_theme(
    json: &ValidatedThemeJson,
    mode: TerminalColorMode,
    terminal: TerminalState,
) -> Result<Theme, ThemeError> {
    Theme::new(json, mode, terminal)
}

pub fn create_system_theme(mode: TerminalColorMode, terminal: TerminalState) -> Theme {
    use super::system_theme::{
        SystemThemeInput, ThemeAppearance, generate_system_theme_colors, indexed_colors,
    };
    let appearance = detect_terminal_theme(&terminal);
    let system = if let Some(background) = terminal.colors.background {
        generate_system_theme_colors(SystemThemeInput {
            background,
            foreground: terminal.colors.foreground,
            ansi: terminal.colors.palette.clone(),
            color_mode: None,
        })
    } else {
        indexed_colors(
            1.0,
            Some(if appearance == "light" {
                ThemeAppearance::Light
            } else {
                ThemeAppearance::Dark
            }),
        )
    };
    let mut resolved_fg = HashMap::new();
    let mut resolved_bg = HashMap::new();
    for (key, value) in &system.colors {
        if let Some(token) = THEME_TOKENS.get(key)
            && let Ok(color) = value
                .parse::<u8>()
                .map(Color::Indexed)
                .or_else(|_| parse_color(value))
        {
            match token.slot {
                super::ThemeTokenSlot::Foreground => &mut resolved_fg,
                super::ThemeTokenSlot::Background => &mut resolved_bg,
            }
            .insert(key.clone(), color);
        }
    }
    Theme {
        appearance,
        color_mode: mode,
        terminal,
        vars: HashMap::new(),
        colors: system.colors,
        resolved_fg,
        resolved_bg,
    }
}

pub fn parse_auto_theme_setting(setting: Option<&str>) -> Option<(String, String)> {
    let (light, dark) = setting?.split_once('/')?;
    let (light, dark) = (light.trim(), dark.trim());
    (!light.is_empty() && !dark.is_empty() && !dark.contains('/'))
        .then(|| (light.into(), dark.into()))
}

pub fn resolve_theme_setting(
    setting: Option<&str>,
    terminal_theme: TerminalTheme,
) -> Option<String> {
    let s = setting?;
    if let Some((light, dark)) = parse_auto_theme_setting(Some(s)) {
        return Some(if terminal_theme == "light" {
            light
        } else {
            dark
        });
    }
    (!s.contains('/')).then(|| s.into())
}

#[derive(Debug, Clone)]
pub struct ResolveThemeOptions {
    pub setting: Option<String>,
    pub terminal: TerminalState,
    pub custom_themes: HashMap<String, ValidatedThemeJson>,
    pub mode: TerminalColorMode,
}

#[derive(Debug, Clone)]
pub struct ResolveThemeResult {
    pub theme: Theme,
    pub error: Option<String>,
}

pub fn resolve_theme(options: ResolveThemeOptions) -> ResolveThemeResult {
    let terminal_theme = detect_terminal_theme(&options.terminal);
    let setting = resolve_theme_setting(options.setting.as_deref(), terminal_theme.clone());

    let (json, error) = match setting.as_deref() {
        Some(name) if name == SYSTEM_THEME_NAME => {
            return ResolveThemeResult {
                theme: create_system_theme(options.mode, options.terminal),
                error: None,
            };
        }
        Some(name) => {
            if let Some(theme) = options.custom_themes.get(name) {
                (theme, None)
            } else if let Some((_, theme)) = BUILTIN_THEMES.iter().find(|(n, _)| *n == name) {
                (*theme, None)
            } else {
                return ResolveThemeResult {
                    theme: create_system_theme(options.mode, options.terminal),
                    error: Some(format!("Unknown theme: {name}")),
                };
            }
        }
        None => {
            return ResolveThemeResult {
                theme: create_system_theme(options.mode, options.terminal),
                error: None,
            };
        }
    };

    match create_theme(json, options.mode, options.terminal.clone()) {
        Ok(theme) => ResolveThemeResult { theme, error },
        Err(e) => ResolveThemeResult {
            theme: create_system_theme(options.mode, options.terminal),
            error: Some(e.to_string()),
        },
    }
}

pub fn parse_terminal_state(raw: &serde_json::Value) -> Result<TerminalState, serde_json::Error> {
    let rgb = |v: &serde_json::Value| -> Option<RgbColor> {
        let channel = |key: &str| {
            v.get(key)?
                .as_f64()
                .filter(|x| (0.0..=255.0).contains(x) && x.fract() == 0.0)
        };
        Some(RgbColor {
            r: channel("r")?,
            g: channel("g")?,
            b: channel("b")?,
        })
    };
    let colors = &raw["colors"];
    let palette = colors["palette"]
        .as_array()
        .filter(|a| a.len() == 16)
        .and_then(|a| a.iter().map(rgb).collect());
    Ok(TerminalState {
        colors: TerminalColors {
            foreground: rgb(&colors["foreground"]),
            background: rgb(&colors["background"]),
            palette,
        },
        scheme: raw["scheme"]
            .as_str()
            .filter(|s| matches!(*s, "light" | "dark"))
            .map(str::to_owned),
        pending: raw["pending"].as_bool() == Some(true),
    })
}
pub fn detect_terminal_theme(terminal: &TerminalState) -> String {
    if let Some(scheme) = &terminal.scheme {
        return scheme.clone();
    }
    match terminal
        .colors
        .background
        .map(|bg| super::system_theme::terminal_appearance(bg, terminal.colors.foreground))
    {
        Some(super::system_theme::ThemeAppearance::Light) => "light".into(),
        _ => "dark".into(),
    }
}

impl Default for Theme {
    fn default() -> Self {
        let mode = TerminalColorMode::Truecolor;
        create_theme(&DARK_THEME, mode, TerminalState::default())
            .unwrap_or_else(|_| create_system_theme(mode, TerminalState::default()))
    }
}
