use crate::render_context::{RenderContext, line_text};
use crate::theme::Theme;
use pi_rs_tui::{
    Component, Text, background_ansi, foreground_ansi, truncate_to_width, visible_width,
};

const CORAL: pi_rs_tui::RgbColor = pi_rs_tui::RgbColor {
    r: 228.0,
    g: 138.0,
    b: 122.0,
};
const BLUE: pi_rs_tui::RgbColor = pi_rs_tui::RgbColor {
    r: 79.0,
    g: 142.0,
    b: 179.0,
};
const YELLOW: pi_rs_tui::RgbColor = pi_rs_tui::RgbColor {
    r: 234.0,
    g: 182.0,
    b: 93.0,
};
const RESET: &str = "\x1b[0m";

pub fn pi_logo_lines(ctx: &dyn RenderContext) -> [String; 2] {
    let mode = ctx.theme().current().get_color_mode();
    let fg = |color: pi_rs_tui::RgbColor| foreground_ansi(&pi_rs_tui::Color::Rgb(color), mode);
    let bg = |color: pi_rs_tui::RgbColor| background_ansi(&pi_rs_tui::Color::Rgb(color), mode);
    let blue = fg(BLUE);
    let yellow = fg(YELLOW);
    let coral = fg(CORAL);
    let blue_bg = bg(BLUE);
    [
        format!("{coral}{blue_bg}▀{RESET}{coral}▀█{RESET} "),
        format!("{blue}█▀{RESET} {yellow}█{RESET}"),
    ]
}

pub struct HeaderSource {
    pub ctx: Box<dyn RenderContext>,
    pub origin: Option<String>,
    pub role: Option<String>,
    pub connected: bool,
    pub expanded: bool,
    pub hints: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    pub expanded_hints: Box<dyn Fn() -> Vec<String> + Send + Sync>,
}

pub struct BuiltInHeader {
    source: HeaderSource,
}

impl BuiltInHeader {
    pub fn new(source: HeaderSource) -> Self {
        Self { source }
    }
}

impl Component for BuiltInHeader {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.source.ctx.theme().current();
        let [top, bottom] = pi_logo_lines(self.source.ctx.as_ref());
        let where_parts: Vec<String> = [
            self.source
                .origin
                .as_deref()
                .map(line_text)
                .unwrap_or_default(),
            self.source.role.as_deref().unwrap_or("").to_owned(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
        let where_text = where_parts.join(" · ");
        let state = if self.source.connected {
            String::new()
        } else {
            format!(" {}", theme.fg("warning", "reconnecting…"))
        };
        let separator = theme.fg("muted", " · ");
        let hints = if self.source.expanded {
            (self.source.expanded_hints)()
        } else {
            (self.source.hints)()
        };
        let mut lines = vec![format!("{top} {}{state}", theme.fg("dim", &where_text))];
        if self.source.expanded {
            lines.push(bottom);
            lines.extend(hints);
        } else {
            lines.push(format!("{bottom} {}", hints.join(&separator)));
            lines.push(theme.fg(
                "dim",
                &format!(
                    "Press {} to show full startup help.",
                    self.source.ctx.key_text("app.tools.expand")
                ),
            ));
        }
        let mut out = vec![String::new()];
        out.extend(Text::new(lines.join("\n"), 1, 0, None).render(width));
        out
    }

    fn invalidate(&mut self) {}
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

pub struct StatusLines {
    notices: Vec<Notice>,
}

impl Default for StatusLines {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusLines {
    pub fn new() -> Self {
        Self {
            notices: Vec::new(),
        }
    }

    pub fn add(&mut self, kind: NoticeKind, text: String) {
        if let Some(last) = self.notices.last_mut()
            && std::mem::discriminant(&last.kind) == std::mem::discriminant(&kind)
            && last.text == text
        {
            last.count += 1;
            return;
        }
        self.notices.push(Notice {
            kind,
            text,
            count: 1,
        });
    }
}

impl Component for StatusLines {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = Theme::default();
        self.notices
            .iter()
            .map(|n| {
                let color = match n.kind {
                    NoticeKind::Info => "text",
                    NoticeKind::Warning => "warning",
                    NoticeKind::Error => "error",
                };
                let count = if n.count > 1 {
                    format!(" (x{})", n.count)
                } else {
                    String::new()
                };
                theme.fg(color, &format!("{}{}", n.text, count))
            })
            .collect()
    }

    fn invalidate(&mut self) {}
}

pub struct BuiltInFooter {
    left: String,
    right: String,
}

impl BuiltInFooter {
    pub fn new(left: String, right: String) -> Self {
        Self { left, right }
    }
}

impl Component for BuiltInFooter {
    fn render(&self, width: usize) -> Vec<String> {
        vec![spread(&self.left, &self.right, width, 1)]
    }

    fn invalidate(&mut self) {}
}

pub fn spread(left: &str, right: &str, width: usize, min_padding: usize) -> String {
    let mut left_text = left.to_owned();
    let mut left_width = visible_width(&left_text);
    if left_width > width {
        left_text = truncate_to_width(&left_text, width, true);
        left_width = visible_width(&left_text);
    }
    let right_width = visible_width(right);
    if left_width + min_padding + right_width <= width {
        return format!(
            "{left_text}{}{right}",
            " ".repeat(width.saturating_sub(left_width + right_width))
        );
    }
    let available = width.saturating_sub(left_width + min_padding);
    if available == 0 {
        return left_text;
    }
    let cut = truncate_to_width(right, available, false);
    let cut_width = visible_width(&cut);
    format!(
        "{left_text}{}{cut}",
        " ".repeat(width.saturating_sub(left_width + cut_width))
    )
}
