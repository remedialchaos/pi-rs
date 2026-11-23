use crate::keybindings::{PRIVILEGED_ACTIONS, builtin_keybindings};
use crate::render_context::format_key_text;
use pi_rs_tui::{Component, Focusable, truncate_to_width};

pub const KEYBINDING_SOURCES: &[&str] = &[
    "~/.pi/agent/keybindings.json",
    "~/.pi/p2-tui/prefs.json (keybindings)",
];

fn border(ctx: &dyn crate::render_context::RenderContext, width: usize) -> String {
    ctx.theme()
        .current()
        .fg("border", &"─".repeat(width.max(1)))
}

pub struct CommandPalette {
    ctx: Box<dyn crate::render_context::RenderContext>,
    commands: Vec<CommandInfo>,
    filter: String,
    selected: usize,
    on_select: Box<dyn Fn(String) + Send>,
    on_cancel: Box<dyn Fn() + Send>,
    focused: bool,
}

#[derive(Debug, Clone)]
pub struct CommandInfo {
    pub id: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub keys: Vec<String>,
}

impl CommandPalette {
    pub fn new(
        ctx: Box<dyn crate::render_context::RenderContext>,
        commands: Vec<CommandInfo>,
        on_select: impl Fn(String) + Send + 'static,
        on_cancel: impl Fn() + Send + 'static,
    ) -> Self {
        Self {
            ctx,
            commands,
            filter: String::new(),
            selected: 0,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            focused: false,
        }
    }

    fn filtered(&self) -> Vec<&CommandInfo> {
        if self.filter.is_empty() {
            self.commands.iter().collect()
        } else {
            self.commands
                .iter()
                .filter(|c| {
                    pi_rs_tui::fuzzy_match(&self.filter, &c.label).matches
                        || pi_rs_tui::fuzzy_match(&self.filter, &c.id).matches
                })
                .collect()
        }
    }
}

impl Component for CommandPalette {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let filtered = self.filtered();
        let mut lines = Vec::new();

        lines.push(theme.fg("accent", &format!("Command Palette: {}", self.filter)));
        lines.push(border(self.ctx.as_ref(), width));

        for (i, cmd) in filtered.iter().enumerate().take(10) {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let style = if i == self.selected { "accent" } else { "text" };
            let keys = cmd.keys.join(", ");
            lines.push(format!(
                "{}{}  {}",
                prefix,
                theme.fg(style, &cmd.label),
                theme.fg("muted", &keys)
            ));
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
            let filtered = self.filtered();
            if self.selected < filtered.len().saturating_sub(1) {
                self.selected += 1;
            }
        } else if keys.matches(data, "tui.select.confirm") {
            let filtered = self.filtered();
            if let Some(cmd) = filtered.get(self.selected) {
                (self.on_select)(cmd.id.clone());
            }
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        } else if data.chars().count() == 1 && !data.chars().any(char::is_control) {
            self.filter.push_str(data);
            self.selected = 0;
        } else if data == "\x7f" || data == "\x08" {
            self.filter.pop();
            self.selected = 0;
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for CommandPalette {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub fn hotkeys_text(ctx: &dyn crate::render_context::RenderContext) -> Vec<String> {
    let theme = ctx.theme().current();
    let mut lines = vec![theme.bold("Hotkeys")];
    lines.push(String::new());

    let keybindings = builtin_keybindings(crate::keybindings::KeyPlatform::default());
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
        lines.push(format!(
            "{}  {}",
            theme.fg(style, id),
            theme.fg("muted", &keys)
        ));
    }
    lines
}

pub struct TextOverlay {
    lines: Vec<String>,
    scroll: usize,
    on_close: Box<dyn Fn() + Send>,
    focused: bool,
}

impl TextOverlay {
    pub fn new(lines: Vec<String>, on_close: impl Fn() + Send + 'static) -> Self {
        Self {
            lines,
            scroll: 0,
            on_close: Box::new(on_close),
            focused: false,
        }
    }
}

impl Component for TextOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec!["Text Viewer".to_string()];
        lines.push(String::new());

        for line in self.lines.iter().skip(self.scroll).take(20) {
            lines.push(truncate_to_width(line, width, true));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = pi_rs_tui::get_keybindings();
        if keys.matches(data, "tui.select.up") {
            if self.scroll > 0 {
                self.scroll -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.scroll < self.lines.len().saturating_sub(20) {
                self.scroll += 1;
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
    pub slash: Option<&'static str>,
    pub action: CoreAction,
}
pub const CORE_COMMANDS: &[CoreCommand] = &[
    CoreCommand {
        id: "p2.palette",
        title: "Command palette",
        slash: None,
        action: CoreAction::Palette,
    },
    CoreCommand {
        id: "p2.hotkeys",
        title: "Keyboard shortcuts",
        slash: Some("hotkeys"),
        action: CoreAction::Hotkeys,
    },
    CoreCommand {
        id: "p2.settings",
        title: "Settings",
        slash: Some("settings"),
        action: CoreAction::Settings,
    },
    CoreCommand {
        id: "p2.keybindings",
        title: "Rebind keys",
        slash: Some("keybindings"),
        action: CoreAction::Keybindings,
    },
    CoreCommand {
        id: "app.exit",
        title: "Exit",
        slash: Some("quit"),
        action: CoreAction::Exit,
    },
    CoreCommand {
        id: "p2.tui.reload",
        title: "Reload the TUI from the server",
        slash: Some("reload"),
        action: CoreAction::Reload,
    },
    CoreCommand {
        id: "p2.panel.focus",
        title: "Focus the side panel or the editor",
        slash: None,
        action: CoreAction::PanelFocus,
    },
    CoreCommand {
        id: "p2.panel.close",
        title: "Close the side panel",
        slash: Some("close-panel"),
        action: CoreAction::PanelClose,
    },
    CoreCommand {
        id: "app.tools.expand",
        title: "Toggle tool output",
        slash: None,
        action: CoreAction::ExpandTools,
    },
    CoreCommand {
        id: "app.thinking.toggle",
        title: "Toggle thinking blocks",
        slash: None,
        action: CoreAction::ToggleThinking,
    },
];
