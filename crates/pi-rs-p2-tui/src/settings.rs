use crate::keybindings::{KeyPlatform, PRIVILEGED_ACTIONS, builtin_keybindings};
use crate::preferences::TuiSettingField;
use crate::render_context::format_key_text;
use crate::theme::ThemeRef;
use indexmap::IndexMap;
use pi_rs_tui::{Component, Container, Focusable, KeyId, SettingItem, SettingsList, Text};
use std::boxed::Box;

type Value = serde_json::Value;

#[derive(Debug, Clone)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub description: String,
    pub path: Vec<String>,
    pub fallback: Value,
    pub values: Vec<Value>,
}

pub struct ThemePicker {
    ctx: Box<dyn crate::render_context::RenderContext>,
    themes: Vec<String>,
    selected: usize,
    preview_ref: ThemeRef,
    on_select: Box<dyn Fn(String) + Send>,
    on_cancel: Box<dyn Fn() + Send>,
    focused: bool,
}

impl ThemePicker {
    pub fn new(
        ctx: Box<dyn crate::render_context::RenderContext>,
        themes: Vec<String>,
        current: &str,
        on_select: impl Fn(String) + Send + 'static,
        on_cancel: impl Fn() + Send + 'static,
    ) -> Self {
        let selected = themes.iter().position(|t| t == current).unwrap_or(0);
        let preview_ref = ThemeRef::new(ctx.theme().current());
        Self {
            ctx,
            themes,
            selected,
            preview_ref,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            focused: false,
        }
    }

    fn sample(&self, _width: usize) -> Vec<String> {
        let theme = self.preview_ref.current();
        vec![
            theme.fg("accent", &theme.bold("Theme Preview")),
            String::new(),
            theme.fg("text", "This is normal text"),
            theme.fg("muted", "This is muted text"),
            theme.fg("dim", "This is dim text"),
            theme.fg("success", "Success message"),
            theme.fg("error", "Error message"),
            theme.fg("warning", "Warning message"),
            String::new(),
            theme.fg("mdHeading", "# Heading 1"),
            theme.fg("mdCode", "`inline code`"),
        ]
    }
}

impl Component for ThemePicker {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let mut lines = Vec::new();
        let preview_width = width.saturating_sub(2);
        let list_width = width.min(40);

        for (i, t) in self.themes.iter().enumerate() {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let style = if i == self.selected { "accent" } else { "text" };
            lines.push(format!(
                "{}{}{}",
                prefix,
                theme.fg(style, t),
                " ".repeat(list_width.saturating_sub(t.len() + 2))
            ));
        }

        let sample = self.sample(preview_width);
        for (i, line) in sample.iter().enumerate() {
            if i < lines.len() {
                lines[i] = format!("{}  {}", lines[i], line);
            } else {
                lines.push(format!("{}  {}", " ".repeat(list_width), line));
            }
        }

        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = pi_rs_tui::get_keybindings();
        if keys.matches(data, "tui.select.up") {
            if self.selected > 0 {
                self.selected -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.selected < self.themes.len().saturating_sub(1) {
                self.selected += 1;
            }
        } else if keys.matches(data, "tui.select.confirm") {
            (self.on_select)(self.themes[self.selected].clone());
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ThemePicker {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub fn open_settings(
    ctx: Box<dyn crate::render_context::RenderContext>,
    settings: &Value,
    plugin_fields: IndexMap<String, TuiSettingField>,
    mut change: impl FnMut(&[String], &Value) + 'static,
    cancel: impl FnMut() + 'static,
) -> SettingsList {
    let mut fields: Vec<Field> = vec![
        Field {
            id: "hide-thinking".into(),
            label: "Hide thinking".into(),
            description: "Hide thinking blocks in assistant responses".into(),
            path: vec!["hideThinkingBlock".into()],
            fallback: serde_json::json!(true),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "mermaid-rendering".into(),
            label: "Mermaid diagrams".into(),
            description: "Render Mermaid code blocks as diagrams".into(),
            path: vec!["markdown".into(), "mermaid".into()],
            fallback: serde_json::json!("streaming"),
            values: vec![
                serde_json::json!("off"),
                serde_json::json!("final"),
                serde_json::json!("streaming"),
            ],
        },
        Field {
            id: "quiet-startup".into(),
            label: "Quiet startup".into(),
            description: "Hide the startup header (or keep only its first line)".into(),
            path: vec!["quietStartup".into()],
            fallback: serde_json::json!(false),
            values: vec![
                serde_json::json!(true),
                serde_json::json!("header"),
                serde_json::json!(false),
            ],
        },
        Field {
            id: "show-hardware-cursor".into(),
            label: "Show hardware cursor".into(),
            description: "Show the terminal cursor while still positioning it for IME support"
                .into(),
            path: vec!["showHardwareCursor".into()],
            fallback: serde_json::json!(false),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "editor-padding".into(),
            label: "Editor padding".into(),
            description: "Horizontal padding for the input editor (0-3)".into(),
            path: vec!["editorPaddingX".into()],
            fallback: serde_json::json!(0),
            values: vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2),
                serde_json::json!(3),
            ],
        },
        Field {
            id: "output-padding".into(),
            label: "Output padding".into(),
            description: "Horizontal padding for transcript output".into(),
            path: vec!["outputPad".into()],
            fallback: serde_json::json!(1),
            values: vec![serde_json::json!(0), serde_json::json!(1)],
        },
        Field {
            id: "autocomplete-max-visible".into(),
            label: "Autocomplete max items".into(),
            description: "Maximum visible items in autocomplete".into(),
            path: vec!["autocompleteMaxVisible".into()],
            fallback: serde_json::json!(5),
            values: vec![
                serde_json::json!(3),
                serde_json::json!(5),
                serde_json::json!(7),
                serde_json::json!(10),
                serde_json::json!(15),
                serde_json::json!(20),
            ],
        },
        Field {
            id: "clear-on-shrink".into(),
            label: "Clear on shrink".into(),
            description: "Clear empty rows when content shrinks".into(),
            path: vec!["terminal".into(), "clearOnShrink".into()],
            fallback: serde_json::json!(false),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "terminal-progress".into(),
            label: "Terminal progress".into(),
            description: "Show progress in the terminal tab while the agent works".into(),
            path: vec!["terminal".into(), "showTerminalProgress".into()],
            fallback: serde_json::json!(false),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "fullscreen-exit-output".into(),
            label: "Fullscreen exit output".into(),
            description: "What to print when leaving fullscreen".into(),
            path: vec!["fullscreenExitOutput".into()],
            fallback: serde_json::json!("transcript"),
            values: vec![
                serde_json::json!("transcript"),
                serde_json::json!("resume-hint"),
            ],
        },
        Field {
            id: "fullscreen-scrollbar".into(),
            label: "Fullscreen scrollbar".into(),
            description: "When to show the transcript scrollbar".into(),
            path: vec!["fullscreenScrollbar".into()],
            fallback: serde_json::json!("auto"),
            values: vec![
                serde_json::json!("auto"),
                serde_json::json!("always"),
                serde_json::json!("hidden"),
            ],
        },
        Field {
            id: "fullscreen-copy-on-select".into(),
            label: "Fullscreen copy on select".into(),
            description: "Copy a mouse selection when the button is released".into(),
            path: vec!["fullscreenCopyOnSelect".into()],
            fallback: serde_json::json!(true),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "fullscreen-wheel-scroll-lines".into(),
            label: "Fullscreen wheel scrolling".into(),
            description: "Lines per mouse-wheel event; 'auto' speeds up fast spins".into(),
            path: vec!["fullscreenWheelScrollLines".into()],
            fallback: serde_json::json!("auto"),
            values: vec![
                serde_json::json!("auto"),
                serde_json::json!(1),
                serde_json::json!(2),
                serde_json::json!(3),
                serde_json::json!(5),
                serde_json::json!(10),
            ],
        },
        Field {
            id: "show-images".into(),
            label: "Show images".into(),
            description: "Render images inline (when the launcher supports them)".into(),
            path: vec!["terminal".into(), "showImages".into()],
            fallback: serde_json::json!(true),
            values: vec![serde_json::json!(true), serde_json::json!(false)],
        },
        Field {
            id: "image-width-cells".into(),
            label: "Image width".into(),
            description: "Preferred inline image width in terminal cells".into(),
            path: vec!["terminal".into(), "imageWidthCells".into()],
            fallback: serde_json::json!(60),
            values: vec![
                serde_json::json!(60),
                serde_json::json!(80),
                serde_json::json!(120),
            ],
        },
    ];
    let mut plugins = plugin_fields;
    plugins.sort_keys();
    for (key, plugin) in plugins {
        if let (Some(name), Some(owner), Some(label), Some(values)) = (
            plugin["name"].as_str(),
            plugin["plugin"].as_str(),
            plugin["label"].as_str(),
            plugin["values"].as_array(),
        ) {
            fields.push(Field {
                id: format!("plugin:{key}"),
                label: label.into(),
                description: plugin["description"].as_str().unwrap_or_default().into(),
                path: vec!["plugins".into(), owner.into(), name.into()],
                fallback: plugin["fallback"].clone(),
                values: values.clone(),
            });
        }
    }
    let display = |v: &Value| {
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
    };
    let items = fields
        .iter()
        .map(|field| {
            let current = field
                .path
                .iter()
                .try_fold(settings, |v, k| v.get(k))
                .unwrap_or(&field.fallback);
            SettingItem {
                id: field.id.clone(),
                label: field.label.clone(),
                description: Some(field.description.clone()),
                current_value: display(current),
                values: field.values.iter().map(display).collect(),
                submenu: None,
            }
        })
        .collect();
    SettingsList::new(
        items,
        10,
        ctx.theme().settings_list(),
        Box::new(move |id, value| {
            if let Some(field) = fields.iter().find(|field| field.id == id)
                && let Some(v) = field.values.iter().find(|v| display(v) == value)
            {
                change(&field.path, v);
            }
        }),
        Box::new(cancel),
        Default::default(),
    )
}

pub struct KeyCapture {
    ctx: Box<dyn crate::render_context::RenderContext>,
    on_capture: Box<dyn Fn(KeyId) + Send>,
    on_cancel: Box<dyn Fn() + Send>,
    focused: bool,
}

impl KeyCapture {
    pub fn new(
        ctx: Box<dyn crate::render_context::RenderContext>,
        on_capture: impl Fn(KeyId) + Send + 'static,
        on_cancel: impl Fn() + Send + 'static,
    ) -> Self {
        Self {
            ctx,
            on_capture: Box::new(on_capture),
            on_cancel: Box::new(on_cancel),
            focused: false,
        }
    }
}

impl Component for KeyCapture {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        vec![
            theme.fg("accent", "Press a key combination..."),
            theme.fg("dim", "(Esc to cancel)"),
        ]
    }

    fn handle_input(&mut self, data: &str) {
        if data == "\x1b" {
            (self.on_cancel)();
            return;
        }
        if let Some(key) = pi_rs_tui::parse_key(data) {
            (self.on_capture)(key.id().into());
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for KeyCapture {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub fn open_keybindings(ctx: Box<dyn crate::render_context::RenderContext>) -> Container {
    let theme = ctx.theme().current();
    let keybindings = builtin_keybindings(KeyPlatform::default());
    let mut container = Container::default();

    for (id, def) in keybindings {
        let keys = def
            .default_keys
            .as_slice()
            .iter()
            .map(|k| format_key_text(k.as_str(), false))
            .collect::<Vec<_>>()
            .join(", ");
        let is_privileged = PRIVILEGED_ACTIONS.contains(&id);
        let style = if is_privileged { "warning" } else { "text" };
        container.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            format!("{}  {}", theme.fg(style, id), theme.fg("muted", &keys)),
            0,
            0,
            None,
        )));
    }
    container
}

type KeyRebindFn =
    std::sync::Arc<dyn Fn(&str, Option<crate::keybindings::KeyIdValue>) + Send + Sync>;

/// Interactive `/keybindings` overlay: navigate bindings, press Enter to rebind via `KeyCapture`,
/// Backspace to reset to default, rejecting launcher-owned privileged actions.
pub struct KeybindingsOverlay {
    ctx: Box<dyn crate::render_context::RenderContext>,
    entries: Vec<(String, String, Vec<String>, bool)>,
    user: IndexMap<String, crate::keybindings::KeyIdValue>,
    selected: usize,
    capture: Option<KeyCapture>,
    warning: Option<String>,
    on_change: KeyRebindFn,
    on_close: Box<dyn Fn() + Send>,
    focused: bool,
}

impl KeybindingsOverlay {
    pub fn new(
        ctx: Box<dyn crate::render_context::RenderContext>,
        user: IndexMap<String, crate::keybindings::KeyIdValue>,
        on_change: impl Fn(&str, Option<crate::keybindings::KeyIdValue>) + Send + Sync + 'static,
        on_close: impl Fn() + Send + 'static,
    ) -> Self {
        let builtins = builtin_keybindings(KeyPlatform::default());
        let entries = builtins
            .into_iter()
            .map(|(id, def)| {
                (
                    id.to_string(),
                    def.description.to_string(),
                    def.default_keys.as_slice().to_vec(),
                    PRIVILEGED_ACTIONS.contains(&id),
                )
            })
            .collect();
        Self {
            ctx,
            entries,
            user,
            selected: 0,
            capture: None,
            warning: None,
            on_change: std::sync::Arc::new(on_change),
            on_close: Box::new(on_close),
            focused: true,
        }
    }

    fn effective_keys(&self, id: &str, defaults: &[String]) -> String {
        let keys = self.user.get(id).map(|v| v.as_slice()).unwrap_or(defaults);
        if keys.is_empty() {
            "unbound".to_string()
        } else {
            keys.iter()
                .map(|k| format_key_text(k, false))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

impl Component for KeybindingsOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let border = theme.fg("border", &"─".repeat(width.max(1)));
        let mut lines = vec![
            border.clone(),
            pi_rs_tui::truncate_to_width(&theme.fg("accent", "Rebind keys"), width, true),
        ];
        if let Some(capture) = &self.capture {
            if let Some((id, desc, _, _)) = self.entries.get(self.selected) {
                lines.push(pi_rs_tui::truncate_to_width(
                    &format!("{} — {}", theme.bold(id), theme.fg("muted", desc)),
                    width,
                    true,
                ));
            }
            lines.extend(capture.render(width));
            lines.push(border);
            return lines;
        }
        let max_visible = 12usize;
        let start = self.selected.saturating_sub(max_visible / 2);
        let end = (start + max_visible).min(self.entries.len());
        for (idx, (id, _desc, defaults, is_priv)) in self
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
        {
            let prefix = if idx == self.selected { "→ " } else { "  " };
            let keys = self.effective_keys(id, defaults);
            let style = if *is_priv {
                "warning"
            } else if idx == self.selected {
                "accent"
            } else {
                "text"
            };
            let suffix = if *is_priv { " [launcher]" } else { "" };
            let row = format!(
                "{}{}  {}{}",
                prefix,
                theme.fg(style, id),
                theme.fg("muted", &keys),
                theme.fg("dim", suffix)
            );
            lines.push(pi_rs_tui::truncate_to_width(&row, width, true));
        }
        if let Some(warning) = &self.warning {
            lines.push(pi_rs_tui::truncate_to_width(
                &theme.fg("warning", warning),
                width,
                true,
            ));
        }
        lines.push(pi_rs_tui::truncate_to_width(
            &theme.fg("dim", "Enter rebind · Backspace reset · Esc close"),
            width,
            true,
        ));
        lines.push(border);
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(capture) = &mut self.capture {
            if data == "\x1b" {
                self.capture = None;
                return;
            }
            if let Some(key) = pi_rs_tui::parse_key(data) {
                let key_id: KeyId = key.id().into();
                if let Some((id, _, _, _)) = self.entries.get(self.selected).cloned() {
                    let val = crate::keybindings::KeyIdValue::One(key_id.clone());
                    self.user.insert(id.clone(), val.clone());
                    (self.on_change)(&id, Some(val));
                }
                self.capture = None;
                self.warning = None;
                return;
            }
            let _ = capture;
            return;
        }
        let keys = pi_rs_tui::get_keybindings();
        let len = self.entries.len();
        if keys.matches(data, "tui.select.up") {
            self.selected = if self.selected == 0 {
                len.saturating_sub(1)
            } else {
                self.selected - 1
            };
        } else if keys.matches(data, "tui.select.down") && len > 0 {
            self.selected = (self.selected + 1) % len;
        } else if keys.matches(data, "tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(12);
        } else if keys.matches(data, "tui.select.pageDown") {
            self.selected = (self.selected + 12).min(len.saturating_sub(1));
        } else if keys.matches(data, "tui.select.confirm") {
            if let Some((id, _, _, is_priv)) = self.entries.get(self.selected) {
                if *is_priv {
                    self.warning = Some(format!(
                        "{id} is launcher-owned; edit ~/.pi/p2-tui/prefs.json directly"
                    ));
                } else {
                    self.warning = None;
                    let dummy_ctx = Box::new(CloneThemeCtx(self.ctx.theme().clone()));
                    self.capture = Some(KeyCapture::new(dummy_ctx, |_| {}, || {}));
                }
            }
        } else if data == "\x7f" || data == "\x08" {
            if let Some((id, _, _, is_priv)) = self.entries.get(self.selected).cloned() {
                if is_priv {
                    self.warning = Some(format!("{id} is launcher-owned"));
                } else {
                    self.user.shift_remove(&id);
                    (self.on_change)(&id, None);
                    self.warning = None;
                }
            }
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_close)();
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for KeybindingsOverlay {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

struct CloneThemeCtx(ThemeRef);
impl crate::render_context::RenderContext for CloneThemeCtx {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, _id: &str) -> String {
        String::new()
    }
    fn request_render(&self) {}
}
