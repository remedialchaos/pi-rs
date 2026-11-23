use crate::render_context::{RenderContext, more_lines_hint, safe_text};
use pi_rs_tui::Component;

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";

pub type MessageContext = dyn RenderContext;

fn zone(lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return lines.to_vec();
    }
    let mut zoned = Vec::with_capacity(lines.len() + 2);
    zoned.push(OSC133_ZONE_START.to_string());
    zoned.extend(lines.iter().cloned());
    zoned.push(OSC133_ZONE_END.to_string());
    zoned
}

pub struct UserMessageComponent {
    ctx: Box<MessageContext>,
    text: String,
    author: Option<String>,
    expanded: bool,
}

impl UserMessageComponent {
    pub fn new(ctx: Box<MessageContext>, text: String, author: Option<String>) -> Self {
        Self {
            ctx,
            text,
            author,
            expanded: false,
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
    }
}

impl Component for UserMessageComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let mut lines = Vec::new();

        if let Some(author) = &self.author {
            lines.push(theme.fg("customMessageLabel", &format!("[{}]", author)));
        }

        lines.push(theme.fg("userMessageText", &safe_text(&self.text)));

        if !self.expanded && self.text.lines().count() > 3 {
            lines.push(more_lines_hint(
                self.ctx.as_ref(),
                &theme,
                self.text.lines().count() - 3,
                "lines",
            ));
        }

        zone(&lines)
    }

    fn invalidate(&mut self) {}
}

pub struct AssistantMessageComponent {
    ctx: Box<MessageContext>,
    message: crate::components::transcript::AssistantMessage,
    expanded: bool,
}

impl AssistantMessageComponent {
    pub fn new(
        ctx: Box<MessageContext>,
        message: crate::components::transcript::AssistantMessage,
    ) -> Self {
        Self {
            ctx,
            message,
            expanded: false,
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let mut lines = Vec::new();

        for block in &self.message.content {
            if let crate::components::transcript::ContentBlock::Text { text } = block {
                lines.extend(pi_rs_tui::wrap_text_with_ansi(
                    &theme.fg("text", &safe_text(text)),
                    width,
                ));
            }
        }

        zone(&lines)
    }

    fn invalidate(&mut self) {}
}

pub struct CompactionSummaryComponent {
    ctx: Box<MessageContext>,
    summary: String,
    expanded: bool,
}

impl CompactionSummaryComponent {
    pub fn new(ctx: Box<MessageContext>, summary: String) -> Self {
        Self {
            ctx,
            summary,
            expanded: false,
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
    }
}

impl Component for CompactionSummaryComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let mut lines = Vec::new();

        if self.expanded {
            lines.push(theme.fg("muted", &self.summary));
        } else {
            lines.push(theme.fg(
                "muted",
                &format!(
                    "Context compacted ({})",
                    crate::render_context::format_tokens(self.summary.len() as u64)
                ),
            ));
        }

        zone(&lines)
    }

    fn invalidate(&mut self) {}
}

pub struct NoticeComponent {
    ctx: Box<MessageContext>,
    text: String,
    level: String,
}

impl NoticeComponent {
    pub fn new(ctx: Box<MessageContext>, text: String, level: String) -> Self {
        Self { ctx, text, level }
    }
}

impl Component for NoticeComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme().current();
        let color = match self.level.as_str() {
            "error" => "error",
            "warning" => "warning",
            _ => "muted",
        };
        vec![zone(&[theme.fg(color, &safe_text(&self.text))])[0].clone()]
    }

    fn invalidate(&mut self) {}
}
