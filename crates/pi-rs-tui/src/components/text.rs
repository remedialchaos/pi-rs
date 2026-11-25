use std::cell::RefCell;

use crate::tui::Component;
use crate::utils::{
    apply_background_to_line, is_whitespace_char, visible_width, wrap_text_with_ansi,
};

use super::TextStyleFn;

pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    background: Option<TextStyleFn>,
    cache: RefCell<Option<(usize, Vec<String>)>>,
}

impl Default for Text {
    fn default() -> Self {
        Self::new("", 1, 1, None)
    }
}

impl Text {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        background: Option<TextStyleFn>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            background,
            cache: RefCell::new(None),
        }
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.text != text {
            self.text = text;
            self.invalidate();
        }
    }
    pub fn set_custom_bg_fn(&mut self, background: Option<TextStyleFn>) {
        self.background = background;
        self.invalidate();
    }
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
        self.invalidate();
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        if self.text.chars().all(is_whitespace_char) {
            return Vec::new();
        }
        if let Some((_, lines)) = self.cache.borrow().as_ref().filter(|(w, _)| *w == width) {
            return lines.clone();
        }
        let text = if self.text.contains('\t') {
            std::borrow::Cow::Owned(self.text.replace('\t', "   "))
        } else {
            std::borrow::Cow::Borrowed(self.text.as_str())
        };
        let padding = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding * 2).max(1);
        let wrapped = wrap_text_with_ansi(&text, content_width);
        let mut lines = Vec::with_capacity(wrapped.len() + self.padding_y * 2);
        let apply = |line: String| match &self.background {
            Some(background) => apply_background_to_line(&line, width, |s| background(s)),
            None => line,
        };
        for _ in 0..self.padding_y {
            lines.push(apply(" ".repeat(width)));
        }
        for line in wrapped {
            let trailing = width
                .saturating_sub(padding + visible_width(&line))
                .max(padding);
            let mut padded = String::with_capacity(line.len() + padding + trailing);
            padded.extend(std::iter::repeat_n(' ', padding));
            padded.push_str(&line);
            padded.extend(std::iter::repeat_n(' ', trailing));
            lines.push(apply(padded));
        }
        for i in 0..self.padding_y {
            lines.push(lines[i].clone());
        }
        *self.cache.borrow_mut() = Some((width, lines.clone()));
        lines
    }
    fn invalidate(&mut self) {
        self.cache.get_mut().take();
    }
}
