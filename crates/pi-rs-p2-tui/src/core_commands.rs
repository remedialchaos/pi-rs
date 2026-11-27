use crate::keybindings::{
    KeyPlatform, KeybindingsConfig, PRIVILEGED_ACTIONS, builtin_keybindings, is_privileged,
    resolve_privileged_keys,
};
use crate::render_context::{RenderContext, format_key_text};
use crate::theme::Theme;
use indexmap::IndexMap;
use pi_rs_tui::{
    Component, Focusable, Input, SelectItem, SelectList, fuzzy_filter, truncate_to_width,
};
use std::collections::{BTreeMap, HashMap};

pub const KEYBINDING_SOURCES: &[&str] = &[
    "~/.pi/agent/keybindings.json",
    "~/.pi/p2-tui/prefs.json (keybindings)",
];

const TEXT_OVERLAY_HEIGHT: usize = 18;

fn border(theme: &Theme, width: usize) -> String {
    theme.fg("border", &"─".repeat(width.max(1)))
}

#[derive(Debug, Clone)]
pub struct CommandInfo {
    pub id: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub keys: Vec<String>,
    pub slash: Option<String>,
    pub argument_hint: Option<String>,
    pub when: Option<String>,
    pub palette: bool,
}

impl Default for CommandInfo {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            description: String::new(),
            category: "General".to_string(),
            keys: Vec::new(),
            slash: None,
            argument_hint: None,
            when: None,
            palette: true,
        }
    }
}

struct PaletteEntry {
    id: String,
    title: String,
    detail: String,
}

/// Command palette: fuzzy search over active commands (`Commands` + `Input` + `SelectList`).
pub struct CommandPalette {
    ctx: Box<dyn RenderContext>,
    input: Input,
    list: SelectList,
    items: Vec<PaletteEntry>,
    selected_id: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    cancelled: std::rc::Rc<std::cell::Cell<bool>>,
    on_select: Box<dyn Fn(String) + Send>,
    on_cancel: Box<dyn Fn() + Send>,
    focused: bool,
}

impl CommandPalette {
    pub fn new(
        ctx: Box<dyn RenderContext>,
        commands: Vec<CommandInfo>,
        on_select: impl Fn(String) + Send + 'static,
        on_cancel: impl Fn() + Send + 'static,
    ) -> Self {
        let items: Vec<PaletteEntry> = commands
            .into_iter()
            .filter(|item| item.palette && item.id != "p2.palette")
            .map(|item| {
                let mut detail_parts = Vec::new();
                let key_str = format_key_text(&item.keys.join("/"), false);
                if !key_str.is_empty() {
                    detail_parts.push(key_str);
                }
                if let Some(slash) = &item.slash
                    && !slash.is_empty()
                {
                    detail_parts.push(format!("/{slash}"));
                }
                if !item.category.is_empty() {
                    detail_parts.push(item.category.clone());
                }
                PaletteEntry {
                    id: item.id,
                    title: item.label,
                    detail: detail_parts.join(" · "),
                }
            })
            .collect();
        let selected_id = std::rc::Rc::new(std::cell::RefCell::new(None));
        let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
        let list = Self::build_list(ctx.as_ref(), &items, "", &selected_id, &cancelled);
        let mut input = Input::default();
        input.set_focused(true);
        Self {
            ctx,
            input,
            list,
            items,
            selected_id,
            cancelled,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            focused: true,
        }
    }

    fn build_list(
        ctx: &dyn RenderContext,
        items: &[PaletteEntry],
        query: &str,
        selected_id: &std::rc::Rc<std::cell::RefCell<Option<String>>>,
        cancelled: &std::rc::Rc<std::cell::Cell<bool>>,
    ) -> SelectList {
        let filtered: Vec<&PaletteEntry> = if query.is_empty() {
            items.iter().collect()
        } else {
            let owned: Vec<&PaletteEntry> = items.iter().collect();
            fuzzy_filter(&owned, query, |entry| {
                format!("{} {} {}", entry.title, entry.id, entry.detail)
            })
        };
        let select_items: Vec<SelectItem> = filtered
            .into_iter()
            .map(|item| SelectItem {
                value: item.id.clone(),
                label: item.title.clone(),
                description: if item.detail.is_empty() {
                    None
                } else {
                    Some(item.detail.clone())
                },
            })
            .collect();
        let mut list = SelectList::new(
            select_items,
            10,
            ctx.theme().select_list(),
            Default::default(),
        );
        let picked = selected_id.clone();
        list.on_select = Some(Box::new(move |item| {
            *picked.borrow_mut() = Some(item.value.clone());
        }));
        let cancel_flag = cancelled.clone();
        list.on_cancel = Some(Box::new(move || {
            cancel_flag.set(true);
        }));
        list
    }

    fn refilter(&mut self) {
        self.list = Self::build_list(
            self.ctx.as_ref(),
            &self.items,
            self.input.value(),
            &self.selected_id,
            &self.cancelled,
        );
    }
}

impl Component for CommandPalette {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let mut lines = vec![
            border(&theme, width),
            truncate_to_width(&theme.fg("accent", "Commands"), width, true),
        ];
        lines.extend(self.input.render(width));
        lines.extend(self.list.render(width));
        lines.push(border(&theme, width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = pi_rs_tui::get_keybindings();
        if [
            "tui.select.up",
            "tui.select.down",
            "tui.select.confirm",
            "tui.select.cancel",
        ]
        .iter()
        .any(|id| keys.matches(data, id))
        {
            self.list.handle_input(data);
            if let Some(id) = self.selected_id.borrow_mut().take() {
                (self.on_select)(id);
            } else if self.cancelled.replace(false) {
                (self.on_cancel)();
            }
        } else {
            self.input.handle_input(data);
            self.refilter();
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
        self.list.invalidate();
    }
}

impl Focusable for CommandPalette {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

fn format_keys(list: &[String]) -> String {
    if list.is_empty() {
        "unbound".to_string()
    } else {
        format_key_text(&list.join("/"), false)
    }
}

/// `/hotkeys`: commands grouped by category with contexts, launcher keys, conflicts, and config sources.
pub fn hotkeys_report_text(
    ctx: &dyn RenderContext,
    commands: &[CommandInfo],
    user_keys: &KeybindingsConfig,
    role: &str,
) -> Vec<String> {
    let theme = ctx.theme().current();
    let definitions = builtin_keybindings(KeyPlatform::default());
    let privileged = resolve_privileged_keys(&definitions, user_keys);
    let mut lines = Vec::new();
    let push_heading = |out: &mut Vec<String>, text: &str| {
        out.push(String::new());
        out.push(theme.bold(&theme.fg("accent", text)));
    };
    let push_row = |out: &mut Vec<String>, left: &str, right: &str| {
        let padded = format!("{left:<26}");
        out.push(format!("  {} {right}", theme.fg("dim", &padded)));
    };

    let mut by_category: IndexMap<String, Vec<&CommandInfo>> = IndexMap::new();
    for item in commands {
        let category = if item.category.is_empty() {
            "General".to_string()
        } else {
            item.category.clone()
        };
        by_category.entry(category).or_default().push(item);
    }
    for (category, items) in by_category {
        push_heading(&mut lines, &category);
        for item in items {
            let mut desc = item.label.clone();
            if let Some(slash) = &item.slash
                && !slash.is_empty()
            {
                let hint = item
                    .argument_hint
                    .as_deref()
                    .filter(|h| !h.is_empty())
                    .map(|h| format!(" {h}"))
                    .unwrap_or_default();
                desc.push_str(&theme.fg("muted", &format!("  /{slash}{hint}")));
            }
            if let Some(when) = &item.when
                && !when.is_empty()
            {
                desc.push_str(&theme.fg("muted", &format!("  [{when}]")));
            }
            push_row(&mut lines, &format_keys(&item.keys), &desc);
        }
    }

    if role == "owner" {
        push_heading(&mut lines, "Working directory and environment");
        let tab_keys: Vec<String> = user_keys
            .get("tui.input.tab")
            .or_else(|| definitions.get("tui.input.tab").map(|d| &d.default_keys))
            .map(|k| k.as_slice().to_vec())
            .unwrap_or_default();
        push_row(
            &mut lines,
            &format_keys(&tab_keys),
            "New-session and /cd prompts: next environment (this device, then /envs)",
        );
        push_row(
            &mut lines,
            "[env:]path",
            "As the footer shows it: gpu:/data on an environment, /data on this device",
        );
        push_row(
            &mut lines,
            "/cd [[env:]path]",
            "Relative to the session's directory; Tab completes folders; local: is this device",
        );
        push_row(
            &mut lines,
            "/envs",
            "Enter moves the session to the environment (● marks its current one); Ctrl+T test, Ctrl+K host key, Ctrl+F forget",
        );
    }

    push_heading(&mut lines, "Editor and transcript (pi-tui)");
    for (id, def) in &definitions {
        if id.starts_with("tui.editor.") || id.starts_with("tui.altScreen.") {
            let keys = user_keys
                .get(*id)
                .map(|k| k.as_slice().to_vec())
                .unwrap_or_else(|| def.default_keys.as_slice().to_vec());
            push_row(&mut lines, &format_keys(&keys), def.description);
        }
    }

    push_heading(
        &mut lines,
        "Launcher-owned (handled by your local launcher, never by server code)",
    );
    for &id in PRIVILEGED_ACTIONS {
        let keys = privileged.keys.get(id).cloned().unwrap_or_default();
        let desc = definitions.get(id).map(|d| d.description).unwrap_or(id);
        push_row(&mut lines, &format_keys(&keys), desc);
    }

    let mut conflicts = Vec::new();
    for (id, key, other) in &privileged.lost {
        conflicts.push(format!("Launcher action {id} lost key {key} to {other}"));
    }
    let mut seen_by_key: HashMap<(String, Option<&str>), Vec<&str>> = HashMap::new();
    for item in commands {
        for key in &item.keys {
            seen_by_key
                .entry((key.clone(), item.when.as_deref()))
                .or_default()
                .push(item.id.as_str());
        }
    }
    let mut dup_keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for ((key, _when), ids) in seen_by_key {
        if ids.len() > 1 {
            dup_keys.insert(key, ids.into_iter().map(str::to_owned).collect());
        }
    }
    for (key, ids) in dup_keys {
        conflicts.push(format!(
            "Conflict on key {}: {}",
            format_key_text(&key, false),
            ids.join(", ")
        ));
    }
    let unknown: Vec<&String> = user_keys
        .keys()
        .filter(|id| {
            !definitions.contains_key(id.as_str())
                && !commands.iter().any(|cmd| &cmd.id == *id)
                && !is_privileged(id)
        })
        .collect();
    if !conflicts.is_empty() || !unknown.is_empty() {
        push_heading(&mut lines, "Conflicts and problems");
        for msg in conflicts {
            lines.push(format!("  {}", theme.fg("warning", &msg)));
        }
        for id in unknown {
            lines.push(format!(
                "  {}",
                theme.fg("warning", &format!("Unknown keybinding id kept: {id}"))
            ));
        }
    }

    push_heading(&mut lines, "Configuration");
    lines.push(format!(
        "  {}",
        theme.fg(
            "muted",
            &format!(
                "Keybindings: {}; server defaults apply below them.",
                KEYBINDING_SOURCES.join(", then ")
            ),
        )
    ));

    if lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    lines
}

pub fn hotkeys_text(ctx: &dyn RenderContext) -> Vec<String> {
    let definitions = builtin_keybindings(KeyPlatform::default());
    let commands: Vec<CommandInfo> = CORE_COMMANDS
        .iter()
        .map(|cmd| {
            let keys = definitions
                .get(cmd.id)
                .map(|d| d.default_keys.as_slice().to_vec())
                .unwrap_or_default();
            CommandInfo {
                id: cmd.id.to_string(),
                label: cmd.title.to_string(),
                description: String::new(),
                category: cmd.category.to_string(),
                keys,
                slash: cmd.slash.map(str::to_string),
                argument_hint: None,
                when: None,
                palette: cmd.palette,
            }
        })
        .collect();
    hotkeys_report_text(ctx, &commands, &IndexMap::new(), "owner")
}

/// Scrollable read-only text overlay (`18` lines, `n-m of N` footer, Enter/Esc closes).
pub struct TextOverlay {
    ctx: Box<dyn RenderContext>,
    title: String,
    lines: Vec<String>,
    scroll: usize,
    on_close: Box<dyn Fn() + Send>,
    focused: bool,
}

impl TextOverlay {
    pub fn new(
        ctx: Box<dyn RenderContext>,
        title: impl Into<String>,
        lines: Vec<String>,
        on_close: impl Fn() + Send + 'static,
    ) -> Self {
        Self {
            ctx,
            title: title.into(),
            lines,
            scroll: 0,
            on_close: Box::new(on_close),
            focused: false,
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(TEXT_OVERLAY_HEIGHT)
    }
}

impl Component for TextOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let visible: Vec<&String> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(TEXT_OVERLAY_HEIGHT)
            .collect();
        let start = if self.lines.is_empty() {
            0
        } else {
            self.scroll + 1
        };
        let end = self.scroll + visible.len();
        let mut out = vec![
            border(&theme, width),
            truncate_to_width(&theme.fg("accent", &self.title), width, true),
        ];
        for line in visible {
            out.push(truncate_to_width(line, width, true));
        }
        out.push(truncate_to_width(
            &theme.fg(
                "dim",
                &format!(
                    "{start}-{end} of {} · ↑↓ PgUp/PgDn · Esc closes",
                    self.lines.len()
                ),
            ),
            width,
            true,
        ));
        out.push(border(&theme, width));
        out
    }

    fn handle_input(&mut self, data: &str) {
        let keys = pi_rs_tui::get_keybindings();
        let max = self.max_scroll();
        if keys.matches(data, "tui.select.cancel") || keys.matches(data, "tui.select.confirm") {
            (self.on_close)();
        } else if keys.matches(data, "tui.select.up") {
            self.scroll = self.scroll.saturating_sub(1);
        } else if keys.matches(data, "tui.select.down") {
            self.scroll = (self.scroll + 1).min(max);
        } else if keys.matches(data, "tui.select.pageUp") {
            self.scroll = self.scroll.saturating_sub(TEXT_OVERLAY_HEIGHT);
        } else if keys.matches(data, "tui.select.pageDown") {
            self.scroll = (self.scroll + TEXT_OVERLAY_HEIGHT).min(max);
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for TextOverlay {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreAction {
    Palette,
    Hotkeys,
    Settings,
    Keybindings,
    Exit,
    Reload,
    PanelFocus,
    PanelClose,
    ExpandTools,
    ToggleThinking,
}
pub struct CoreCommand {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub slash: Option<&'static str>,
    pub palette: bool,
    pub action: CoreAction,
}
pub const CORE_COMMANDS: &[CoreCommand] = &[
    CoreCommand {
        id: "p2.palette",
        title: "Command palette",
        category: "General",
        slash: None,
        palette: false,
        action: CoreAction::Palette,
    },
    CoreCommand {
        id: "p2.hotkeys",
        title: "Keyboard shortcuts",
        category: "General",
        slash: Some("hotkeys"),
        palette: true,
        action: CoreAction::Hotkeys,
    },
    CoreCommand {
        id: "p2.settings",
        title: "Settings",
        category: "General",
        slash: Some("settings"),
        palette: true,
        action: CoreAction::Settings,
    },
    CoreCommand {
        id: "p2.keybindings",
        title: "Rebind keys",
        category: "General",
        slash: Some("keybindings"),
        palette: true,
        action: CoreAction::Keybindings,
    },
    CoreCommand {
        id: "app.exit",
        title: "Exit",
        category: "General",
        slash: Some("quit"),
        palette: true,
        action: CoreAction::Exit,
    },
    CoreCommand {
        id: "p2.tui.reload",
        title: "Reload the TUI from the server",
        category: "General",
        slash: Some("reload"),
        palette: true,
        action: CoreAction::Reload,
    },
    CoreCommand {
        id: "p2.panel.focus",
        title: "Focus the side panel or the editor",
        category: "View",
        slash: None,
        palette: true,
        action: CoreAction::PanelFocus,
    },
    CoreCommand {
        id: "p2.panel.close",
        title: "Close the side panel",
        category: "View",
        slash: Some("close-panel"),
        palette: true,
        action: CoreAction::PanelClose,
    },
    CoreCommand {
        id: "app.tools.expand",
        title: "Toggle tool output",
        category: "View",
        slash: None,
        palette: true,
        action: CoreAction::ExpandTools,
    },
    CoreCommand {
        id: "app.thinking.toggle",
        title: "Toggle thinking blocks",
        category: "View",
        slash: None,
        palette: true,
        action: CoreAction::ToggleThinking,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeyIdValue;
    use crate::theme::ThemeRef;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct DummyCtx(ThemeRef);
    impl RenderContext for DummyCtx {
        fn theme(&self) -> &ThemeRef {
            &self.0
        }
        fn key_text(&self, _id: &str) -> String {
            String::new()
        }
        fn request_render(&self) {}
    }

    #[test]
    fn command_palette_renders_commands_input_select_list_and_excludes_palette_command() {
        let picked = Arc::new(Mutex::new(None));
        let picked_out = picked.clone();
        let commands = vec![
            CommandInfo {
                id: "p2.palette".into(),
                label: "Command palette".into(),
                category: "General".into(),
                keys: vec!["f1".into()],
                palette: false,
                ..Default::default()
            },
            CommandInfo {
                id: "p2.settings".into(),
                label: "Settings".into(),
                category: "General".into(),
                slash: Some("settings".into()),
                keys: vec![],
                palette: true,
                ..Default::default()
            },
        ];
        let mut palette = CommandPalette::new(
            Box::new(DummyCtx(ThemeRef::new(Theme::default()))),
            commands,
            move |id| {
                *picked_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
            },
            || {},
        );
        let plain = pi_rs_tui::strip_terminal_sequences(&palette.render(80).join("\n"));
        assert!(plain.contains("Commands"), "{plain}");
        assert!(plain.contains("Settings"), "{plain}");
        assert!(plain.contains("/settings · General"), "{plain}");
        assert!(!plain.contains("Command palette"), "{plain}");
        palette.handle_input("\r");
        assert_eq!(
            picked.lock().unwrap_or_else(|e| e.into_inner()).as_deref(),
            Some("p2.settings")
        );
    }

    #[test]
    fn hotkeys_text_includes_categories_privileged_section_conflicts_and_sources() {
        let mut user = KeybindingsConfig::new();
        user.insert("p2.attach".into(), KeyIdValue::One("f3".into()));
        user.insert("plugin.pick".into(), KeyIdValue::One("f3".into()));
        user.insert("unknown.custom".into(), KeyIdValue::One("f9".into()));
        let commands = vec![CommandInfo {
            id: "p2.hotkeys".into(),
            label: "Keyboard shortcuts".into(),
            category: "General".into(),
            slash: Some("hotkeys".into()),
            when: Some("!overlay".into()),
            palette: true,
            ..Default::default()
        }];
        let out = hotkeys_report_text(
            &DummyCtx(ThemeRef::new(Theme::default())),
            &commands,
            &user,
            "owner",
        );
        let plain = pi_rs_tui::strip_terminal_sequences(&out.join("\n"));
        assert!(plain.contains("General"), "{plain}");
        assert!(plain.contains("[!overlay]"), "{plain}");
        assert!(plain.contains("Editor and transcript (pi-tui)"), "{plain}");
        assert!(plain.contains("Launcher-owned"), "{plain}");
        assert!(plain.contains("Conflicts and problems"), "{plain}");
        assert!(
            plain.contains("Unknown keybinding id kept: unknown.custom"),
            "{plain}"
        );
        assert!(plain.contains(KEYBINDING_SOURCES[0]), "{plain}");
    }

    #[test]
    fn text_overlay_renders_18_lines_footer_and_closes_on_confirm_or_cancel() {
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_out = closed.clone();
        let lines: Vec<String> = (1..=30).map(|i| format!("row {i:02}")).collect();
        let mut overlay = TextOverlay::new(
            Box::new(DummyCtx(ThemeRef::new(Theme::default()))),
            "Keyboard shortcuts",
            lines,
            move || {
                closed_out.fetch_add(1, Ordering::Relaxed);
            },
        );
        let rendered = pi_rs_tui::strip_terminal_sequences(&overlay.render(80).join("\n"));
        assert!(rendered.contains("Keyboard shortcuts"), "{rendered}");
        assert!(
            rendered.contains("row 01") && rendered.contains("row 18"),
            "{rendered}"
        );
        assert!(!rendered.contains("row 19"), "{rendered}");
        assert!(
            rendered.contains("1-18 of 30 · ↑↓ PgUp/PgDn · Esc closes"),
            "{rendered}"
        );
        overlay.handle_input("\r");
        assert_eq!(closed.load(Ordering::Relaxed), 1);
    }
}
