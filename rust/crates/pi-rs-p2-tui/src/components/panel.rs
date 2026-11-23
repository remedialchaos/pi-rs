use crate::render_context::RenderContext;
use pi_rs_tui::{Component, Focusable, truncate_to_width};
use std::cell::Cell;

pub trait PanelContent: Component {
    fn dispose(&mut self) {}
}
pub struct PanelFrame {
    pub id: String,
    pub content: Box<dyn PanelContent>,
    pub focused: bool,
    ctx: Box<dyn RenderContext>,
    title: Box<dyn Fn() -> String + Send>,
    height: Option<usize>,
    top: Cell<usize>,
}
impl PanelFrame {
    pub fn new(
        ctx: Box<dyn RenderContext>,
        id: String,
        content: Box<dyn PanelContent>,
        title: impl Fn() -> String + Send + 'static,
    ) -> Self {
        Self {
            id,
            content,
            focused: false,
            ctx,
            title: Box::new(title),
            height: None,
            top: Cell::default(),
        }
    }
    pub fn set_height(&mut self, rows: usize) {
        self.height = Some(rows);
    }
}
impl Component for PanelFrame {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let bar = theme.fg(if self.focused { "accent" } else { "border" }, "│");
        if self.height == Some(0) {
            return Vec::new();
        }
        let (key, label) = if self.focused {
            ("tui.select.cancel", "back")
        } else {
            ("p2.panel.focus", "focus")
        };
        let hint = crate::render_context::key_hint(self.ctx.as_ref(), key, label);
        let mut lines = vec![truncate_to_width(
            &format!("{bar} {}  {hint}", theme.bold(&(self.title)())),
            width,
            true,
        )];
        let body = self.content.render(width.saturating_sub(2).max(1));
        let rows = self.height.map_or(body.len(), |h| h.saturating_sub(1));
        self.top
            .set(self.top.get().min(body.len().saturating_sub(rows)));
        for row in 0..rows {
            let line = body.get(self.top.get() + row).map_or("", String::as_str);
            lines.push(truncate_to_width(&format!("{bar} {line}"), width, true));
        }
        lines
    }
    fn handle_input(&mut self, data: &str) {
        if self.focused {
            let keys = pi_rs_tui::get_keybindings();
            let page = self.height.unwrap_or(2).saturating_sub(1).max(1);
            let top = self.top.get();
            let next = if keys.matches(data, "tui.select.pageDown") {
                top.saturating_add(page)
            } else if keys.matches(data, "tui.select.pageUp") {
                top.saturating_sub(page)
            } else if keys.matches(data, "tui.select.down") {
                top.saturating_add(1)
            } else if keys.matches(data, "tui.select.up") {
                top.saturating_sub(1)
            } else {
                self.content.handle_input(data);
                return;
            };
            self.top.set(next);
            self.ctx.request_render();
        }
    }
    fn invalidate(&mut self) {
        self.content.invalidate();
    }
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}
impl Focusable for PanelFrame {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
impl Drop for PanelFrame {
    fn drop(&mut self) {
        self.content.dispose();
    }
}
