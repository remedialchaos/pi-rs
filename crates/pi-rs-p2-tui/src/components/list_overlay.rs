use crate::components::selectors::SelectorContext;
use pi_rs_tui::{Component, Focusable, KeyId};

#[derive(Debug, Clone)]
pub struct ListRow {
    pub key: String,
    pub label: String,
    pub detail: Option<String>,
    pub description: Option<String>,
    pub color: Option<RowColor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowColor {
    Accent,
    Muted,
    Warning,
    Error,
    Success,
}

type RowAction = Box<dyn Fn(Option<&ListRow>) + Send + Sync>;
pub struct ListAction {
    pub id: Option<String>,
    pub key: Option<KeyId>,
    pub label: String,
    pub run: RowAction,
}

pub struct ListOverlay {
    ctx: SelectorContext,
    rows: Vec<ListRow>,
    actions: Vec<ListAction>,
    on_select: Option<RowAction>,
    filter: String,
    selected: usize,
    on_close: Box<dyn Fn() + Send + Sync>,
    focused: bool,
}

impl ListOverlay {
    pub fn new(
        ctx: SelectorContext,
        rows: Vec<ListRow>,
        actions: Vec<ListAction>,
        on_close: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            ctx,
            rows,
            actions,
            on_select: None,
            filter: String::new(),
            selected: 0,
            on_close: Box::new(on_close),
            focused: false,
        }
    }

    pub fn with_on_select(mut self, select: impl Fn(&ListRow) + Send + Sync + 'static) -> Self {
        self.on_select = Some(Box::new(move |row| {
            if let Some(row) = row {
                select(row)
            }
        }));
        self
    }

    fn filtered_rows(&self) -> Vec<&ListRow> {
        if self.filter.is_empty() {
            self.rows.iter().collect()
        } else {
            self.rows
                .iter()
                .filter(|r| pi_rs_tui::fuzzy_match(&self.filter, &r.label).matches)
                .collect()
        }
    }
}

impl Component for ListOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let filtered = self.filtered_rows();
        let mut lines = Vec::new();

        lines.push(theme.fg("accent", &format!("List: {}", self.filter)));
        lines.push(theme.fg("border", &"─".repeat(width)));

        for (i, row) in filtered
            .iter()
            .enumerate()
            .skip(self.selected.saturating_sub(9))
            .take(10)
        {
            let prefix = if i == self.selected { "→ " } else { "  " };
            let color = match row.color {
                Some(RowColor::Accent) => "accent",
                Some(RowColor::Muted) => "muted",
                Some(RowColor::Warning) => "warning",
                Some(RowColor::Error) => "error",
                Some(RowColor::Success) => "success",
                None => "text",
            };
            let style = if i == self.selected { "accent" } else { color };
            let detail = row.detail.as_deref().unwrap_or("");
            lines.push(format!(
                "{}{}{}",
                prefix,
                theme.fg(style, &row.label),
                theme.fg("muted", detail)
            ));
        }

        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        let len = self.filtered_rows().len();
        if keys.matches(data, "tui.select.up") {
            self.selected = if self.selected == 0 {
                len.saturating_sub(1)
            } else {
                self.selected - 1
            };
        } else if keys.matches(data, "tui.select.down") {
            if len > 0 {
                self.selected = (self.selected + 1) % len;
            }
        } else if keys.matches(data, "tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(10);
        } else if keys.matches(data, "tui.select.pageDown") {
            self.selected = (self.selected + 10).min(len.saturating_sub(1));
        } else if keys.matches(data, "tui.select.confirm") {
            let filtered = self.filtered_rows();
            if let Some(select) = &self.on_select {
                select(filtered.get(self.selected).copied());
            }
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_close)();
        } else if let Some(action) = self.actions.iter().find(|action| match &action.id {
            Some(id) => keys.matches(data, id),
            None => action
                .key
                .as_ref()
                .is_some_and(|key| pi_rs_tui::keys::matches_key_data(data, key)),
        }) {
            (action.run)(self.filtered_rows().get(self.selected).copied());
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

impl Focusable for ListOverlay {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub struct ConfirmOverlay {
    ctx: SelectorContext,
    lines: Vec<String>,
    on_confirm: Box<dyn Fn(bool) + Send + Sync>,
    selected: bool,
    focused: bool,
}

impl ConfirmOverlay {
    pub fn new(
        ctx: SelectorContext,
        lines: Vec<String>,
        on_confirm: impl Fn(bool) + Send + Sync + 'static,
    ) -> Self {
        Self {
            ctx,
            lines,
            on_confirm: Box::new(on_confirm),
            selected: true,
            focused: false,
        }
    }
}

impl Component for ConfirmOverlay {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let mut lines = vec![theme.bold("Confirm")];
        lines.extend(self.lines.iter().map(|l| theme.fg("text", l)));
        let yes = if self.selected {
            theme.fg("accent", "[Yes]")
        } else {
            theme.fg("muted", "[Yes]")
        };
        let no = if !self.selected {
            theme.fg("accent", "[No]")
        } else {
            theme.fg("muted", "[No]")
        };
        lines.push(format!("{}  {}", yes, no));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.left") || keys.matches(data, "tui.select.right") {
            self.selected = !self.selected;
        } else if keys.matches(data, "tui.select.confirm") {
            (self.on_confirm)(self.selected);
        } else if data.eq_ignore_ascii_case("y") {
            (self.on_confirm)(true);
        } else if keys.matches(data, "tui.select.cancel") || data == "n" || data == "N" {
            (self.on_confirm)(false);
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ConfirmOverlay {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub async fn confirm(
    ctx: SelectorContext,
    lines: Vec<String>,
    mount: impl FnOnce(ConfirmOverlay) -> Box<dyn FnOnce()>,
) -> bool {
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = std::sync::Mutex::new(Some(send));
    let close = mount(ConfirmOverlay::new(ctx, lines, move |answer| {
        if let Ok(mut slot) = send.lock()
            && let Some(sender) = slot.take()
        {
            let _ = sender.send(answer);
        }
    }));
    let answer = receive.await.unwrap_or(false);
    close();
    answer
}

pub struct TextViewer {
    ctx: SelectorContext,
    text: String,
    on_close: Box<dyn Fn()>,
    line: usize,
    focused: bool,
}

impl TextViewer {
    pub fn new(
        ctx: SelectorContext,
        text: String,
        line: usize,
        on_close: impl Fn() + 'static,
    ) -> Self {
        Self {
            ctx,
            text,
            on_close: Box::new(on_close),
            line: line.saturating_sub(1),
            focused: false,
        }
    }
}

impl Component for TextViewer {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let lines: Vec<&str> = self.text.lines().collect();
        lines
            .iter()
            .skip(self.line)
            .take(20)
            .map(|l| theme.fg("text", l))
            .collect()
    }

    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            if self.line > 0 {
                self.line -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            let lines: Vec<&str> = self.text.lines().collect();
            if self.line < lines.len().saturating_sub(20) {
                self.line += 1;
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

impl Focusable for TextViewer {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
