use std::cell::Cell;

use unicode_segmentation::UnicodeSegmentation;

use crate::keybindings::get_keybindings;
use crate::keys::decode_kitty_printable;
use crate::kill_ring::KillRing;
use crate::tui::{
    CURSOR_MARKER, Component, Focusable, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult,
    TuiMouseEventType, render_fake_cursor,
};
use crate::undo_stack::UndoStack;
use crate::utils::{is_whitespace_char, slice_by_column, truncate_with_ellipsis, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward};

use super::TextStyleFn;

#[derive(Default)]
pub struct InputOptions {
    pub prompt: Option<String>,
    pub placeholder: String,
    pub placeholder_style: Option<TextStyleFn>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Kill,
    Yank,
    TypeWord,
}

pub type SubmitHandler = Box<dyn FnMut(&str)>;

pub struct Input {
    value: String,
    cursor: usize,
    prompt: String,
    placeholder: String,
    placeholder_style: Option<TextStyleFn>,
    rendered_start_column: Cell<usize>,
    focused: bool,
    paste: Option<String>,
    kill_ring: KillRing,
    last_action: Option<Action>,
    undo: UndoStack<(String, usize)>,
    pub on_submit: Option<SubmitHandler>,
    pub on_escape: Option<Box<dyn FnMut()>>,
}

impl Default for Input {
    fn default() -> Self {
        Self::new(InputOptions::default())
    }
}

impl Input {
    pub fn new(options: InputOptions) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: options.prompt.unwrap_or_else(|| "> ".into()),
            placeholder: options.placeholder,
            placeholder_style: options.placeholder_style,
            rendered_start_column: Cell::new(0),
            focused: false,
            paste: None,
            kill_ring: KillRing::default(),
            last_action: None,
            undo: UndoStack::default(),
            on_submit: None,
            on_escape: None,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_value(&mut self, value: String) {
        self.cursor = value.floor_char_boundary(self.cursor.min(value.len()));
        self.value = value;
        self.last_action = None;
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = self.value.floor_char_boundary(cursor.min(self.value.len()));
        self.last_action = None;
    }

    fn push_undo(&mut self) {
        self.undo.push((self.value.clone(), self.cursor));
    }

    fn insert(&mut self, value: &str) {
        self.value.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    fn type_text(&mut self, value: &str) {
        if value.chars().next().is_some_and(is_whitespace_char) && value.chars().count() == 1
            || self.last_action != Some(Action::TypeWord)
        {
            self.push_undo();
        }
        self.last_action = Some(Action::TypeWord);
        self.insert(value);
    }

    fn prev_char(&self) -> usize {
        self.value[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_char(&self) -> usize {
        self.cursor
            + self.value[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }

    fn delete(&mut self, target: usize, kill: bool) {
        if !kill {
            self.last_action = None;
        }
        if target == self.cursor {
            return;
        }
        self.push_undo();
        let start = target.min(self.cursor);
        let end = target.max(self.cursor);
        if kill {
            self.kill_ring.push(
                self.value[start..end].to_owned(),
                target < self.cursor,
                self.last_action == Some(Action::Kill),
            );
            self.last_action = Some(Action::Kill);
        }
        self.value.replace_range(start..end, "");
        self.cursor = start;
    }

    fn yank(&mut self, pop: bool) {
        if pop && (self.last_action != Some(Action::Yank) || self.kill_ring.len() <= 1)
            || self.kill_ring.is_empty()
        {
            return;
        }
        self.push_undo();
        if pop {
            let previous = self.kill_ring.peek().map_or(0, str::len);
            let start = self.cursor.saturating_sub(previous);
            self.value.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.kill_ring.rotate();
        }
        if let Some(value) = self.kill_ring.peek() {
            self.value.insert_str(self.cursor, value);
            self.cursor += value.len();
            self.last_action = Some(Action::Yank);
        }
    }
}

impl Focusable for Input {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Component for Input {
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn handle_input(&mut self, data: &str) {
        const START: &str = "\x1b[200~";
        const END: &str = "\x1b[201~";
        let mut cleaned = None;
        if data.contains(START) {
            self.paste = Some(String::new());
            cleaned = Some(data.replacen(START, "", 1));
        }
        let data = cleaned.as_deref().unwrap_or(data);
        if let Some(buffer) = &mut self.paste {
            buffer.push_str(data);
            if let Some(end) = buffer.find(END)
                && let Some(buffer) = self.paste.take()
            {
                self.push_undo();
                self.last_action = None;
                for text in buffer[..end].split_inclusive(['\r', '\n', '\t']) {
                    match text.as_bytes().last() {
                        Some(b'\r' | b'\n') => self.insert(&text[..text.len() - 1]),
                        Some(b'\t') => {
                            self.insert(&text[..text.len() - 1]);
                            self.insert("    ");
                        }
                        _ => self.insert(text),
                    }
                }
                let rest = &buffer[end + END.len()..];
                if !rest.is_empty() {
                    self.handle_input(rest);
                }
            }
            return;
        }
        let keys = get_keybindings();
        let is = |id: &str| keys.matches(data, id);
        if is("tui.select.cancel") {
            if let Some(on_escape) = &mut self.on_escape {
                on_escape();
            }
        } else if is("tui.editor.undo") {
            if let Some((value, cursor)) = self.undo.pop() {
                self.value = value;
                self.cursor = cursor;
                self.last_action = None;
            }
        } else if is("tui.input.submit") || data == "\n" {
            if let Some(on_submit) = &mut self.on_submit {
                on_submit(&self.value);
            }
        } else if is("tui.editor.deleteCharBackward") {
            self.delete(self.prev_char(), false);
        } else if is("tui.editor.deleteCharForward") {
            self.delete(self.next_char(), false);
        } else if is("tui.editor.deleteWordBackward") {
            self.delete(find_word_backward(&self.value, self.cursor, None), true);
        } else if is("tui.editor.deleteWordForward") {
            self.delete(find_word_forward(&self.value, self.cursor, None), true);
        } else if is("tui.editor.deleteToLineStart") {
            self.delete(0, true);
        } else if is("tui.editor.deleteToLineEnd") {
            self.delete(self.value.len(), true);
        } else if is("tui.editor.yank") {
            self.yank(false);
        } else if is("tui.editor.yankPop") {
            self.yank(true);
        } else if is("tui.editor.cursorLeft") {
            self.set_cursor(self.prev_char());
        } else if is("tui.editor.cursorRight") {
            self.set_cursor(self.next_char());
        } else if is("tui.editor.cursorLineStart") {
            self.set_cursor(0);
        } else if is("tui.editor.cursorLineEnd") {
            self.set_cursor(self.value.len());
        } else if is("tui.editor.cursorWordLeft") {
            if self.cursor != 0 {
                self.set_cursor(find_word_backward(&self.value, self.cursor, None));
            }
        } else if is("tui.editor.cursorWordRight") {
            if self.cursor < self.value.len() {
                self.set_cursor(find_word_forward(&self.value, self.cursor, None));
            }
        } else if let Some(printable) = decode_kitty_printable(data) {
            self.type_text(&printable);
        } else if !data.chars().any(char::is_control) {
            self.type_text(data);
        }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if event.event_type != TuiMouseEventType::Press
            || event.button != TuiMouseButton::Left
            || event.y != 0
        {
            return None;
        }
        let target = self.rendered_start_column.get() + event.x.saturating_sub(2);
        let mut column = 0;
        let mut cursor = self.value.len();
        for (index, grapheme) in self.value.grapheme_indices(true) {
            column += visible_width(grapheme);
            if target < column {
                cursor = index;
                break;
            }
        }
        self.set_cursor(cursor);
        Some(TuiMouseEventResult {
            handled: true,
            focus: true,
            ..Default::default()
        })
    }

    fn render(&self, width: usize) -> Vec<String> {
        use std::borrow::Cow;
        let available = width.saturating_sub(visible_width(&self.prompt));
        self.rendered_start_column.set(0);
        if available == 0 {
            return vec![truncate_with_ellipsis(&self.prompt, width, "", false)];
        }
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let total = visible_width(&self.value);
        let mut position = self.cursor;
        let placeholder = self.value.is_empty() && !self.placeholder.is_empty();
        let text = if placeholder {
            Cow::Owned(truncate_with_ellipsis(
                &self.placeholder,
                available,
                "",
                false,
            ))
        } else if total < available {
            Cow::Borrowed(self.value.as_str())
        } else {
            let window = available - usize::from(self.cursor == self.value.len());
            let column = visible_width(&self.value[..self.cursor]);
            let start = column
                .saturating_sub(window / 2)
                .min(total.saturating_sub(window));
            self.rendered_start_column.set(start);
            position =
                slice_by_column(&self.value, start, column.saturating_sub(start), true).len();
            Cow::Owned(slice_by_column(&self.value, start, window, true))
        };
        let position = text.floor_char_boundary(position.min(text.len()));
        let (before, rest) = text.split_at(position);
        let (at, after) = match rest.graphemes(true).next() {
            Some(grapheme) => (grapheme, &rest[grapheme.len()..]),
            None => (" ", ""),
        };
        let mut line = String::with_capacity(self.prompt.len() + text.len() + marker.len() + 20);
        line.push_str(&self.prompt);
        line.push_str(before);
        line.push_str(marker);
        if placeholder && let Some(style) = &self.placeholder_style {
            line.push_str(&render_fake_cursor(&style(at)));
            line.push_str(&style(after));
        } else {
            line.push_str(&render_fake_cursor(at));
            line.push_str(after);
        }
        line.extend(std::iter::repeat_n(
            ' ',
            width.saturating_sub(visible_width(&line)),
        ));
        vec![line]
    }
}
