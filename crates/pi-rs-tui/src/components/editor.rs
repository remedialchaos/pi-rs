//! Multiline prompt editing, including atomic paste markers.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::editor_component::{
    EditorAppearance, EditorAutocomplete, EditorBorder, EditorCallback, EditorComponent,
    EditorHistory, EditorInsertion,
};
use crate::keybindings::get_keybindings;
use crate::keys::{decode_printable_key, matches_key_data};
use crate::kill_ring::KillRing;
use crate::tui::{
    CURSOR_MARKER, Component, Focusable, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult,
    TuiMouseEventType, render_fake_cursor,
};
use crate::undo_stack::UndoStack;
use crate::utils::{is_cjk, is_whitespace_char, truncate_with_ellipsis, visible_width};
use crate::word_navigation::{
    WordNavigationOptions, WordSegment, find_word_backward, find_word_forward, segment_words,
};

use super::{TextStyleFn, select_list::SelectListTheme};

mod completion;
use completion::Completion;

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: The pattern is fixed and validated by the editor tests.
    Regex::new(r"\[paste #([0-9]+)( (\+[0-9]+ lines|[0-9]+ chars))?\]")
        .expect("valid paste marker regex")
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk<'a> {
    pub text: &'a str,
    pub start_index: usize,
    pub end_index: usize,
}

pub fn word_wrap_line(line: &str, width: usize) -> Vec<TextChunk<'_>> {
    wrap(line, width, line.grapheme_indices(true))
}

fn wrap<'a>(
    line: &'a str,
    width: usize,
    segments: impl Iterator<Item = (usize, &'a str)>,
) -> Vec<TextChunk<'a>> {
    let chunk = |a, b| TextChunk {
        text: &line[a..b],
        start_index: a,
        end_index: b,
    };
    if width == 0 {
        return vec![chunk(0, 0)];
    }
    if visible_width(line) <= width {
        return vec![chunk(0, line.len())];
    }
    let whitespace = |s: &str| s.chars().all(is_whitespace_char);
    let mut parts = Vec::new();
    let mut segments = segments.peekable();
    let (mut start, mut cells, mut opportunity) = (0, 0, None);
    while let Some((at, text)) = segments.next() {
        let size = visible_width(text);
        if cells + size > width {
            if let Some((index, before)) =
                opportunity.filter(|&(_, before)| cells - before + size <= width)
            {
                parts.push(chunk(start, index));
                start = index;
                cells -= before;
            } else if at > start {
                parts.push(chunk(start, at));
                start = at;
                cells = 0;
            }
            opportunity = None;
        }
        if size > width && text.graphemes(true).nth(1).is_some() {
            let mut inner = word_wrap_line(text, width);
            if let Some(last) = inner.pop() {
                parts.extend(
                    inner
                        .into_iter()
                        .map(|part| chunk(at + part.start_index, at + part.end_index)),
                );
                start = at + last.start_index;
                cells = visible_width(last.text);
            }
            opportunity = None;
            continue;
        }
        cells += size;
        if let Some(&(next_index, next)) = segments.peek()
            && !whitespace(next)
            && (whitespace(text) || is_cjk(text) || is_cjk(next))
        {
            opportunity = Some((next_index, cells));
        }
    }
    parts.push(chunk(start, line.len()));
    parts
}

#[derive(Clone, Default)]
struct State {
    text: String,
    cursor: usize,
}

#[derive(Clone, Default)]
struct Pastes {
    slots: BTreeMap<usize, Rc<str>>,
    counter: usize,
}

impl Pastes {
    fn markers<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (Range<usize>, usize)> + 'a {
        let text = if self.slots.is_empty() || !text.contains("[paste #") {
            ""
        } else {
            text
        };
        MARKER.captures_iter(text).filter_map(|hit| {
            let id = hit.get(1)?.as_str().parse().ok()?;
            self.slots
                .contains_key(&id)
                .then_some((hit.get(0)?.range(), id))
        })
    }

    fn graphemes<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, &'a str)> + 'a {
        let mut markers = self.markers(text).peekable();
        let mut skip_until = 0;
        text.grapheme_indices(true).filter_map(move |(i, part)| {
            if i < skip_until {
                return None;
            }
            if markers.peek().is_some_and(|(range, _)| range.start == i)
                && let Some((marker, _)) = markers.next()
            {
                skip_until = marker.end;
                return Some((i, &text[marker]));
            }
            Some((i, part))
        })
    }

    fn words<'a>(&self, text: &'a str) -> Vec<WordSegment<'a>> {
        let mut out = Vec::new();
        let mut done = 0;
        for (marker, _) in self.markers(text) {
            out.extend(segment_words(&text[done..marker.start]));
            done = marker.end;
            out.push(WordSegment {
                text: &text[marker],
                word_like: false,
            });
        }
        out.extend(segment_words(&text[done..]));
        out
    }

    fn expand<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut markers = self.markers(text).peekable();
        if markers.peek().is_none() {
            return Cow::Borrowed(text);
        }
        let mut out = String::new();
        let mut done = 0;
        for (marker, id) in markers {
            out.push_str(&text[done..marker.start]);
            out.push_str(&self.slots[&id]);
            done = marker.end;
        }
        out.push_str(&text[done..]);
        Cow::Owned(out)
    }

    fn remove(&mut self, state: &mut State, id: usize) {
        if self.slots.remove(&id).is_none() {
            return;
        }
        let newer = self.slots.split_off(&(id + 1));
        self.slots
            .extend(newer.into_iter().map(|(id, value)| (id - 1, value)));
        self.counter = self.counter.saturating_sub(1);
        let mut done = 0;
        let mut out = String::new();
        let mut cursor = state.cursor;
        for hit in MARKER.captures_iter(&state.text) {
            let Some(full) = hit.get(0) else { continue };
            let Some(num) = hit.get(1).and_then(|n| n.as_str().parse::<usize>().ok()) else {
                continue;
            };
            if num <= id {
                continue;
            }
            out.push_str(&state.text[done..full.start()]);
            let start = out.len();
            let suffix = hit.get(2).map_or("", |s| s.as_str());
            use std::fmt::Write;
            let _ = write!(out, "[paste #{}{suffix}]", num - 1);
            done = full.end();
            if done <= state.cursor {
                cursor = cursor
                    .saturating_add(out.len() - start)
                    .saturating_sub(full.len());
            }
        }
        if done != 0 {
            out.push_str(&state.text[done..]);
            state.text = out;
            state.cursor = cursor;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Word,
    Kill,
    Yank,
}

#[derive(Clone, Copy)]
enum Move {
    Grapheme,
    Word,
    Edge,
}

pub struct EditorTheme {
    pub border_color: TextStyleFn,
    pub select_list: SelectListTheme,
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self {
            border_color: Rc::new(str::to_owned),
            select_list: SelectListTheme::default(),
        }
    }
}

pub struct EditorOptions {
    pub padding_x: usize,
    pub autocomplete_max_visible: usize,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self {
            padding_x: 0,
            autocomplete_max_visible: 5,
        }
    }
}

pub struct Editor {
    state: State,
    pastes: Pastes,
    undo: UndoStack<(State, Pastes)>,
    paste_buffer: Option<String>,
    kill_ring: KillRing,
    action: Option<Action>,
    history: VecDeque<String>,
    history_index: Option<usize>,
    history_draft: Option<State>,
    jump_forward: Option<bool>,
    preferred_col: Option<usize>,
    snapped: Option<usize>,
    theme: EditorTheme,
    border: Option<EditorBorder>,
    padding_x: usize,
    rows: Box<dyn Fn() -> usize>,
    request_render: Rc<dyn Fn()>,
    focused: bool,
    rendered_width: Cell<usize>,
    scroll_offset: Cell<usize>,
    rendered_count: Cell<usize>,
    menu_height: Cell<usize>,
    completion: Completion,
    pub disable_submit: bool,
    pub on_submit: Option<EditorCallback>,
    pub on_change: Option<EditorCallback>,
}

impl Editor {
    pub fn new(
        rows: Box<dyn Fn() -> usize>,
        request_render: Rc<dyn Fn()>,
        theme: EditorTheme,
        options: EditorOptions,
    ) -> Self {
        Self {
            state: State::default(),
            pastes: Pastes::default(),
            undo: UndoStack::default(),
            paste_buffer: None,
            kill_ring: KillRing::default(),
            action: None,
            history: VecDeque::new(),
            history_index: None,
            history_draft: None,
            jump_forward: None,
            preferred_col: None,
            snapped: None,
            theme,
            border: None,
            padding_x: options.padding_x,
            rows,
            request_render,
            focused: false,
            rendered_width: Cell::new(80),
            scroll_offset: Cell::new(0),
            rendered_count: Cell::new(1),
            menu_height: Cell::new(0),
            completion: Completion::new(options.autocomplete_max_visible.clamp(3, 20)),
            disable_submit: false,
            on_submit: None,
            on_change: None,
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.state.text.split('\n')
    }

    pub fn cursor(&self) -> (usize, usize) {
        let before = &self.state.text[..self.state.cursor];
        (
            before.bytes().filter(|&c| c == b'\n').count(),
            self.state.cursor - self.line_start(),
        )
    }

    pub fn padding_x(&self) -> usize {
        self.padding_x
    }
    pub fn autocomplete_max_visible(&self) -> usize {
        self.completion.max
    }
    pub fn is_showing_autocomplete(&self) -> bool {
        self.completion.menu.is_some()
    }

    fn line_start(&self) -> usize {
        self.state.text[..self.state.cursor]
            .rfind('\n')
            .map_or(0, |i| i + 1)
    }
    fn line_end(&self) -> usize {
        self.state.text[self.state.cursor..]
            .find('\n')
            .map_or(self.state.text.len(), |i| self.state.cursor + i)
    }

    fn set_cursor(&mut self, cursor: usize) {
        self.state.cursor = cursor;
        self.preferred_col = None;
        self.snapped = None;
    }

    fn changed(&mut self) {
        if let Some(callback) = &mut self.on_change {
            callback(&self.state.text);
        }
    }

    fn remember(&mut self) {
        self.undo.push((self.state.clone(), self.pastes.clone()));
    }

    fn leave_history(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn navigate_history(&mut self, older: bool) {
        self.action = None;
        let index = if older {
            Some(self.history_index.map_or(0, |i| i + 1))
        } else if let Some(i) = self.history_index {
            i.checked_sub(1)
        } else {
            return;
        };
        if index.is_some_and(|i| i >= self.history.len()) {
            return;
        }
        if self.history_index.is_none() {
            self.remember();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = index;
        if let Some(index) = index {
            self.state.text.clone_from(&self.history[index]);
            self.set_cursor(if older { 0 } else { self.state.text.len() });
        } else {
            self.state = self.history_draft.take().unwrap_or_default();
            self.set_cursor(self.state.cursor);
        }
        self.scroll_offset.set(0);
        self.changed();
    }

    fn insert(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.state.text.insert_str(self.state.cursor, value);
        self.set_cursor(self.state.cursor + value.len());
        self.changed();
    }

    fn type_text(&mut self, value: &str) {
        self.leave_history();
        if self.action != Some(Action::Word)
            || value.chars().next().is_some_and(is_whitespace_char) && value.chars().count() == 1
        {
            self.remember();
        }
        self.action = Some(Action::Word);
        self.insert(value);
        if self.is_showing_autocomplete()
            || value.chars().any(|c| {
                self.completion.triggers.contains(&c)
                    || c.is_ascii_alphanumeric()
                    || ".-_".contains(c)
            })
            || is_cjk(value)
            || value == "/"
        {
            self.refresh_completion(false);
        }
    }

    fn paste(&mut self, raw: &str) {
        static CTRL: LazyLock<Regex> = LazyLock::new(|| {
            // INFALLIBLE: The expression is a fixed Kitty Ctrl-key report.
            Regex::new(r"\x1b\[([0-9]+);5u").expect("valid paste control regex")
        });
        self.completion.cancel();
        self.leave_history();
        self.action = None;
        self.remember();
        let decoded = CTRL.replace_all(raw, |hit: &regex::Captures<'_>| {
            hit.get(1)
                .and_then(|n| n.as_str().parse::<u8>().ok())
                .filter(u8::is_ascii_alphabetic)
                .map_or_else(|| hit[0].to_owned(), |c| char::from(c & 31).to_string())
        });
        let mut value = normalize(&decoded);
        value.retain(|c| c == '\n' || c >= ' ');
        if value.starts_with(['/', '~', '.'])
            && self
                .state
                .text
                .as_bytes()
                .get(self.state.cursor.wrapping_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            value.insert(0, ' ');
        }
        let lines = value.split('\n').count();
        let chars = value.encode_utf16().count();
        if lines > 10 || chars > 1000 {
            self.pastes.counter += 1;
            let id = self.pastes.counter;
            self.pastes.slots.insert(id, Rc::from(value));
            self.insert(&if lines > 10 {
                format!("[paste #{id} +{lines} lines]")
            } else {
                format!("[paste #{id} {chars} chars]")
            });
        } else {
            self.insert(&value);
        }
    }

    fn delete(&mut self, target: usize, kill: bool, renumber: bool) {
        self.leave_history();
        if !kill {
            self.action = None;
        }
        let cursor = self.state.cursor;
        if target != cursor {
            self.remember();
            let range = target.min(cursor)..target.max(cursor);
            let id = if renumber {
                self.pastes
                    .markers(&self.state.text[range.clone()])
                    .next()
                    .map(|(_, id)| id)
            } else {
                None
            };
            if kill {
                self.kill_ring.push(
                    self.state.text[range.clone()].to_owned(),
                    target < cursor,
                    self.action == Some(Action::Kill),
                );
                self.action = Some(Action::Kill);
            }
            self.state.text.replace_range(range.clone(), "");
            self.set_cursor(range.start);
            if let Some(id) = id {
                self.pastes.remove(&mut self.state, id);
            }
        }
        self.changed();
        if !kill {
            self.refresh_completion(false);
        }
    }

    fn target(&self, forward: bool, movement: Move, cross_line: bool) -> usize {
        let cursor = self.state.cursor;
        let start = self.line_start();
        let end = self.line_end();
        let line = &self.state.text[start..end];
        let col = cursor - start;
        if cross_line {
            if forward && cursor == end {
                return (end + 1).min(self.state.text.len());
            }
            if !forward && cursor == start {
                return start.saturating_sub(1);
            }
        }
        match movement {
            Move::Edge => {
                if forward {
                    end
                } else {
                    start
                }
            }
            Move::Grapheme if forward => {
                cursor
                    + self
                        .pastes
                        .graphemes(&line[col..])
                        .next()
                        .map_or(0, |(_, g)| g.len())
            }
            Move::Grapheme => {
                start
                    + self
                        .pastes
                        .graphemes(&line[..col])
                        .last()
                        .map_or(0, |(i, _)| i)
            }
            Move::Word => {
                let segment: &crate::word_navigation::SegmentWords<'_> =
                    &|text| self.pastes.words(text);
                let options = WordNavigationOptions {
                    segment: (!self.pastes.slots.is_empty()).then_some(segment),
                    is_atomic_segment: Some(&|text| {
                        text.starts_with("[paste #") && text.ends_with(']')
                    }),
                };
                let find = if forward {
                    find_word_forward
                } else {
                    find_word_backward
                };
                start + find(line, col, Some(&options))
            }
        }
    }

    fn move_horizontal(&mut self, forward: bool, movement: Move) {
        self.action = None;
        let target = self.target(forward, movement, !matches!(movement, Move::Edge));
        if forward && target == self.state.cursor && matches!(movement, Move::Grapheme) {
            let ranges = self.visual_lines(self.rendered_width.get());
            self.preferred_col = ranges
                .get(Self::visual_at(&ranges, target))
                .map(|line| units(&self.state.text[line.start..target]));
        } else {
            self.set_cursor(target);
        }
        if self.is_showing_autocomplete() {
            self.refresh_completion(false);
        }
    }

    fn yank(&mut self, rotate: bool) {
        if self.kill_ring.is_empty()
            || rotate && (self.action != Some(Action::Yank) || self.kill_ring.len() < 2)
        {
            return;
        }
        self.remember();
        self.leave_history();
        if rotate {
            let len = self.kill_ring.peek().map_or(0, str::len);
            let cursor = self.state.cursor;
            let start = self
                .state
                .text
                .floor_char_boundary(cursor.saturating_sub(len));
            self.state.text.replace_range(start..cursor, "");
            self.set_cursor(start);
            self.kill_ring.rotate();
        }
        if let Some(text) = self.kill_ring.peek() {
            self.state.text.insert_str(self.state.cursor, text);
            self.set_cursor(self.state.cursor + text.len());
            self.action = Some(Action::Yank);
            self.changed();
        }
    }

    fn submit(&mut self) {
        self.completion.cancel();
        let text = self
            .pastes
            .expand(&self.state.text)
            .trim_matches(is_whitespace_char)
            .to_owned();
        self.state = State::default();
        self.pastes = Pastes::default();
        self.undo.clear();
        self.leave_history();
        self.action = None;
        self.scroll_offset.set(0);
        self.changed();
        if let Some(callback) = &mut self.on_submit {
            callback(&text);
        }
    }

    fn newline(&mut self) {
        self.completion.cancel();
        self.leave_history();
        self.action = None;
        self.remember();
        self.insert("\n");
    }

    fn visual_lines(&self, width: usize) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for line in self.lines() {
            if visible_width(line) <= width {
                ranges.push(start..start + line.len());
            } else {
                ranges.extend(
                    wrap(line, width, self.pastes.graphemes(line))
                        .into_iter()
                        .map(|part| start + part.start_index..start + part.end_index),
                );
            }
            start += line.len() + 1;
        }
        ranges
    }

    fn visual_at(ranges: &[Range<usize>], cursor: usize) -> usize {
        ranges
            .partition_point(|r| r.start <= cursor)
            .saturating_sub(1)
    }

    fn max_visual_col(&self, ranges: &[Range<usize>], index: usize) -> usize {
        let range = &ranges[index];
        let count = units(&self.state.text[range.clone()]);
        count.saturating_sub(usize::from(
            ranges
                .get(index + 1)
                .is_some_and(|next| range.end == next.start),
        ))
    }

    fn vertical(&mut self, delta: isize, page: bool) {
        self.action = None;
        let ranges = self.visual_lines(self.rendered_width.get());
        let visible = Self::visual_at(&ranges, self.state.cursor);
        let source = Self::visual_at(&ranges, self.snapped.unwrap_or(self.state.cursor));
        let current = units(
            &self.state.text[ranges[source].start..self.snapped.unwrap_or(self.state.cursor)],
        );
        if !page {
            let edge = if delta < 0 {
                visible == 0
            } else {
                visible + 1 == ranges.len()
            };
            let history = if delta < 0 {
                self.history_index.is_some() || self.state.cursor == self.line_start()
            } else {
                self.history_index.is_some()
            };
            if edge && history {
                self.navigate_history(delta < 0);
                return;
            }
            if edge {
                self.move_horizontal(delta > 0, Move::Edge);
                return;
            }
        }
        let mut target = visible.saturating_add_signed(delta).min(ranges.len() - 1);
        loop {
            let maximum = self.max_visual_col(&ranges, target);
            let desired = if current < self.max_visual_col(&ranges, source) {
                current
            } else {
                self.preferred_col.unwrap_or(current)
            };
            let preferred = (desired > maximum).then_some(desired);
            let range = &ranges[target];
            let proposed =
                range.start + byte_at_units(&self.state.text[range.clone()], desired.min(maximum));
            let mut start = proposed;
            let line_start = self.state.text[..proposed].rfind('\n').map_or(0, |i| i + 1);
            let end = self.state.text[proposed..]
                .find('\n')
                .map_or(self.state.text.len(), |i| i + proposed);
            let mut skip_to = None;
            for (index, piece) in self.pastes.graphemes(&self.state.text[line_start..end]) {
                let index = line_start + index;
                if index > proposed {
                    break;
                }
                if proposed < index + piece.len() {
                    start = index;
                    if delta > 0 && index < range.start {
                        skip_to = ranges
                            .iter()
                            .position(|line| line.start >= index + piece.len());
                    }
                    break;
                }
            }
            if let Some(skip) = skip_to.filter(|&skip| skip > target) {
                target = skip;
                continue;
            }
            self.state.cursor = start;
            self.preferred_col = preferred;
            self.snapped = (start != proposed).then_some(proposed);
            break;
        }
        if self.is_showing_autocomplete() {
            self.refresh_completion(false);
        }
    }

    fn jump(&mut self, text: &str, forward: bool) {
        if text.is_empty() {
            return;
        }
        let cursor = self.state.cursor;
        let found = if forward {
            let start = self
                .state
                .text
                .ceil_char_boundary((cursor + 1).min(self.state.text.len()));
            self.state.text[start..]
                .find(text)
                .map(|index| start + index)
        } else {
            self.state.text[..cursor].rfind(text)
        };
        if let Some(found) = found {
            self.set_cursor(found);
            self.action = None;
        }
    }

    fn page(&self) -> usize {
        ((self.rows)().saturating_mul(3) / 10).max(5)
    }
}

fn units(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

fn byte_at_units(text: &str, mut units: usize) -> usize {
    if text.is_ascii() {
        return units.min(text.len());
    }
    for (index, ch) in text.char_indices() {
        if units < ch.len_utf16() {
            return index;
        }
        units -= ch.len_utf16();
    }
    text.len()
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

fn border(direction: char, hidden: usize, width: usize) -> String {
    if hidden == 0 {
        return "─".repeat(width);
    }
    let label = format!(" {direction} {hidden} more ");
    let size = visible_width(&label);
    if width >= size + 2 {
        let left = (width - size) / 2;
        return format!(
            "{}{label}{}",
            "─".repeat(left),
            "─".repeat(width - size - left)
        );
    }
    truncate_with_ellipsis(&format!("───{label}"), width, &"..."[..width.min(3)], true)
}

impl EditorComponent for Editor {
    fn text(&self) -> &str {
        &self.state.text
    }
    fn set_text(&mut self, text: &str) {
        self.completion.cancel();
        self.leave_history();
        self.action = None;
        let text = normalize(text);
        if self.state.text != text {
            self.remember();
        }
        self.pastes = Pastes::default();
        self.state.text = text;
        self.set_cursor(self.state.text.len());
        self.scroll_offset.set(0);
        self.changed();
    }
    fn expanded_text(&self) -> Cow<'_, str> {
        self.pastes.expand(&self.state.text)
    }
    fn set_on_submit(&mut self, callback: Option<EditorCallback>) {
        self.on_submit = callback;
    }
    fn set_on_change(&mut self, callback: Option<EditorCallback>) {
        self.on_change = callback;
    }
}

impl EditorHistory for Editor {
    fn add_to_history(&mut self, text: &str) {
        let text = text.trim_matches(is_whitespace_char);
        if text.is_empty() {
            return;
        }
        if let Some(index) = self.history.iter().position(|previous| previous == text) {
            self.history.remove(index);
        }
        self.history.push_front(text.to_owned());
        self.history.truncate(100);
    }
}

impl EditorInsertion for Editor {
    fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.completion.cancel();
        self.remember();
        self.leave_history();
        self.action = None;
        self.insert(&normalize(text));
    }
}

impl EditorAppearance for Editor {
    fn set_padding_x(&mut self, padding: usize) {
        if self.padding_x != padding {
            self.padding_x = padding;
            (self.request_render)();
        }
    }
    fn set_border_color(&mut self, color: EditorBorder) {
        self.border = Some(color);
    }
}

impl EditorAutocomplete for Editor {
    fn set_autocomplete_provider(
        &mut self,
        provider: Box<dyn crate::autocomplete::AutocompleteProvider>,
    ) {
        self.completion.set_provider(provider.into());
    }
    fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        let max = max_visible.clamp(3, 20);
        if self.completion.max != max {
            self.completion.max = max;
            (self.request_render)();
        }
    }
}

impl Focusable for Editor {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Component for Editor {
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
    fn captures_viewport_input(&self) -> bool {
        self.is_showing_autocomplete()
    }
    fn tick(&mut self, now: Instant) -> bool {
        self.tick_completion(now)
    }

    fn render(&self, width: usize) -> Vec<String> {
        let padding = self.padding_x.min(width.saturating_sub(1) / 2);
        let content = width.saturating_sub(padding * 2).max(1);
        let wrap_width = content.saturating_sub(usize::from(padding == 0)).max(1);
        self.rendered_width.set(wrap_width);
        let ranges = self.visual_lines(wrap_width);
        let cursor = Self::visual_at(&ranges, self.state.cursor);
        let page = self.page();
        let first = self
            .scroll_offset
            .get()
            .max((cursor + 1).saturating_sub(page))
            .min(cursor)
            .min(ranges.len().saturating_sub(page));
        let last = (first + page).min(ranges.len());
        self.scroll_offset.set(first);
        self.rendered_count.set(last - first);
        let style = |text: &str| match &self.border {
            Some(border) => border(text),
            None => (self.theme.border_color)(text),
        };
        let mut lines = Vec::with_capacity(last - first + 2);
        lines.push(style(&border('↑', first, width)));
        for (row, range) in ranges.iter().enumerate().take(last).skip(first) {
            let mut line = " ".repeat(padding);
            let text = &self.state.text[range.clone()];
            if row == cursor {
                let pos = self.state.cursor - range.start;
                line.push_str(&text[..pos]);
                if self.focused {
                    line.push_str(CURSOR_MARKER);
                }
                let after = &text[pos..];
                if let Some((_, grapheme)) = self.pastes.graphemes(after).next() {
                    line.push_str(&render_fake_cursor(grapheme));
                    line.push_str(&after[grapheme.len()..]);
                } else {
                    line.push_str(&render_fake_cursor(" "));
                }
            } else {
                line.push_str(text);
            }
            let cells = visible_width(&line);
            if cells > width {
                line = truncate_with_ellipsis(&line, width, "", false);
            }
            line.extend(std::iter::repeat_n(
                ' ',
                width.saturating_sub(visible_width(&line)),
            ));
            lines.push(line);
        }
        lines.push(style(&border('↓', ranges.len() - last, width)));
        self.menu_height.set(0);
        if let Some(menu) = &self.completion.menu {
            let rows = menu.list.render(content);
            self.menu_height.set(rows.len());
            for row in rows {
                let mut padded = " ".repeat(padding);
                padded.push_str(&row);
                padded.extend(std::iter::repeat_n(
                    ' ',
                    width.saturating_sub(visible_width(&padded)),
                ));
                lines.push(padded);
            }
        }
        lines
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        let offset = self.rendered_count.get() + 2;
        let padding = self.padding_x.min(event.width.saturating_sub(1) / 2);
        if let Some(menu) = &mut self.completion.menu
            && (offset..offset + self.menu_height.get()).contains(&event.y)
        {
            let result = menu.list.handle_mouse(&TuiMouseEvent {
                x: event.x.saturating_sub(padding),
                y: event.y - offset,
                width: event.width.saturating_sub(padding * 2).max(1),
                height: self.menu_height.get(),
                ..*event
            });
            if event.event_type == TuiMouseEventType::Click && result.is_some() {
                self.accept_completion(false);
            }
            return result.map(|r| TuiMouseEventResult { focus: true, ..r });
        }
        if event.event_type != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
            return None;
        }
        if (1..=self.rendered_count.get()).contains(&event.y) {
            let ranges = self.visual_lines(self.rendered_width.get());
            let row = self.scroll_offset.get() + event.y - 1;
            if let Some(range) = ranges.get(row) {
                let target = event.x.saturating_sub(padding);
                let text = &self.state.text[range.clone()];
                let mut col = 0;
                let mut at = text.len();
                let mut last = 0;
                for (index, piece) in self.pastes.graphemes(text) {
                    last = index;
                    col += visible_width(piece);
                    if target < col {
                        at = index;
                        break;
                    }
                }
                if at == text.len()
                    && ranges
                        .get(row + 1)
                        .is_some_and(|next| range.end == next.start)
                {
                    at = last;
                }
                let mut at = range.start + at;
                if let Some((span, _)) = self
                    .pastes
                    .markers(&self.state.text)
                    .find(|(range, _)| range.contains(&at))
                {
                    at = span.start;
                }
                self.set_cursor(at);
                self.leave_history();
                self.action = None;
                if self.is_showing_autocomplete() {
                    self.refresh_completion(false);
                }
            }
        }
        Some(TuiMouseEventResult {
            handled: true,
            focus: true,
            ..Default::default()
        })
    }

    fn handle_input(&mut self, data: &str) {
        let keys = get_keybindings();
        let mut data = Cow::Borrowed(data);
        if let Some(forward) = self.jump_forward.take() {
            if keys.matches(&data, "tui.editor.jumpForward")
                || keys.matches(&data, "tui.editor.jumpBackward")
            {
                return;
            }
            let printable = decode_printable_key(&data);
            if let Some(text) = printable.as_deref().or_else(|| {
                data.starts_with(|ch: char| ch >= ' ')
                    .then_some(data.as_ref())
            }) {
                self.jump(text, forward);
                return;
            }
        }
        if data.contains("\x1b[200~") {
            self.paste_buffer = Some(String::new());
            data = Cow::Owned(data.replacen("\x1b[200~", "", 1));
        }
        if let Some(buffer) = &mut self.paste_buffer {
            buffer.push_str(&data);
            if let Some(end) = buffer.find("\x1b[201~")
                && let Some(buffer) = self.paste_buffer.take()
            {
                if end > 0 {
                    self.paste(&buffer[..end]);
                }
                if let Some(rest) = buffer.get(end + 6..).filter(|s| !s.is_empty()) {
                    self.handle_input(rest);
                }
            }
            return;
        }
        let is = |key| keys.matches(&data, key);
        if is("tui.input.copy") {
            return;
        }
        if is("tui.editor.undo") {
            self.leave_history();
            self.completion.cancel();
            if let Some((state, pastes)) = self.undo.pop() {
                self.state = state;
                self.pastes = pastes;
                self.set_cursor(self.state.cursor);
                self.action = None;
                self.changed();
            }
            return;
        }
        if self.is_showing_autocomplete() {
            if is("tui.select.cancel") {
                self.completion.cancel();
                return;
            }
            if ["up", "down", "pageUp", "pageDown"]
                .iter()
                .any(|action| match *action {
                    "up" => is("tui.select.up"),
                    "down" => is("tui.select.down"),
                    "pageUp" => is("tui.select.pageUp"),
                    _ => is("tui.select.pageDown"),
                })
                && let Some(menu) = &mut self.completion.menu
            {
                menu.list.handle_input(&data);
                return;
            }
            if is("tui.input.tab") {
                self.accept_completion(false);
                return;
            }
            if is("tui.select.confirm") && !self.accept_completion(true) {
                return;
            }
        }
        if is("tui.input.tab") {
            self.refresh_completion(true);
        } else if is("tui.editor.deleteToLineEnd") {
            self.delete(self.target(true, Move::Edge, true), true, false);
        } else if is("tui.editor.deleteToLineStart") {
            self.delete(self.target(false, Move::Edge, true), true, false);
        } else if is("tui.editor.deleteWordBackward") {
            self.delete(self.target(false, Move::Word, true), true, false);
        } else if is("tui.editor.deleteWordForward") {
            self.delete(self.target(true, Move::Word, true), true, false);
        } else if is("tui.editor.deleteCharBackward") || matches_key_data(&data, "shift+backspace")
        {
            self.delete(self.target(false, Move::Grapheme, true), false, true);
        } else if is("tui.editor.deleteCharForward") || matches_key_data(&data, "shift+delete") {
            self.delete(self.target(true, Move::Grapheme, true), false, false);
        } else if is("tui.editor.yank") {
            self.yank(false);
        } else if is("tui.editor.yankPop") {
            self.yank(true);
        } else if is("tui.editor.historyPrevious") || is("tui.editor.historyNext") {
            self.completion.cancel();
            self.navigate_history(is("tui.editor.historyPrevious"));
        } else if is("tui.editor.cursorLineStart") {
            self.move_horizontal(false, Move::Edge);
        } else if is("tui.editor.cursorLineEnd") {
            self.move_horizontal(true, Move::Edge);
        } else if is("tui.editor.cursorWordLeft") {
            self.move_horizontal(false, Move::Word);
        } else if is("tui.editor.cursorWordRight") {
            self.move_horizontal(true, Move::Word);
        } else if is("tui.input.newLine")
            || data.starts_with('\n')
            || data == "\x1b[13;2~"
            || data.contains('\x1b') && data.contains('\r')
        {
            let swapped = !self.disable_submit
                && matches_key_data(&data, "enter")
                && keys
                    .get_keys("tui.input.submit")
                    .iter()
                    .any(|key| matches!(key.as_str(), "shift+enter" | "shift+return"));
            if swapped && self.state.text[..self.state.cursor].ends_with('\\') {
                self.delete(self.state.cursor - 1, false, true);
                self.submit();
            } else {
                self.newline();
            }
        } else if is("tui.input.submit") {
            if self.disable_submit {
                return;
            }
            if self.state.text[..self.state.cursor].ends_with('\\') {
                self.delete(self.state.cursor - 1, false, true);
                self.newline();
            } else {
                self.submit();
            }
        } else if is("tui.editor.cursorUp") {
            self.vertical(-1, false);
        } else if is("tui.editor.cursorDown") {
            self.vertical(1, false);
        } else if is("tui.editor.cursorLeft") {
            self.move_horizontal(false, Move::Grapheme);
        } else if is("tui.editor.cursorRight") {
            self.move_horizontal(true, Move::Grapheme);
        } else if is("tui.editor.pageUp") {
            self.vertical(-(self.page() as isize), true);
        } else if is("tui.editor.pageDown") {
            self.vertical(self.page() as isize, true);
        } else if is("tui.editor.jumpForward") {
            self.jump_forward = Some(true);
        } else if is("tui.editor.jumpBackward") {
            self.jump_forward = Some(false);
        } else if matches_key_data(&data, "shift+space") {
            self.type_text(" ");
        } else if let Some(printable) = decode_printable_key(&data) {
            self.type_text(&printable);
        } else if data.starts_with(|ch: char| ch >= ' ') {
            self.type_text(&data);
        }
    }
}
