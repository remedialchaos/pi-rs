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

pub type PreviewHintFn = Box<dyn Fn(usize) -> String + Send + Sync>;

pub struct VisualLinePreview {
    text: String,
    max_lines: usize,
    width: usize,
    padding_x: usize,
    keep: Keep,
    format_hint: Option<PreviewHintFn>,
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
            keep: Keep::End,
            format_hint: None,
            cached_lines: None,
            cached_width: None,
            focused: false,
        }
    }

    pub fn with_keep(mut self, keep: Keep) -> Self {
        self.keep = keep;
        self.cached_lines = None;
        self.cached_width = None;
        self
    }

    pub fn with_hint(
        mut self,
        format_hint: impl Fn(usize) -> String + Send + Sync + 'static,
    ) -> Self {
        self.format_hint = Some(Box::new(format_hint));
        self.cached_lines = None;
        self.cached_width = None;
        self
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        let width = if width == 0 { self.width } else { width };
        if self.cached_width == Some(width)
            && let Some(cached) = &self.cached_lines
        {
            return cached.clone();
        }

        let mut truncated =
            truncate_to_visual_lines(&self.text, self.max_lines, width, self.padding_x, self.keep);
        if truncated.skipped_count > 0
            && let Some(format_hint) = &self.format_hint
        {
            let hint = format_hint(truncated.skipped_count);
            if !hint.is_empty() {
                truncated.visual_lines.push(hint);
            }
        }
        truncated.visual_lines
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
