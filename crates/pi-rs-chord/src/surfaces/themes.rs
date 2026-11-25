use crate::{
    Result,
    context::{Context, lock},
    state::{Batch, NativeObservation, NativeSource},
    watch::NativeWatchCall,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub mode: ThemeMode,
    pub colors: IndexMap<String, String>,
    #[serde(default)]
    pub web: IndexMap<String, String>,
    #[serde(default)]
    pub tui: IndexMap<String, String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemePick {
    pub dark: Option<String>,
    pub light: Option<String>,
}
pub fn fallback_theme(mode: ThemeMode) -> Theme {
    let colors = match mode {
        ThemeMode::Dark => [
            "#4c9aff", "#16181d", "#e6e6e6", "#8a8f98", "#ff6b6b", "#f0b429", "#3ecf8e",
        ],
        ThemeMode::Light => [
            "#0b62d6", "#ffffff", "#1c1e21", "#6b7280", "#c62828", "#b26a00", "#1b7f4b",
        ],
    };
    Theme {
        id: format!(
            "chord.{}",
            if mode == ThemeMode::Dark {
                "dark"
            } else {
                "light"
            }
        ),
        name: if mode == ThemeMode::Dark {
            "Dark"
        } else {
            "Light"
        }
        .into(),
        mode,
        colors: ["accent", "bg", "fg", "muted", "error", "warning", "success"]
            .into_iter()
            .zip(colors)
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        web: IndexMap::new(),
        tui: IndexMap::new(),
    }
}
pub fn active_theme(themes: &[Theme], pick: &ThemePick, mode: ThemeMode) -> Theme {
    let wanted = match mode {
        ThemeMode::Dark => &pick.dark,
        ThemeMode::Light => &pick.light,
    };
    themes
        .iter()
        .filter(|t| t.mode == mode)
        .find(|t| wanted.as_ref() == Some(&t.id))
        .or_else(|| themes.iter().find(|t| t.mode == mode))
        .cloned()
        .unwrap_or_else(|| fallback_theme(mode))
}
pub fn theme_variables(theme: &Theme) -> IndexMap<String, String> {
    let mut variables = theme
        .colors
        .iter()
        .map(|(k, v)| (format!("--{k}"), v.clone()))
        .collect::<IndexMap<_, _>>();
    for (key, value) in &theme.web {
        variables.insert(format!("--{key}"), value.clone());
    }
    variables
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorDepth {
    #[serde(rename = "truecolor")]
    Truecolor,
    #[serde(rename = "256")]
    Indexed,
}
pub fn terminal_color(theme: &Theme, role: &str, background: bool, depth: ColorDepth) -> String {
    let default = if background { "49" } else { "39" };
    let fallback = fallback_theme(theme.mode);
    let Some(value) = theme
        .tui
        .get(role)
        .or_else(|| theme.colors.get(role))
        .or_else(|| fallback.colors.get(role))
    else {
        return default.into();
    };
    let prefix = if background { 48 } else { 38 };
    if !value.is_empty()
        && value.len() <= 3
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u16>().is_ok_and(|n| n < 256)
    {
        return format!("{prefix};5;{value}");
    }
    let Some(hex) = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return default.into();
    };
    let Ok(rgb) = u32::from_str_radix(hex, 16) else {
        return default.into();
    };
    let [r, g, b] = [
        ((rgb >> 16) & 255) as i32,
        ((rgb >> 8) & 255) as i32,
        (rgb & 255) as i32,
    ];
    match depth {
        ColorDepth::Truecolor => format!("{prefix};2;{r};{g};{b}"),
        ColorDepth::Indexed => {
            let cube = [0, 95, 135, 175, 215, 255];
            let indices =
                [r, g, b].map(|v| (0..6).min_by_key(|i| (cube[*i] - v).pow(2)).unwrap_or(0));
            let cube_distance = [r, g, b]
                .into_iter()
                .zip(indices)
                .map(|(v, i)| (v - cube[i]).pow(2))
                .sum::<i32>();
            let gray = ((f64::from(r + g + b) / 3.0 - 8.0) / 10.0)
                .round()
                .clamp(0.0, 23.0) as i32;
            let gray_value = 8 + 10 * gray;
            let gray_distance = [r, g, b]
                .into_iter()
                .map(|v| (v - gray_value).pow(2))
                .sum::<i32>();
            let index = if gray_distance < cube_distance {
                232 + gray as usize
            } else {
                16 + 36 * indices[0] + 6 * indices[1] + indices[2]
            };
            format!("{prefix};5;{index}")
        }
    }
}
struct ThemeInputs {
    themes: Vec<Theme>,
    pick: ThemePick,
    mode: ThemeMode,
    stopped: bool,
}
pub struct ThemeFollower {
    inputs: Arc<Mutex<ThemeInputs>>,
    pub source: NativeSource<Theme>,
    _themes: NativeObservation<Vec<Theme>>,
    _pick: NativeObservation<ThemePick>,
}
impl ThemeFollower {
    pub fn follow(
        themes: NativeSource<Vec<Theme>>,
        pick: NativeSource<ThemePick>,
        mode: ThemeMode,
        batch: Batch,
        context: &Context,
    ) -> Result<Self> {
        let inputs = Arc::new(Mutex::new(ThemeInputs {
            themes: themes.value(),
            pick: pick.value(),
            mode,
            stopped: false,
        }));
        let source = {
            let inputs = lock(&inputs);
            NativeSource::new(
                batch,
                active_theme(&inputs.themes, &inputs.pick, mode),
                |a, b| a == b,
            )
        };
        let state = inputs.clone();
        let output = source.clone();
        let themes = themes.observe(
            context,
            move |call, ctx| {
                if let NativeWatchCall::Value { value } = call {
                    let next = {
                        let mut state = lock(&state);
                        if state.stopped {
                            return Ok(());
                        }
                        state.themes = value;
                        active_theme(&state.themes, &state.pick, state.mode)
                    };
                    output.set(next, ctx)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        let state = inputs.clone();
        let output = source.clone();
        let pick = pick.observe(
            context,
            move |call, ctx| {
                if let NativeWatchCall::Value { value } = call {
                    let next = {
                        let mut state = lock(&state);
                        if state.stopped {
                            return Ok(());
                        }
                        state.pick = value;
                        active_theme(&state.themes, &state.pick, state.mode)
                    };
                    output.set(next, ctx)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        Ok(Self {
            inputs,
            source,
            _themes: themes,
            _pick: pick,
        })
    }
    pub fn set_mode(&self, mode: ThemeMode, context: &Context) -> Result<()> {
        let next = {
            let mut inputs = lock(&self.inputs);
            if inputs.stopped {
                return Ok(());
            }
            inputs.mode = mode;
            active_theme(&inputs.themes, &inputs.pick, mode)
        };
        self.source.set(next, context)
    }
    pub fn stop(&self) {
        lock(&self.inputs).stopped = true;
    }
}
