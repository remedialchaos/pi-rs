use crate::components::selectors::SelectorContext;
use pi_rs_tui::{CURSOR_MARKER, Component, Focusable, decode_kitty_printable, truncate_to_width};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
const MAX: usize = 100_000;

pub struct SecretInput {
    focused: bool,
    on_submit: Box<dyn Fn(String) + Send + Sync>,
    value: Vec<char>,
    pasting: bool,
    ctx: SelectorContext,
}

impl SecretInput {
    pub fn new(ctx: SelectorContext) -> Self {
        Self {
            focused: false,
            on_submit: Box::new(|_| {}),
            value: Vec::new(),
            pasting: false,
            ctx,
        }
    }

    pub fn length(&self) -> usize {
        self.value.len()
    }

    pub fn clear(&mut self) {
        self.value.clear();
    }

    fn add(&mut self, text: &str) {
        for ch in text.chars() {
            if self.value.len() < MAX && !ch.is_control() {
                self.value.push(ch);
            }
        }
    }

    pub fn handle_input(&mut self, data: &str) {
        let mut data = data.to_string();
        if data.contains(PASTE_START) {
            self.pasting = true;
            data = data.replace(PASTE_START, "");
        }
        if self.pasting {
            if let Some(end) = data.find(PASTE_END) {
                self.add(&data[..end]);
                self.pasting = false;
            } else {
                self.add(&data);
            }
            return;
        }

        let keys = &self.ctx.keys;
        if keys.matches(&data, "tui.input.submit") {
            let value: String = self.value.iter().collect();
            self.clear();
            (self.on_submit)(value);
        } else if keys.matches(&data, "tui.editor.deleteCharBackward") {
            self.value.pop();
        } else if keys.matches(&data, "tui.editor.deleteToLineStart")
            || keys.matches(&data, "tui.editor.deleteWordBackward")
        {
            self.clear();
        } else {
            let printable = decode_kitty_printable(&data).or_else(|| {
                if data.chars().count() == 1 && !data.chars().any(char::is_control) {
                    Some(data)
                } else {
                    None
                }
            });
            if let Some(p) = printable {
                self.add(&p);
            }
        }
    }
}

impl Component for SecretInput {
    fn render(&self, width: usize) -> Vec<String> {
        let dots = "•".repeat(self.value.len().min(width.saturating_sub(6)));
        let cursor = if self.focused {
            format!("{}\x1b[7m \x1b[27m", CURSOR_MARKER)
        } else {
            String::new()
        };
        vec![truncate_to_width(
            &format!("> {}{}", dots, cursor),
            width,
            true,
        )]
    }

    fn handle_input(&mut self, data: &str) {
        self.handle_input(data);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for SecretInput {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
