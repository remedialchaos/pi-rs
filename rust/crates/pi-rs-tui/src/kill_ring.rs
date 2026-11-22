//! Emacs-style kill accumulation and yank cycling.

use std::collections::VecDeque;

#[derive(Debug, Default)]
pub struct KillRing {
    entries: VecDeque<String>,
}

impl KillRing {
    pub fn push(&mut self, mut text: String, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && let Some(last) = self.entries.back_mut() {
            if prepend {
                text.push_str(last);
                *last = text;
            } else {
                last.push_str(&text);
            }
        } else {
            self.entries.push_back(text);
        }
    }

    pub fn peek(&self) -> Option<&str> {
        self.entries.back().map(String::as_str)
    }

    pub fn rotate(&mut self) {
        if self.entries.len() > 1
            && let Some(last) = self.entries.pop_back()
        {
            self.entries.push_front(last);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
