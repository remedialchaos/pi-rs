//! Live interactive theme selection and rendering.

mod recipe;
mod syntax;
mod system;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use pi_rs_tui::components::TextStyleFn;
use pi_rs_tui::terminal_colors::{OscColorTarget, parse_osc_color_response};
use pi_rs_tui::terminal_image::{get_capabilities, get_terminal_color_mode};
use pi_rs_tui::{
    Color, ColorMixSpace, MarkdownTheme, RgbColor, TerminalColorMode, TerminalColorScheme,
    TerminalColors, background_ansi, color_to_oklch, foreground_ansi, indexed_color, mix_colors,
    parse_color, parse_terminal_color_scheme_report,
};
use serde_json::Value;

const DARK: &str = include_str!(
    "../../../../../../../packages/coding-agent/src/modes/interactive/theme/dark.json"
);
const LIGHT: &str = include_str!(
    "../../../../../../../packages/coding-agent/src/modes/interactive/theme/light.json"
);
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}
const SCHEMA: &str =
    include_str!("../../../../../../../packages/coding-agent/schemas/theme.schema.json");
const SYSTEM: &str = "system";
const BACKGROUNDS: &[&str] = &[
    "selectedBg",
    "searchMatchBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const FALLBACKS: &[(&str, &str)] = &[
    ("scrollbarTrack", "muted"),
    ("scrollbarThumb", "text"),
    ("searchMatchBg", "selectedBg"),
    ("searchMatchText", "text"),
    ("thinkingMax", "thinkingXhigh"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeError(String);

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ThemeError {}

struct Schema {
    validator: pi_rs_schema::Validator,
    tokens: Vec<String>,
}

static THEME_SCHEMA: LazyLock<Result<Schema, ThemeError>> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(SCHEMA).map_err(|err| ThemeError(err.to_string()))?;
    let tokens = schema
        .pointer("/properties/colors/properties")
        .and_then(Value::as_object)
        .ok_or_else(|| ThemeError("Embedded theme schema has no color tokens".into()))?
        .keys()
        .cloned()
        .collect();
    let validator = pi_rs_schema::compile(&schema).map_err(|err| ThemeError(err.to_string()))?;
    Ok(Schema { validator, tokens })
});

#[derive(Debug, PartialEq)]
struct Paint {
    open: String,
    close: &'static str,
    background: bool,
    color: Option<Color>,
    dim: bool,
}

impl Paint {
    fn new(token: &str, color: Option<Color>, dim: bool, mode: TerminalColorMode) -> Self {
        let background = BACKGROUNDS.contains(&token);
        let mut open = match (color, background) {
            (Some(color), true) => background_ansi(&color, mode),
            (Some(color), false) => foreground_ansi(&color, mode),
            (None, true) => "\x1b[49m".into(),
            (None, false) => "\x1b[39m".into(),
        };
        let close = if background {
            "\x1b[49m"
        } else if dim {
            open.push_str("\x1b[2m");
            "\x1b[22;39m"
        } else {
            "\x1b[39m"
        };
        Self {
            open,
            close,
            background,
            color,
            dim,
        }
    }

    fn append(&self, out: &mut String, text: &str) {
        out.push_str(&self.open);
        out.push_str(text);
        out.push_str(self.close);
    }
}

#[derive(Debug, PartialEq)]
struct Palette {
    name: String,
    source: Option<PathBuf>,
    appearance: Option<TerminalColorScheme>,
    paint: HashMap<String, Paint>,
}

impl Palette {
    fn parse(content: &str, label: &str, mode: TerminalColorMode) -> Result<Self, ThemeError> {
        let invalid = |message: &str| ThemeError(format!("Invalid theme {label:?}: {message}"));
        let value: Value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
            .map_err(|err| invalid(&err.to_string()))?;
        let schema = THEME_SCHEMA.as_ref().map_err(Clone::clone)?;
        let errors = schema.validator.errors(&pi_rs_schema::Value::from(&value));
        if !errors.is_empty() {
            let message = errors
                .iter()
                .map(|err| format!("{}: {}", err.path, err.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(invalid(&message));
        }
        let name = value["name"]
            .as_str()
            .ok_or_else(|| invalid("expected a theme name"))?;
        let vars = value["vars"].as_object();
        let colors = value["colors"]
            .as_object()
            .ok_or_else(|| invalid("expected a colors object"))?;
        let mut paint = HashMap::with_capacity(schema.tokens.len());
        for token in &schema.tokens {
            let input = colors
                .get(token)
                .or_else(|| {
                    let (_, fallback) = FALLBACKS.iter().find(|(key, _)| *key == token.as_str())?;
                    colors.get(*fallback)
                })
                .ok_or_else(|| invalid(&format!("missing color token {token}")))?;
            let color = resolve_color(input, vars)
                .map_err(|err| invalid(&format!("/colors/{token}: {err}")))?;
            paint.insert(token.clone(), Paint::new(token, color, false, mode));
        }
        let appearance = match value["appearance"].as_str() {
            Some("dark") => Some(TerminalColorScheme::Dark),
            Some("light") => Some(TerminalColorScheme::Light),
            _ => infer_appearance(&paint),
        };
        Ok(Self {
            name: name.into(),
            source: None,
            appearance,
            paint,
        })
    }

    fn system(terminal: &TerminalState, mode: TerminalColorMode) -> Self {
        let paint = system::generate(&terminal.colors, terminal.pending)
            .map(|(token, color, dim)| (token.to_owned(), Paint::new(token, color, dim, mode)))
            .collect();
        Self {
            name: SYSTEM.into(),
            source: None,
            appearance: None,
            paint,
        }
    }

    fn append_fg(&self, out: &mut String, token: &str, text: &str) {
        if let Some(paint) = self.paint.get(token) {
            paint.append(out, text);
        } else {
            out.push_str(text);
        }
    }
}

fn infer_appearance(paint: &HashMap<String, Paint>) -> Option<TerminalColorScheme> {
    let (mut foreground, mut background) = ((0.0, 0), (0.0, 0));
    for paint in paint.values() {
        if let Some(color) = paint.color
            && !matches!(color, Color::Indexed(0..=15))
        {
            let average = if paint.background {
                &mut background
            } else {
                &mut foreground
            };
            average.0 += color_to_oklch(&color).l;
            average.1 += 1;
        }
    }
    let mean = |(sum, count): (f64, u32)| (count != 0).then(|| sum / f64::from(count));
    let dark = match (mean(foreground), mean(background)) {
        (Some(fg), Some(bg)) => bg < fg,
        (Some(fg), None) => fg > 0.5,
        (None, Some(bg)) => bg < 0.5,
        (None, None) => return None,
    };
    Some(if dark {
        TerminalColorScheme::Dark
    } else {
        TerminalColorScheme::Light
    })
}

fn resolve_color<'a>(
    mut value: &'a Value,
    vars: Option<&'a serde_json::Map<String, Value>>,
) -> Result<Option<Color>, String> {
    let mut visited = HashSet::new();
    loop {
        if let Some(index) = value.as_f64() {
            return indexed_color(index).map(Some);
        }
        let text = value
            .as_str()
            .ok_or("expected a color or variable reference")?;
        if text.is_empty() {
            return Ok(None);
        }
        if text.starts_with('#')
            || text.get(..6).is_some_and(|prefix| {
                prefix.eq_ignore_ascii_case("oklch(") || prefix.eq_ignore_ascii_case("okhsl(")
            })
        {
            return parse_color(text).map(Some);
        }
        if !visited.insert(text) {
            return Err(format!("Circular variable reference detected: {text}"));
        }
        value = vars
            .and_then(|vars| vars.get(text))
            .ok_or_else(|| format!("Variable reference not found: {text}"))?;
    }
}

struct TerminalState {
    colors: TerminalColors,
    scheme: Option<TerminalColorScheme>,
    env_scheme: Option<TerminalColorScheme>,
    pending: bool,
    osc_palette: [Option<RgbColor>; 16],
}

impl TerminalState {
    fn appearance(&self) -> TerminalColorScheme {
        if let Some(background) = self.colors.background {
            system::appearance(background, self.colors.foreground)
        } else {
            self.scheme
                .or(self.env_scheme)
                .unwrap_or(TerminalColorScheme::Dark)
        }
    }
}

fn env_scheme(value: &str) -> Option<TerminalColorScheme> {
    let index = value.rsplit(';').next()?.trim();
    if index.is_empty() || index.len() > 2 || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match index.parse::<u8>().ok()? {
        0..=6 | 8 => Some(TerminalColorScheme::Dark),
        7 | 9..=15 => Some(TerminalColorScheme::Light),
        _ => None,
    }
}

fn setting_name(setting: &str, appearance: TerminalColorScheme) -> Result<&str, ThemeError> {
    if let Some((light, dark)) = setting.split_once('/') {
        let (light, dark) = (light.trim(), dark.trim());
        if light.is_empty() || dark.is_empty() || dark.contains('/') {
            return Err(ThemeError(format!(
                "Invalid automatic theme setting: {setting:?}"
            )));
        }
        return Ok(match appearance {
            TerminalColorScheme::Light => light,
            TerminalColorScheme::Dark => dark,
        });
    }
    Ok(setting)
}

fn theme_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json") && path.is_file())
        .collect();
    paths.sort_unstable();
    paths
}

fn read_palette(path: &Path, mode: TerminalColorMode) -> Result<Palette, ThemeError> {
    let content = fs::read_to_string(path)
        .map_err(|err| ThemeError(format!("Reading theme {}: {err}", path.display())))?;
    let mut palette = Palette::parse(&content, &path.display().to_string(), mode)?;
    palette.source = Some(path.to_owned());
    Ok(palette)
}

fn load_palette(
    directory: &Path,
    registered: Option<&[PathBuf]>,
    name: &str,
    terminal: &TerminalState,
    mode: TerminalColorMode,
) -> Result<Palette, ThemeError> {
    match name {
        SYSTEM => return Ok(Palette::system(terminal, mode)),
        "dark" => return Palette::parse(DARK, name, mode),
        "light" => return Palette::parse(LIGHT, name, mode),
        _ => {}
    }
    let filename = format!("{name}.json");
    let candidate = Path::new(&filename);
    if candidate
        .file_name()
        .is_none_or(|file| file != candidate.as_os_str())
    {
        return Err(ThemeError(format!("Invalid theme name: {name:?}")));
    }
    if let Some(paths) = registered {
        return paths
            .iter()
            .filter_map(|path| read_palette(path, mode).ok())
            .find(|palette| palette.name == name)
            .ok_or_else(|| ThemeError(format!("Theme not found: {name}")));
    }
    let path = directory.join(candidate);
    match fs::metadata(&path) {
        Ok(_) => return read_palette(&path, mode),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ThemeError(format!(
                "Reading theme {}: {err}",
                path.display()
            )));
        }
    }
    // Themes are listed by document name, which need not match the filename.
    theme_files(directory)
        .iter()
        .filter_map(|path| read_palette(path, mode).ok())
        .find(|palette| palette.name == name)
        .ok_or_else(|| ThemeError(format!("Theme not found: {name}")))
}

struct State {
    terminal: TerminalState,
    setting: String,
    active: String,
    palette: Palette,
    revision: u64,
    registered: Option<Vec<PathBuf>>,
}

impl State {
    fn appearance(&self) -> TerminalColorScheme {
        self.palette
            .appearance
            .unwrap_or_else(|| self.terminal.appearance())
    }

    fn default_color(&self, background: bool) -> Color {
        let reported = if background {
            self.terminal.colors.background
        } else {
            self.terminal.colors.foreground
        };
        let rgb = reported.unwrap_or_else(|| match (self.appearance(), background) {
            (TerminalColorScheme::Dark, true) | (TerminalColorScheme::Light, false) => RgbColor {
                r: 0.0,
                g: 0.0,
                b: 0.0,
            },
            (TerminalColorScheme::Dark, false) => RgbColor {
                r: 229.0,
                g: 229.0,
                b: 231.0,
            },
            (TerminalColorScheme::Light, true) => RgbColor {
                r: 255.0,
                g: 255.0,
                b: 255.0,
            },
        });
        Color::Rgb(rgb)
    }

    fn set_palette(&mut self, palette: Palette) -> bool {
        if self.palette == palette {
            return false;
        }
        self.palette = palette;
        self.revision = next_revision();
        true
    }
}

struct Shared {
    directory: PathBuf,
    mode: TerminalColorMode,
    state: RwLock<State>,
}

/// A session-local, cloneable handle; style callbacks see the current palette.
#[derive(Clone)]
pub struct Theme(Arc<Shared>);

impl Theme {
    pub fn load(agent_dir: &Path, setting: Option<&str>) -> Result<Self, ThemeError> {
        Self::load_with_mode(
            agent_dir,
            setting,
            get_terminal_color_mode(get_capabilities()),
        )
    }

    fn load_with_mode(
        agent_dir: &Path,
        setting: Option<&str>,
        mode: TerminalColorMode,
    ) -> Result<Self, ThemeError> {
        let directory = agent_dir.join("themes");
        let terminal = TerminalState {
            colors: TerminalColors::default(),
            scheme: None,
            env_scheme: std::env::var("COLORFGBG")
                .ok()
                .as_deref()
                .and_then(env_scheme),
            pending: true,
            osc_palette: [None; 16],
        };
        let setting = setting.unwrap_or(SYSTEM);
        let active = setting_name(setting, terminal.appearance())?;
        let palette = load_palette(&directory, None, active, &terminal, mode)?;
        Ok(Self(Arc::new(Shared {
            directory,
            mode,
            state: RwLock::new(State {
                terminal,
                setting: setting.into(),
                active: active.into(),
                palette,
                revision: next_revision(),
                registered: None,
            }),
        })))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.0
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.0
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn name(&self) -> String {
        self.read().palette.name.clone()
    }

    /// The appearance this palette was designed for, or the terminal's current appearance.
    pub fn appearance(&self) -> TerminalColorScheme {
        self.read().appearance()
    }

    /// The concrete native color, including a terminal default for empty tokens.
    pub fn color(&self, token: &str) -> Option<Color> {
        let state = self.read();
        let paint = state.palette.paint.get(token)?;
        let default = |background| state.default_color(background);
        let color = paint.color.unwrap_or_else(|| default(paint.background));
        if paint.dim {
            return Some(
                mix_colors(&color, &default(true), 0.4, ColorMixSpace::Oklch).unwrap_or(color),
            );
        }
        Some(color)
    }

    /// Changes when the effective palette changes, including across handles.
    pub fn revision(&self) -> u64 {
        self.read().revision
    }

    pub(crate) fn set_registered_paths(&self, paths: Vec<PathBuf>) {
        self.write().registered = Some(paths);
    }

    pub fn names(&self) -> Vec<String> {
        let mut names = BTreeSet::from([String::from("dark"), String::from("light")]);
        let state = self.read();
        let discovered;
        let paths = if let Some(paths) = &state.registered {
            paths
        } else {
            discovered = theme_files(&self.0.directory);
            &discovered
        };
        names.extend(paths.iter().filter_map(|path| {
            read_palette(path, self.0.mode)
                .ok()
                .map(|palette| palette.name)
        }));
        names.remove(SYSTEM);
        std::iter::once(SYSTEM.to_owned()).chain(names).collect()
    }

    /// An unsuccessful selection leaves the current theme unchanged.
    pub fn select(&self, setting: &str) -> Result<(), ThemeError> {
        let mut state = self.write();
        let active = setting_name(setting, state.terminal.appearance())?;
        let palette = load_palette(
            &self.0.directory,
            state.registered.as_deref(),
            active,
            &state.terminal,
            self.0.mode,
        )?;
        active.clone_into(&mut state.active);
        setting.clone_into(&mut state.setting);
        state.set_palette(palette);
        Ok(())
    }

    /// Refresh the selected file, retaining the last valid palette on failures.
    pub fn reload(&self) -> Result<(), ThemeError> {
        self.reapply(&mut self.write(), true).map(|_| ())
    }

    fn reapply(&self, state: &mut State, reload: bool) -> Result<bool, ThemeError> {
        let active = setting_name(&state.setting, state.terminal.appearance())?;
        if !reload && active == state.active && active != SYSTEM {
            return Ok(false);
        }
        if active == state.active
            && let Some(path) = &state.palette.source
        {
            let palette = read_palette(path, self.0.mode)?;
            return Ok(state.set_palette(palette));
        }
        let palette = load_palette(
            &self.0.directory,
            state.registered.as_deref(),
            active,
            &state.terminal,
            self.0.mode,
        )?;
        if state.active != active {
            active.clone_into(&mut state.active);
        }
        Ok(state.set_palette(palette))
    }

    /// Watch the theme directory for content and rename events. Keep the returned guard alive.
    /// The callback may run on a watcher thread; schedule `reload` on the UI loop.
    pub fn watch(
        &self,
        on_change: impl Fn() + Send + 'static,
    ) -> Result<RecommendedWatcher, ThemeError> {
        let fail = |error: &dyn Error| {
            ThemeError(format!(
                "Watching themes at {}: {error}",
                self.0.directory.display()
            ))
        };
        fs::create_dir_all(&self.0.directory).map_err(|err| fail(&err))?;
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            if let Ok(event) = event
                && (event.need_rescan()
                    || (!event.kind.is_access()
                        && (event.paths.is_empty()
                            || event
                                .paths
                                .iter()
                                .any(|path| path.extension().is_some_and(|ext| ext == "json")))))
            {
                on_change();
            }
        })
        .map_err(|err| fail(&err))?;
        watcher
            .watch(&self.0.directory, RecursiveMode::NonRecursive)
            .map_err(|err| fail(&err))?;
        if let Some(paths) = &self.read().registered {
            let directories: BTreeSet<_> = paths
                .iter()
                .filter_map(|path| path.parent())
                .filter(|path| *path != self.0.directory && path.is_dir())
                .collect();
            for directory in directories {
                watcher
                    .watch(directory, RecursiveMode::NonRecursive)
                    .map_err(|err| fail(&err))?;
            }
        }
        Ok(watcher)
    }

    /// Apply a completed color query (including an empty query on timeout).
    pub fn apply_terminal_colors(&self, colors: TerminalColors) -> Result<bool, ThemeError> {
        let mut state = self.write();
        let terminal = &mut state.terminal;
        let foreground = colors.foreground.or(terminal.colors.foreground);
        let background = colors.background.or(terminal.colors.background);
        let defaults_changed =
            foreground != terminal.colors.foreground || background != terminal.colors.background;
        let palette = colors.palette.filter(|palette| palette.len() == 16);
        if !terminal.pending
            && !defaults_changed
            && palette
                .as_ref()
                .is_none_or(|palette| Some(palette) == terminal.colors.palette.as_ref())
        {
            return Ok(false);
        }
        terminal.colors.foreground = foreground;
        terminal.colors.background = background;
        if let Some(palette) = palette {
            for (slot, &rgb) in terminal.osc_palette.iter_mut().zip(&palette) {
                *slot = Some(rgb);
            }
            terminal.colors.palette = Some(palette);
        }
        terminal.pending = false;
        let changed = self.reapply(&mut state, false)?;
        if defaults_changed && !changed {
            state.revision = next_revision();
        }
        Ok(changed || defaults_changed)
    }

    pub fn apply_terminal_scheme(&self, scheme: TerminalColorScheme) -> Result<bool, ThemeError> {
        let mut state = self.write();
        if state.terminal.scheme == Some(scheme) {
            return Ok(false);
        }
        let appearance = state.appearance();
        state.terminal.scheme = Some(scheme);
        if state.terminal.colors.background.is_some() {
            return Ok(false);
        }
        let changed = self.reapply(&mut state, false)?;
        if !changed && state.appearance() != appearance {
            state.revision = next_revision();
            return Ok(true);
        }
        Ok(changed)
    }

    /// Apply a raw OSC color response or terminal color-scheme report.
    pub fn apply_terminal_response(&self, response: &str) -> Result<bool, ThemeError> {
        if let Some(scheme) = parse_terminal_color_scheme_report(response) {
            return self.apply_terminal_scheme(scheme);
        }
        let Some(reply) = parse_osc_color_response(response) else {
            return Ok(false);
        };
        let Some(rgb) = reply.rgb else {
            return Ok(false);
        };
        let mut colors = TerminalColors::default();
        match reply.target {
            OscColorTarget::Foreground => colors.foreground = Some(rgb),
            OscColorTarget::Background => colors.background = Some(rgb),
            OscColorTarget::Index(index) => {
                let mut state = self.write();
                let index = usize::from(index);
                let terminal = &mut state.terminal;
                let Some(slot) = terminal.osc_palette.get_mut(index) else {
                    return Ok(false);
                };
                if *slot == Some(rgb) {
                    return Ok(false);
                }
                *slot = Some(rgb);
                if let Some(palette) = &mut terminal.colors.palette {
                    palette[index] = rgb;
                } else if terminal.osc_palette.iter().all(Option::is_some) {
                    terminal.colors.palette =
                        Some(terminal.osc_palette.into_iter().flatten().collect());
                } else {
                    return Ok(false);
                }
                state.terminal.pending = false;
                return self.reapply(&mut state, false);
            }
        }
        self.apply_terminal_colors(colors)
    }

    pub fn fg(&self, token: &str, text: &str) -> String {
        self.paint(token, text, false)
    }

    pub fn bg(&self, token: &str, text: &str) -> String {
        self.paint(token, text, true)
    }

    fn paint(&self, token: &str, text: &str, background: bool) -> String {
        let state = self.read();
        let Some(paint) = state
            .palette
            .paint
            .get(token)
            .filter(|paint| paint.background == background)
        else {
            return text.to_owned();
        };
        let mut out = String::with_capacity(paint.open.len() + text.len() + paint.close.len());
        paint.append(&mut out, text);
        out
    }

    pub fn highlight_code(&self, code: &str, language: Option<&str>) -> Vec<String> {
        syntax::highlight(&self.read().palette, code, language)
    }

    pub fn markdown(&self) -> MarkdownTheme {
        let fg = |token: &'static str| -> TextStyleFn {
            let theme = self.clone();
            Rc::new(move |text| theme.fg(token, text))
        };
        let theme = self.clone();
        MarkdownTheme {
            heading: fg("mdHeading"),
            link: fg("mdLink"),
            link_url: fg("mdLinkUrl"),
            code: fg("mdCode"),
            code_block: fg("mdCodeBlock"),
            code_block_border: fg("mdCodeBlockBorder"),
            quote: fg("mdQuote"),
            quote_border: fg("mdQuoteBorder"),
            hr: fg("mdHr"),
            list_bullet: fg("mdListBullet"),
            highlight_code: Some(Rc::new(move |code, language| {
                theme.highlight_code(code, language)
            })),
            ..MarkdownTheme::default()
        }
    }
}

#[cfg(test)]
mod tests;
