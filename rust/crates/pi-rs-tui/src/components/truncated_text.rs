use crate::tui::Component;
use crate::utils::{truncate_with_ellipsis, visible_width};

pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
        }
    }
}

impl Component for TruncatedText {
    fn render(&self, width: usize) -> Vec<String> {
        let available = width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let text = self
            .text
            .split_once('\n')
            .map_or(self.text.as_str(), |(text, _)| text);
        let text = truncate_with_ellipsis(text, available, "…", false);
        let tail = width
            .saturating_sub(self.padding_x + visible_width(&text))
            .max(self.padding_x);
        let mut line = String::with_capacity(self.padding_x + text.len() + tail);
        line.extend(std::iter::repeat_n(' ', self.padding_x));
        line.push_str(&text);
        line.extend(std::iter::repeat_n(' ', tail));
        let mut lines = vec![" ".repeat(width); self.padding_y * 2 + 1];
        lines[self.padding_y] = line;
        lines
    }
}
