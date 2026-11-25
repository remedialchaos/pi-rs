use pi_rs_tui::{Component, Focusable, Text};

#[derive(Debug, Clone)]
pub struct VisualTruncateResult {
    pub visual_lines: Vec<String>,
    pub skipped_count: usize,
}

pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
    keep: Keep,
) -> VisualTruncateResult {
    if text.is_empty() {
        return VisualTruncateResult {
            visual_lines: Vec::new(),
            skipped_count: 0,
        };
    }

    let temp_text = Text::new(text.to_string(), padding_x, 0, None);
    let all_visual_lines = temp_text.render(width);

    if all_visual_lines.len() <= max_visual_lines {
        return VisualTruncateResult {
            visual_lines: all_visual_lines,
            skipped_count: 0,
        };
    }

    let skipped_count = all_visual_lines.len() - max_visual_lines;
    let truncated_lines = match keep {
        Keep::Start => all_visual_lines
            .into_iter()
            .take(max_visual_lines)
            .collect(),
        Keep::End => all_visual_lines
            .into_iter()
            .rev()
            .take(max_visual_lines)
            .rev()
            .collect(),
    };

    VisualTruncateResult {
        visual_lines: truncated_lines,
        skipped_count,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Start,
    End,
}

#[derive(Debug, Clone)]
pub struct VisualLinePreviewOptions {
    pub max_lines: usize,
    pub width: usize,
    pub padding_x: usize,
}

pub struct VisualLinePreview {
    text: String,
    max_lines: usize,
    width: usize,
    padding_x: usize,
    cached_lines: Option<Vec<String>>,
    cached_width: Option<usize>,
    focused: bool,
}

impl VisualLinePreview {
    pub fn new(text: String, max_lines: usize, width: usize, padding_x: usize) -> Self {
        Self {
            text,
            max_lines,
            width,
            padding_x,
            cached_lines: None,
            cached_width: None,
            focused: false,
        }
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        let width = if width == 0 { self.width } else { width };
        if self.cached_width == Some(width) {
            return self.cached_lines.clone().unwrap_or_default();
        }

        let temp_text = Text::new(self.text.clone(), self.padding_x, 0, None);
        let lines = temp_text.render(width);

        let result = if lines.len() <= self.max_lines {
            lines
        } else {
            lines.into_iter().rev().take(self.max_lines).rev().collect()
        };

        // Can't cache in &self, so we just return
        result
    }
}

impl Component for VisualLinePreview {
    fn render(&self, width: usize) -> Vec<String> {
        self.render(width)
    }

    fn invalidate(&mut self) {
        self.cached_lines = None;
        self.cached_width = None;
    }
}

impl Focusable for VisualLinePreview {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
