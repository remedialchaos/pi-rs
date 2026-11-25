use crate::render_context::RenderContext;
use pi_rs_tui::{Component, Focusable};

pub trait SelectorKeys {
    fn matches(&self, data: &str, id: &str) -> bool;
}

#[derive(Clone)]
pub struct SelectorContext {
    pub theme: std::rc::Rc<dyn RenderContext>,
    pub keys: std::rc::Rc<dyn SelectorKeys>,
}

impl SelectorContext {
    pub fn new(theme: Box<dyn RenderContext>, keys: Box<dyn SelectorKeys>) -> Self {
        Self {
            theme: theme.into(),
            keys: keys.into(),
        }
    }
}

pub struct DynamicBorder {
    ctx: Box<dyn RenderContext>,
    token: BorderToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderToken {
    Border,
    BorderAccent,
    BorderMuted,
}

impl DynamicBorder {
    pub fn new(ctx: Box<dyn RenderContext>, token: BorderToken) -> Self {
        Self { ctx, token }
    }
}

impl Component for DynamicBorder {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let color = match self.token {
            BorderToken::Border => "border",
            BorderToken::BorderAccent => "borderAccent",
            BorderToken::BorderMuted => "borderMuted",
        };
        vec![theme.fg(color, &"─".repeat(width.max(1)))]
    }
    fn invalidate(&mut self) {}
}

#[derive(Debug, Clone)]
pub struct SelectorModel {
    pub provider: String,
    pub model_id: String,
    pub name: String,
}

type ModelRef = (String, String);

fn search_text(model: &SelectorModel) -> String {
    format!(
        "{} {}/{} {} {} {}",
        model.provider, model.provider, model.model_id, model.provider, model.model_id, model.name
    )
}

pub struct ModelSelectorComponent {
    ctx: SelectorContext,
    models: Vec<SelectorModel>,
    enabled: Vec<ModelRef>,
    current: Option<ModelRef>,
    default: Option<ModelRef>,
    search: String,
    selected: usize,
    focused: bool,
}

impl ModelSelectorComponent {
    pub fn new(
        ctx: SelectorContext,
        models: Vec<SelectorModel>,
        enabled: Vec<ModelRef>,
        current: Option<ModelRef>,
        default: Option<ModelRef>,
    ) -> Self {
        Self {
            ctx,
            models,
            enabled,
            current,
            default,
            search: String::new(),
            selected: 0,
            focused: false,
        }
    }
}

impl Component for ModelSelectorComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let mut lines = vec![theme.bold("Model Selector")];
        for (i, model) in self
            .models
            .iter()
            .filter(|m| pi_rs_tui::fuzzy_match(&self.search, &search_text(m)).matches)
            .enumerate()
            .skip(self.selected.saturating_sub(9))
            .take(10)
        {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let enabled = self
                .enabled
                .contains(&(model.provider.clone(), model.model_id.clone()));
            let id = Some((model.provider.clone(), model.model_id.clone()));
            let marker = if id == self.current {
                "current"
            } else if id == self.default {
                "default"
            } else if enabled {
                "✓"
            } else {
                " "
            };
            lines.push(format!(
                "{} {} {} [{}]",
                prefix, model.provider, model.model_id, marker
            ));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            if self.selected > 0 {
                self.selected -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.selected < self.models.len().saturating_sub(1) {
                self.selected += 1;
            }
        } else if keys.matches(data, "tui.select.confirm") {
            if let Some(model) = self
                .models
                .iter()
                .filter(|m| pi_rs_tui::fuzzy_match(&self.search, &search_text(m)).matches)
                .nth(self.selected)
            {
                let id = (model.provider.clone(), model.model_id.clone());
                if let Some(i) = self.enabled.iter().position(|v| v == &id) {
                    self.enabled.remove(i);
                } else {
                    self.enabled.push(id);
                }
            }
        } else if data == "\x7f" {
            self.search.pop();
            self.selected = 0;
        } else if data.chars().count() == 1 && !data.chars().any(char::is_control) {
            self.search.push_str(data);
            self.selected = 0;
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ModelSelectorComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

const LEVEL_DESCRIPTIONS: &[(&str, &str)] = &[
    ("off", "No thinking"),
    ("minimal", "Minimal thinking"),
    ("low", "Low thinking"),
    ("medium", "Medium thinking"),
    ("high", "High thinking"),
    ("xhigh", "Extra high thinking"),
    ("max", "Maximum thinking"),
];

pub struct ThinkingSelectorComponent {
    ctx: SelectorContext,
    levels: Vec<String>,
    current: String,
    selected: usize,
    focused: bool,
}

impl ThinkingSelectorComponent {
    pub fn new(ctx: SelectorContext, levels: Vec<String>, current: String) -> Self {
        let selected = levels.iter().position(|l| l == &current).unwrap_or(0);
        Self {
            ctx,
            levels,
            current,
            selected,
            focused: false,
        }
    }
}

impl Component for ThinkingSelectorComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let mut lines = vec![theme.bold("Thinking Level")];
        for (i, level) in self.levels.iter().enumerate().take(10) {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let desc = LEVEL_DESCRIPTIONS
                .iter()
                .find(|(k, _)| *k == level)
                .map(|(_, d)| *d)
                .unwrap_or("");
            lines.push(format!("{} {}  {}", prefix, level, theme.fg("muted", desc)));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            if self.selected > 0 {
                self.selected -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.selected < self.levels.len().saturating_sub(1) {
                self.selected += 1;
            }
        } else if keys.matches(data, "tui.select.confirm")
            && let Some(level) = self.levels.get(self.selected)
        {
            self.current = level.clone();
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ThinkingSelectorComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub struct ScopedModelsSelectorComponent {
    ctx: SelectorContext,
    models: Vec<SelectorModel>,
    enabled: Vec<ModelRef>,
    search: String,
    selected: usize,
    focused: bool,
}

impl ScopedModelsSelectorComponent {
    pub fn new(ctx: SelectorContext, models: Vec<SelectorModel>, enabled: Vec<ModelRef>) -> Self {
        Self {
            ctx,
            models,
            enabled,
            search: String::new(),
            selected: 0,
            focused: false,
        }
    }
}

impl Component for ScopedModelsSelectorComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let mut lines = vec![theme.bold("Scoped Models")];
        for (i, model) in self
            .models
            .iter()
            .filter(|m| pi_rs_tui::fuzzy_match(&self.search, &search_text(m)).matches)
            .enumerate()
            .skip(self.selected.saturating_sub(9))
            .take(10)
        {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let enabled = self
                .enabled
                .contains(&(model.provider.clone(), model.model_id.clone()));
            let marker = if enabled { "✓" } else { " " };
            lines.push(format!(
                "{} {} {} [{}]",
                prefix, model.provider, model.model_id, marker
            ));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            if self.selected > 0 {
                self.selected -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.selected < self.models.len().saturating_sub(1) {
                self.selected += 1;
            }
        } else if keys.matches(data, "tui.select.confirm") {
            if let Some(model) = self
                .models
                .iter()
                .filter(|m| pi_rs_tui::fuzzy_match(&self.search, &search_text(m)).matches)
                .nth(self.selected)
            {
                let id = (model.provider.clone(), model.model_id.clone());
                if let Some(i) = self.enabled.iter().position(|v| v == &id) {
                    self.enabled.remove(i);
                } else {
                    self.enabled.push(id);
                }
            }
        } else if data == "\x7f" {
            self.search.pop();
            self.selected = 0;
        } else if data.chars().count() == 1 && !data.chars().any(char::is_control) {
            self.search.push_str(data);
            self.selected = 0;
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ScopedModelsSelectorComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
