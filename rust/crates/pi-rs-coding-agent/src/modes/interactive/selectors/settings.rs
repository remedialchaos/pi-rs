use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use pi_rs_ai::{Model, ThinkingLevel};
use pi_rs_tui::components::settings_list::{SubmenuDone, SubmenuResult};
use pi_rs_tui::{
    Component, ComponentRef, Focusable, SettingItem, SettingsList, SettingsListOptions,
    SettingsListTheme, get_capabilities,
};
use serde_json::{Map, Value, json};

use super::{Picker, PickerItem, append_text, model_items, theme_items, thinking_items};
use crate::{core::settings_manager::SettingsManager, modes::interactive::theme::Theme};

#[derive(Debug, Clone, PartialEq)]
pub struct SettingChange {
    pub path: Vec<String>,
    pub value: Value,
}

#[derive(Clone, Copy)]
enum Values {
    Strings(&'static [&'static str]),
    Bools,
    Numbers(&'static [i64]),
    Quiet,
    Timeout,
    Trust,
    Wheel,
}

struct Field {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    path: &'static [&'static str],
    values: Values,
}

macro_rules! field {
    ($id:literal, $label:literal, $desc:literal, [$($path:literal),+], $values:expr) => {
        Field { id: $id, label: $label, description: $desc, path: &[$($path),+], values: $values }
    };
}

const FIELDS: &[Field] = &[
    field!(
        "autocompact",
        "Auto-compact",
        "Automatically compact context when it gets too large",
        ["compaction", "enabled"],
        Values::Bools
    ),
    field!(
        "show-images",
        "Show images",
        "Render images inline in terminal",
        ["terminal", "showImages"],
        Values::Bools
    ),
    field!(
        "image-width-cells",
        "Image width",
        "Preferred inline image width in terminal cells",
        ["terminal", "imageWidthCells"],
        Values::Numbers(&[60, 80, 120])
    ),
    field!(
        "auto-resize-images",
        "Auto-resize images",
        "Resize large images to 2000x2000 max for model compatibility",
        ["images", "autoResize"],
        Values::Bools
    ),
    field!(
        "block-images",
        "Block images",
        "Prevent images from being sent to LLM providers",
        ["images", "blockImages"],
        Values::Bools
    ),
    field!(
        "skill-commands",
        "Skill commands",
        "Register skills as /skill:name commands",
        ["enableSkillCommands"],
        Values::Bools
    ),
    field!(
        "show-hardware-cursor",
        "Show hardware cursor",
        "Use the terminal cursor instead of the drawn cursor",
        ["showHardwareCursor"],
        Values::Bools
    ),
    field!(
        "editor-padding",
        "Editor padding",
        "Horizontal padding for input editor (0–3)",
        ["editorPaddingX"],
        Values::Numbers(&[0, 1, 2, 3])
    ),
    field!(
        "output-padding",
        "Output padding",
        "Horizontal padding for messages, tool output, and command output",
        ["outputPad"],
        Values::Numbers(&[0, 1])
    ),
    field!(
        "autocomplete-max-visible",
        "Autocomplete max items",
        "Max visible items in autocomplete dropdown",
        ["autocompleteMaxVisible"],
        Values::Numbers(&[3, 5, 7, 10, 15, 20])
    ),
    field!(
        "clear-on-shrink",
        "Clear on shrink",
        "Clear empty rows when content shrinks (may cause flicker)",
        ["terminal", "clearOnShrink"],
        Values::Bools
    ),
    field!(
        "terminal-progress",
        "Terminal progress",
        "Show OSC 9;4 progress indicators in the terminal tab bar",
        ["terminal", "showTerminalProgress"],
        Values::Bools
    ),
    field!(
        "steering-mode",
        "Steering mode",
        "Enter while streaming queues steering messages; choose their delivery order",
        ["steeringMode"],
        Values::Strings(&["one-at-a-time", "all"])
    ),
    field!(
        "follow-up-mode",
        "Follow-up mode",
        "Choose how queued follow-up messages are delivered when the agent stops",
        ["followUpMode"],
        Values::Strings(&["one-at-a-time", "all"])
    ),
    field!(
        "transport",
        "Transport",
        "Preferred transport for providers that support multiple transports",
        ["transport"],
        Values::Strings(&["sse", "websocket", "websocket-cached", "auto"])
    ),
    field!(
        "share-destination",
        "Session share destination",
        "Where to upload sessions with /share",
        ["sessionShareDestination"],
        Values::Strings(&["auto", "gist", "radius"])
    ),
    field!(
        "http-idle-timeout",
        "HTTP idle timeout",
        "Maximum idle gap while waiting for HTTP headers or body chunks",
        ["httpIdleTimeoutMs"],
        Values::Timeout
    ),
    field!(
        "cache-warming-mode",
        "Cache warming",
        "Off, while the agent runs, or also between profitable runs",
        ["cacheWarming"],
        Values::Strings(&["off", "streaming", "idle"])
    ),
    field!(
        "hide-thinking",
        "Hide thinking",
        "Hide thinking blocks in assistant responses",
        ["hideThinkingBlock"],
        Values::Bools
    ),
    field!(
        "mermaid-rendering",
        "Mermaid diagrams",
        "Render Mermaid code blocks as Unicode diagrams",
        ["markdown", "mermaid"],
        Values::Strings(&["off", "final", "streaming"])
    ),
    field!(
        "cache-miss-notices",
        "Cache miss notices",
        "Show notices for cache costs and provider recovery diagnostics",
        ["showCacheMissNotices"],
        Values::Bools
    ),
    field!(
        "collapse-changelog",
        "Collapse changelog",
        "Show condensed changelog after updates",
        ["collapseChangelog"],
        Values::Bools
    ),
    field!(
        "quiet-startup",
        "Quiet startup",
        "Disable verbose printing at startup (header: keep the startup header)",
        ["quietStartup"],
        Values::Quiet
    ),
    field!(
        "install-telemetry",
        "Install telemetry",
        "Send an anonymous version/update ping after changelog-detected updates",
        ["enableInstallTelemetry"],
        Values::Bools
    ),
    field!(
        "default-project-trust",
        "Default project trust",
        "Fallback when no extension or saved decision decides trust",
        ["defaultProjectTrust"],
        Values::Trust
    ),
    field!(
        "double-escape-action",
        "Double-escape action",
        "Action when pressing Escape twice with empty editor",
        ["doubleEscapeAction"],
        Values::Strings(&["tree", "fork", "none"])
    ),
    field!(
        "tree-filter-mode",
        "Tree filter mode",
        "Default filter when opening /tree",
        ["treeFilterMode"],
        Values::Strings(&["default", "no-tools", "user-only", "labeled-only", "all"])
    ),
    field!(
        "tui-mode",
        "TUI mode",
        "Interface layout; regular mode keeps the terminal's normal scrollback",
        ["tuiMode"],
        Values::Strings(&["regular", "fullscreen"])
    ),
    field!(
        "fullscreen-exit-output",
        "Fullscreen exit output",
        "Print the transcript or only a session resume hint on exit",
        ["fullscreenExitOutput"],
        Values::Strings(&["transcript", "resume-hint"])
    ),
    field!(
        "fullscreen-scrollbar",
        "Fullscreen scrollbar",
        "Scrollbar behavior in fullscreen mode",
        ["fullscreenScrollbar"],
        Values::Strings(&["auto", "always", "hidden"])
    ),
    field!(
        "fullscreen-copy-on-select",
        "Fullscreen copy on select",
        "Automatically copy selected text in fullscreen mode",
        ["fullscreenCopyOnSelect"],
        Values::Bools
    ),
    field!(
        "fullscreen-wheel-scroll-lines",
        "Fullscreen wheel scrolling",
        "Lines per mouse-wheel event (auto accelerates fast spins)",
        ["fullscreenWheelScrollLines"],
        Values::Wheel
    ),
];

const TIMEOUTS: [(&str, i64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];
const TRUST: [(&str, &str); 3] = [
    ("Ask", "ask"),
    ("Always trust", "always"),
    ("Never trust", "never"),
];

impl Field {
    fn choices(&self, current: &Value) -> Vec<(String, Value)> {
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| ((*value).to_owned(), json!(value)))
                .collect()
        };
        let boolean = |value: bool| (value.to_string(), json!(value));
        let number = |value: i64| (value.to_string(), json!(value));
        match self.values {
            Values::Strings(values) => strings(values),
            Values::Bools => vec![boolean(true), boolean(false)],
            Values::Numbers(values) => values.iter().copied().map(number).collect(),
            Values::Quiet => vec![
                boolean(true),
                ("header".into(), json!("header")),
                boolean(false),
            ],
            Values::Timeout => TIMEOUTS
                .iter()
                .map(|(label, value)| ((*label).into(), json!(value)))
                .collect(),
            Values::Trust => TRUST
                .iter()
                .map(|(label, value)| ((*label).into(), json!(value)))
                .collect(),
            Values::Wheel => {
                let mut lines = vec![1, 2, 3, 5, 10];
                if let Some(current) = current.as_i64() {
                    lines.push(current);
                }
                lines.sort_unstable();
                lines.dedup();
                std::iter::once(("auto".into(), json!("auto")))
                    .chain(lines.into_iter().map(number))
                    .collect()
            }
        }
    }

    fn current(&self, settings: &Value) -> Value {
        let mut value = self
            .path
            .iter()
            .fold(settings, |value, key| &value[*key])
            .clone();
        if value.is_null() && matches!(self.values, Values::Bools) {
            value = json!(false);
        }
        if matches!(self.values, Values::Timeout)
            && let Some(text) = value.as_str()
        {
            if text.eq_ignore_ascii_case("disabled") {
                value = json!(0);
            } else if let Ok(parsed) = text.trim().parse::<i64>() {
                value = json!(parsed);
            }
        }
        value
    }

    fn display(&self, current: &Value, choices: &[(String, Value)]) -> String {
        if let Some((label, _)) = choices.iter().find(|(_, value)| value == current) {
            return label.clone();
        }
        if matches!(self.values, Values::Timeout)
            && let Some(ms) = current.as_i64()
        {
            return format!("{} sec", ms as f64 / 1000.0);
        }
        current
            .as_str()
            .map_or_else(|| current.to_string(), str::to_owned)
    }
}

pub fn settings_items(
    settings: &SettingsManager,
    supports_images: bool,
) -> Vec<PickerItem<Vec<String>>> {
    let settings = settings.get();
    FIELDS
        .iter()
        .filter(|field| supports_images || !matches!(field.id, "show-images" | "image-width-cells"))
        .map(|field| {
            let value = field.current(&settings);
            let choices = field.choices(&value);
            PickerItem {
                label: format!("{}: {}", field.label, field.display(&value, &choices)),
                search: format!("{} {}", field.label, field.description),
                value: field.path.iter().map(|key| (*key).to_owned()).collect(),
            }
        })
        .collect()
}

pub fn settings_submenu<T: PartialEq + 'static>(
    title: String,
    description: &str,
    options: Vec<PickerItem<T>>,
    current: Option<&T>,
    searchable: bool,
    theme: Theme,
    select: impl FnMut(Option<&T>) + 'static,
) -> Picker {
    let index = current
        .and_then(|current| options.iter().position(|item| &item.value == current))
        .unwrap_or_default();
    let picker = Picker::with_items(title, options, theme, select)
        .preselect(index)
        .description(description);
    if searchable {
        picker
    } else {
        picker.without_filter()
    }
}

fn list_theme(theme: &Theme) -> SettingsListTheme {
    let labels = theme.clone();
    let values = theme.clone();
    let descriptions = theme.clone();
    let hints = theme.clone();
    SettingsListTheme {
        label: Rc::new(move |text, selected| {
            labels.fg(if selected { "accent" } else { "text" }, text)
        }),
        value: Rc::new(move |text, selected| {
            values.fg(if selected { "accent" } else { "muted" }, text)
        }),
        description: Rc::new(move |text| descriptions.fg("muted", text)),
        cursor: "› ".into(),
        hint: Rc::new(move |text| hints.fg("dim", text)),
    }
}

type Change = Rc<RefCell<dyn FnMut(Option<SettingChange>)>>;
type Preview = Rc<RefCell<dyn FnMut(&str)>>;
type Done = Rc<RefCell<Option<SubmenuDone>>>;

pub struct SettingsPicker {
    list: SettingsList,
    theme: Theme,
    settings: Value,
    change: Change,
    preview: Preview,
    models: Rc<Vec<Model>>,
    current: Option<(String, String)>,
}

impl SettingsPicker {
    pub fn new(
        settings: &SettingsManager,
        theme: Theme,
        change: impl FnMut(Option<SettingChange>) + 'static,
    ) -> Self {
        let preview_theme = theme.clone();
        let mut dialog = Self {
            list: SettingsList::new(
                Vec::new(),
                10,
                SettingsListTheme::default(),
                Box::new(|_, _| {}),
                Box::new(|| {}),
                SettingsListOptions::default(),
            ),
            theme,
            settings: settings.get(),
            change: Rc::new(RefCell::new(change)),
            preview: Rc::new(RefCell::new(move |setting: &str| {
                let _ = preview_theme.select(setting);
            })),
            models: Rc::new(Vec::new()),
            current: None,
        };
        dialog.rebuild();
        dialog
    }

    pub fn with_models(mut self, models: Vec<Model>, current: Option<(&str, &str)>) -> Self {
        self.models = Rc::new(models);
        self.current = current.map(|(provider, id)| (provider.into(), id.into()));
        self.rebuild();
        self
    }

    pub fn with_theme_preview(mut self, preview: impl FnMut(&str) + 'static) -> Self {
        self.preview = Rc::new(RefCell::new(preview));
        self.rebuild();
        self
    }

    fn rebuild(&mut self) {
        let supports_images = get_capabilities().images.is_some();
        let mut items = Vec::new();
        let mut mapping = Vec::new();
        for field in FIELDS {
            if !supports_images && matches!(field.id, "show-images" | "image-width-cells") {
                continue;
            }
            if field.id == "tui-mode" {
                items.push(self.warnings());
                items.push(self.model_thinking());
            }
            let current = field.current(&self.settings);
            let choices = field.choices(&current);
            items.push(SettingItem {
                id: field.id.into(),
                label: field.label.into(),
                description: Some(field.description.into()),
                current_value: field.display(&current, &choices),
                values: choices.iter().map(|(label, _)| label.clone()).collect(),
                submenu: None,
            });
            mapping.push((field, choices));
        }
        items.push(self.theme_setting());
        let change = Rc::clone(&self.change);
        let cancel = Rc::clone(&self.change);
        self.list = SettingsList::new(
            items,
            10,
            list_theme(&self.theme),
            Box::new(move |id, value| {
                if id == "theme" {
                    change.borrow_mut()(Some(SettingChange {
                        path: vec!["theme".into()],
                        value: json!(value),
                    }));
                } else if let Some((field, choices)) =
                    mapping.iter().find(|(field, _)| field.id == id)
                    && let Some((_, value)) = choices.iter().find(|(label, _)| label == value)
                {
                    change.borrow_mut()(Some(SettingChange {
                        path: field.path.iter().map(|key| (*key).to_owned()).collect(),
                        value: value.clone(),
                    }));
                }
            }),
            Box::new(move || cancel.borrow_mut()(None)),
            SettingsListOptions {
                enable_search: true,
            },
        );
    }

    fn warnings(&self) -> SettingItem {
        let theme = self.theme.clone();
        let change = Rc::clone(&self.change);
        let current = Rc::new(RefCell::new(
            self.settings["warnings"]["anthropicExtraUsage"]
                .as_bool()
                .unwrap_or(true),
        ));
        SettingItem {
            id: "warnings".into(),
            label: "Warnings".into(),
            description: Some("Enable or disable individual warnings".into()),
            current_value: "configure".into(),
            values: Vec::new(),
            submenu: Some(Box::new(move |_, done| {
                let current = Rc::clone(&current);
                let item = SettingItem {
                    id: "anthropic-extra-usage".into(),
                    label: "Anthropic extra usage".into(),
                    description: Some(
                        "Warn when subscription auth may use paid extra usage".into(),
                    ),
                    current_value: current.borrow().to_string(),
                    values: vec!["true".into(), "false".into()],
                    submenu: None,
                };
                let change = Rc::clone(&change);
                let done = Rc::new(RefCell::new(Some(done)));
                ComponentRef::new(SettingsList::new(
                    vec![item],
                    10,
                    list_theme(&theme),
                    Box::new(move |_, value| {
                        let enabled = value == "true";
                        *current.borrow_mut() = enabled;
                        change.borrow_mut()(Some(SettingChange {
                            path: vec!["warnings".into(), "anthropicExtraUsage".into()],
                            value: json!(enabled),
                        }));
                    }),
                    Box::new(move || finish_menu(&done, None)),
                    SettingsListOptions::default(),
                ))
            })),
        }
    }

    fn model_thinking(&self) -> SettingItem {
        let levels = Rc::new(RefCell::new(
            self.settings["modelThinkingLevels"]
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ));
        let summary = model_summary(&levels.borrow());
        let models = Rc::clone(&self.models);
        let theme = self.theme.clone();
        let change = Rc::clone(&self.change);
        let current = self.current.clone();
        SettingItem {
            id: "model-thinking".into(),
            label: "Default thinking level per model".into(),
            description: Some("Override reasoning for specific models".into()),
            current_value: summary,
            values: Vec::new(),
            submenu: Some(Box::new(move |_, done| {
                ComponentRef::new(ModelThinkingPicker::new(
                    Rc::clone(&models),
                    theme.clone(),
                    Rc::clone(&levels),
                    Rc::clone(&change),
                    current.as_ref(),
                    done,
                ))
            })),
        }
    }

    fn theme_setting(&self) -> SettingItem {
        let theme = self.theme.clone();
        let preview = Rc::clone(&self.preview);
        SettingItem {
            id: "theme".into(),
            label: "Theme".into(),
            description: Some("Color theme for the interface".into()),
            current_value: self.settings["theme"].as_str().unwrap_or("system").into(),
            values: Vec::new(),
            submenu: Some(Box::new(move |current, done| {
                ComponentRef::new(ThemeSettingsPicker::new(
                    theme.clone(),
                    Rc::clone(&preview),
                    current,
                    done,
                ))
            })),
        }
    }
}

impl Component for SettingsPicker {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![self.theme.fg("border", &"─".repeat(width))];
        append_text(&mut lines, width, &self.theme, "accent", "Settings");
        lines.extend(self.list.render(width));
        lines.push(self.theme.fg("border", &"─".repeat(width)));
        lines
    }
    fn handle_input(&mut self, data: &str) {
        self.list.handle_input(data);
    }
    fn tick(&mut self, now: Instant) -> bool {
        self.list.tick(now)
    }
    fn invalidate(&mut self) {
        self.list.invalidate();
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
    fn children(&self) -> &[ComponentRef] {
        self.list.children()
    }
}

fn model_summary(levels: &Map<String, Value>) -> String {
    if levels.is_empty() {
        "none".into()
    } else {
        format!("{} configured", levels.len())
    }
}

fn finish_menu(done: &Done, value: Option<String>) {
    if let Some(done) = done.borrow_mut().take() {
        done(SubmenuResult {
            value,
            navigate_to: None,
        });
    }
}

type ModelLevelMap = Rc<RefCell<Map<String, Value>>>;

struct ModelThinkingPicker {
    picker: Picker,
    models: Rc<Vec<Model>>,
    levels: ModelLevelMap,
    change: Change,
    done: Done,
    theme: Theme,
    model: Option<usize>,
    result: Rc<RefCell<Option<Option<usize>>>>,
    choices: Vec<Option<ThinkingLevel>>,
    preferred: Option<usize>,
}

impl ModelThinkingPicker {
    fn new(
        models: Rc<Vec<Model>>,
        theme: Theme,
        levels: ModelLevelMap,
        change: Change,
        current: Option<&(String, String)>,
        done: SubmenuDone,
    ) -> Self {
        let preferred = current.and_then(|(provider, id)| {
            models
                .iter()
                .position(|model| &model.provider == provider && &model.id == id)
        });
        let mut dialog = Self {
            picker: Picker::new(String::new(), Vec::new(), theme.clone(), |_| {}),
            models,
            levels,
            change,
            done: Rc::new(RefCell::new(Some(done))),
            theme,
            model: None,
            result: Rc::new(RefCell::new(None)),
            choices: Vec::new(),
            preferred,
        };
        dialog.model_step();
        dialog
    }

    fn model_step(&mut self) {
        let current = self.preferred.and_then(|index| self.models.get(index));
        let levels = self.levels.borrow();
        let mut items = model_items(&self.models, current, None);
        for item in &mut items {
            let model = &self.models[item.value];
            if let Some(level) = levels
                .get(&format!("{}/{}", model.provider, model.id))
                .and_then(Value::as_str)
            {
                item.label.push_str(&format!(" · {level}"));
            }
        }
        let selection = Rc::clone(&self.result);
        self.model = None;
        self.picker = settings_submenu(
            "Per-Model Thinking Level".into(),
            "Step 1/2 · Select a model to configure",
            items,
            self.preferred.as_ref(),
            true,
            self.theme.clone(),
            move |index| *selection.borrow_mut() = Some(index.copied()),
        );
        self.picker.set_focused(true);
    }

    fn level_step(&mut self, index: usize) {
        let Some(model) = self.models.get(index) else {
            return;
        };
        self.model = Some(index);
        self.preferred = Some(index);
        let key = format!("{}/{}", model.provider, model.id);
        let current = self
            .levels
            .borrow()
            .get(&key)
            .and_then(Value::as_str)
            .and_then(|level| {
                ThinkingLevel::ALL
                    .into_iter()
                    .find(|candidate| candidate.as_str() == level)
            });
        let levels: Vec<_> = if model.reasoning {
            model.supported_thinking_levels().collect()
        } else {
            vec![ThinkingLevel::Off]
        };
        let mut labels: Vec<_> =
            thinking_items(&levels, current.unwrap_or(ThinkingLevel::Off), None)
                .into_iter()
                .map(|item| item.label)
                .collect();
        self.choices = levels.into_iter().map(Some).collect();
        if current.is_some() {
            labels.push("(clear override) · Revert to global default".into());
            self.choices.push(None);
        }
        let preselect = self
            .choices
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or_default();
        let result = Rc::clone(&self.result);
        self.picker = Picker::new(
            format!("Thinking Level for {} [{}]", model.id, model.provider),
            labels,
            self.theme.clone(),
            move |index| *result.borrow_mut() = Some(index),
        )
        .description("Step 2/2 · Select default thinking level for this model")
        .without_filter()
        .preselect(preselect);
    }
}

impl Component for ModelThinkingPicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }
    fn handle_input(&mut self, data: &str) {
        self.picker.handle_input(data);
        let Some(choice) = self.result.borrow_mut().take() else {
            return;
        };
        match (self.model, choice) {
            (None, Some(index)) => self.level_step(index),
            (None, None) => finish_menu(&self.done, Some(model_summary(&self.levels.borrow()))),
            (Some(index), Some(choice)) => {
                let model = &self.models[index];
                let key = format!("{}/{}", model.provider, model.id);
                let mut levels = self.levels.borrow_mut();
                match self.choices.get(choice) {
                    Some(Some(level)) => {
                        levels.insert(key, json!(level.as_str()));
                    }
                    Some(None) => {
                        levels.remove(&key);
                    }
                    None => return,
                }
                self.change.borrow_mut()(Some(SettingChange {
                    path: vec!["modelThinkingLevels".into()],
                    value: Value::Object(levels.clone()),
                }));
                drop(levels);
                self.model_step();
            }
            (Some(_), None) => self.model_step(),
        }
    }
}

#[derive(Clone, Copy)]
enum ThemeStep {
    Single,
    Automatic,
    Light,
    Dark,
}

struct ThemeSettingsPicker {
    picker: Picker,
    theme: Theme,
    preview: Preview,
    done: Done,
    result: Rc<RefCell<Option<Option<usize>>>>,
    original: String,
    single: String,
    light: String,
    dark: String,
    names: Vec<String>,
    values: Vec<String>,
    step: ThemeStep,
}

impl ThemeSettingsPicker {
    fn new(theme: Theme, preview: Preview, original: &str, done: SubmenuDone) -> Self {
        let names = theme.names();
        let fixed = if names.iter().any(|name| name == original) {
            original.to_owned()
        } else {
            theme.name()
        };
        let (light, dark) = original
            .split_once('/')
            .filter(|(light, dark)| !light.is_empty() && !dark.is_empty())
            .map_or_else(
                || (fixed.clone(), fixed.clone()),
                |(light, dark)| (light.to_owned(), dark.to_owned()),
            );
        let step = if original.contains('/') {
            ThemeStep::Automatic
        } else {
            ThemeStep::Single
        };
        let mut dialog = Self {
            picker: Picker::new(String::new(), Vec::new(), theme.clone(), |_| {}),
            theme,
            preview,
            done: Rc::new(RefCell::new(Some(done))),
            result: Rc::new(RefCell::new(None)),
            original: original.into(),
            single: fixed,
            light,
            dark,
            names,
            values: Vec::new(),
            step,
        };
        dialog.show(step);
        dialog
    }

    fn automatic(&self) -> String {
        format!("{}/{}", self.light, self.dark)
    }

    fn show(&mut self, step: ThemeStep) {
        self.step = step;
        let (title, desc, items, index) = match step {
            ThemeStep::Automatic => (
                "Automatic Theme",
                "Choose themes for light and dark terminal appearance",
                vec![
                    PickerItem::new(format!("Light theme: {}", self.light), "light".into()),
                    PickerItem::new(format!("Dark theme: {}", self.dark), "dark".into()),
                    PickerItem::new("Apply and go back".into(), "apply".into()),
                    PickerItem::new("Switch to a single theme".into(), "single".into()),
                ],
                0,
            ),
            _ => {
                let (title, desc, current) = match step {
                    ThemeStep::Single => (
                        "Theme",
                        "Select a theme, or choose automatic to follow terminal appearance",
                        &self.single,
                    ),
                    ThemeStep::Light => (
                        "Light Theme",
                        "Select the theme for light terminal appearance",
                        &self.light,
                    ),
                    ThemeStep::Dark => (
                        "Dark Theme",
                        "Select the theme for dark terminal appearance",
                        &self.dark,
                    ),
                    ThemeStep::Automatic => ("Theme", "Select a theme", &self.single),
                };
                let mut items = theme_items(&self.names, current);
                if matches!(step, ThemeStep::Single) {
                    let index =
                        usize::from(items.first().is_some_and(|item| item.value == "system"));
                    items.insert(
                        index,
                        PickerItem::new(
                            "automatic · separate light and dark themes".into(),
                            "/".into(),
                        ),
                    );
                }
                let index = items
                    .iter()
                    .position(|item| &item.value == current)
                    .unwrap_or_default();
                (title, desc, items, index)
            }
        };
        let (labels, values): (Vec<_>, Vec<_>) = items
            .into_iter()
            .map(|item| (item.label, item.value))
            .unzip();
        self.values = values;
        let result = Rc::clone(&self.result);
        let mut picker = Picker::new(title.into(), labels, self.theme.clone(), move |index| {
            *result.borrow_mut() = Some(index)
        })
        .description(desc)
        .without_filter()
        .preselect(index);
        if !matches!(step, ThemeStep::Automatic) {
            let values = self.values.clone();
            let preview = Rc::clone(&self.preview);
            let automatic = self.automatic();
            picker = picker.on_highlight(move |index| {
                if let Some(value) = values.get(index) {
                    preview.borrow_mut()(if value == "/" { &automatic } else { value });
                }
            });
        }
        self.picker = picker;
    }
}

impl Component for ThemeSettingsPicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }
    fn handle_input(&mut self, data: &str) {
        self.picker.handle_input(data);
        let Some(result) = self.result.borrow_mut().take() else {
            return;
        };
        let selected = result.and_then(|index| self.values.get(index)).cloned();
        match (self.step, selected.as_deref()) {
            (ThemeStep::Single, Some("/")) => {
                self.preview.borrow_mut()(&self.automatic());
                self.show(ThemeStep::Automatic);
            }
            (ThemeStep::Single, Some(value)) => {
                self.preview.borrow_mut()(value);
                finish_menu(&self.done, selected);
            }
            (ThemeStep::Automatic, Some("light")) => self.show(ThemeStep::Light),
            (ThemeStep::Automatic, Some("dark")) => self.show(ThemeStep::Dark),
            (ThemeStep::Automatic, Some("single")) => {
                self.single = self.theme.name();
                self.preview.borrow_mut()(&self.single);
                self.show(ThemeStep::Single);
            }
            (ThemeStep::Automatic, Some("apply")) => {
                finish_menu(&self.done, Some(self.automatic()))
            }
            (ThemeStep::Light | ThemeStep::Dark, value) => {
                if let Some(value) = value {
                    match self.step {
                        ThemeStep::Light => self.light = value.into(),
                        ThemeStep::Dark => self.dark = value.into(),
                        _ => {}
                    }
                }
                self.preview.borrow_mut()(&self.automatic());
                self.show(ThemeStep::Automatic);
            }
            _ => {
                self.preview.borrow_mut()(&self.original);
                finish_menu(&self.done, None);
            }
        }
    }
}
