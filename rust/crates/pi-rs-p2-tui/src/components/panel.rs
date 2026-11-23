use crate::render_context::RenderContext;
use pi_rs_tui::{Component, truncate_to_width};

pub trait PanelContent: Component {
    fn dispose(&mut self) {}
}
pub struct PanelFrame {
    pub id: String,
    pub content: Box<dyn PanelContent>,
    pub focused: bool,
    ctx: Box<dyn RenderContext>,
    title: Box<dyn Fn() -> String + Send>,
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
        }
    }
}
impl Component for PanelFrame {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let bar = theme.fg(if self.focused { "accent" } else { "border" }, "│");
        let mut lines = vec![truncate_to_width(&(self.title)(), width, true)];
        lines.extend(
            self.content
                .render(width.saturating_sub(2))
                .into_iter()
                .map(|line| format!("{bar} {line}")),
        );
        lines
    }
    fn handle_input(&mut self, data: &str) {
        if self.focused {
            self.content.handle_input(data);
        }
    }
    fn invalidate(&mut self) {
        self.content.invalidate();
    }
}
impl Drop for PanelFrame {
    fn drop(&mut self) {
        self.content.dispose();
    }
}
