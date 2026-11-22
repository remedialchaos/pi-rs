//! Transcript search: match normalized text, highlight terminal cell spans.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use regex::RegexBuilder;
use unicode_segmentation::UnicodeSegmentation;

use crate::components::input::{Input, InputOptions, SubmitHandler};
use crate::keybindings::get_keybindings;
use crate::tui::{Component, Focusable, TuiMouseEvent, TuiMouseEventResult};
use crate::utils::{
    is_whitespace_char, strip_terminal_sequences, truncate_with_ellipsis, visible_width,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltScreenSearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AltScreenSearchMatch {
    pub segments: Vec<AltScreenSearchSegment>,
}

struct Span {
    text: Range<usize>,
    col: Range<usize>,
    row: usize,
    linear: bool,
}

#[derive(Default)]
struct Corpus {
    text: String,
    spans: Vec<Span>,
}

impl Corpus {
    fn new(lines: &[String]) -> Self {
        let mut corpus = Self::default();
        for (row, line) in lines.iter().enumerate() {
            let line = strip_terminal_sequences(line);
            let mut col = 0;
            if line.bytes().all(|b| (32..127).contains(&b)) {
                for word in line.split(' ') {
                    if !word.is_empty() {
                        corpus.append(word, row, col, word.len(), true);
                    }
                    col += word.len() + 1;
                }
            } else {
                for grapheme in line.graphemes(true) {
                    let width = visible_width(grapheme);
                    if !grapheme.chars().all(is_whitespace_char) {
                        corpus.append(grapheme, row, col, width, false);
                    }
                    col += width;
                }
            }
        }
        corpus
    }

    fn append(&mut self, text: &str, row: usize, start: usize, width: usize, linear: bool) {
        if self
            .spans
            .last()
            .is_some_and(|previous| previous.row != row || previous.col.end != start)
        {
            self.text.push(' ');
        }
        let offset = self.text.len();
        self.text.push_str(text);
        self.spans.push(Span {
            text: offset..self.text.len(),
            col: start..start + width,
            row,
            linear,
        });
    }

    fn search(&self, query: &str) -> Vec<AltScreenSearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(regex) = RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()
        else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut i = 0;
        for found in regex.find_iter(&self.text) {
            while self
                .spans
                .get(i)
                .is_some_and(|span| span.text.end <= found.start())
            {
                i += 1;
            }
            let mut hits = AltScreenSearchMatch::default();
            for span in self.spans[i..]
                .iter()
                .take_while(|span| span.text.start < found.end())
            {
                let mut start = span.col.start;
                let mut end = span.col.end;
                if span.linear {
                    start += found.start().saturating_sub(span.text.start);
                    end = span.col.start + (found.end().min(span.text.end) - span.text.start);
                }
                if let Some(previous) = hits.segments.last_mut()
                    && previous.row == span.row
                    && start <= previous.end_col
                {
                    previous.end_col = previous.end_col.max(end);
                } else {
                    hits.segments.push(AltScreenSearchSegment {
                        row: span.row,
                        start_col: start,
                        end_col: end,
                    });
                }
            }
            if !hits.segments.is_empty() {
                result.push(hits);
            }
        }
        result
    }
}

fn normalize_query(query: &str) -> String {
    let mut out = String::new();
    for word in query
        .split(is_whitespace_char)
        .filter(|word| !word.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

pub struct AltScreenSearchResult<'a> {
    pub matches: &'a [AltScreenSearchMatch],
    pub changed: bool,
}

#[derive(Default)]
pub struct AltScreenSearchIndex {
    source: Option<Rc<[String]>>,
    corpus: Corpus,
    query: String,
    matches: Vec<AltScreenSearchMatch>,
}

impl AltScreenSearchIndex {
    pub fn search(&mut self, lines: Rc<[String]>, query: &str) -> AltScreenSearchResult<'_> {
        let source_changed = self
            .source
            .as_ref()
            .is_none_or(|saved| !Rc::ptr_eq(saved, &lines) && **saved != *lines);
        if source_changed {
            self.corpus = Corpus::new(&lines);
        }
        self.source = Some(lines);
        let query = normalize_query(query);
        let changed = source_changed || query != self.query;
        if changed {
            self.matches = self.corpus.search(&query);
            self.query = query;
        }
        AltScreenSearchResult {
            matches: &self.matches,
            changed,
        }
    }

    pub fn matches(&self) -> &[AltScreenSearchMatch] {
        &self.matches
    }
}

pub fn find_alt_screen_search_matches(lines: &[String], query: &str) -> Vec<AltScreenSearchMatch> {
    let query = normalize_query(query);
    if query.is_empty() {
        Vec::new()
    } else {
        Corpus::new(lines).search(&query)
    }
}

pub fn get_alt_screen_search_match_key(
    found: &AltScreenSearchMatch,
) -> Option<(usize, usize, usize, usize)> {
    let (a, b) = (found.segments.first()?, found.segments.last()?);
    Some((a.row, a.start_col, b.row, b.end_col))
}

pub type NavigationButtonStyle = Rc<dyn Fn(&str, bool) -> String>;

pub struct AltScreenSearchComponent {
    input: Input,
    query_change: SubmitHandler,
    style: Option<NavigationButtonStyle>,
    result: (Option<usize>, usize),
    buttons: Cell<[(usize, usize); 2]>,
    hovered: Option<i8>,
    focused: bool,
}

impl AltScreenSearchComponent {
    pub fn new(query_change: SubmitHandler, style: Option<NavigationButtonStyle>) -> Self {
        Self {
            input: Input::new(InputOptions {
                prompt: Some(" ".into()),
                placeholder: "Find in transcript".into(),
                placeholder_style: Some(Rc::new(|text| format!("\x1b[2m{text}\x1b[22m"))),
            }),
            query_change,
            style,
            result: (None, 0),
            buttons: Cell::new([(0, 0); 2]),
            hovered: None,
            focused: false,
        }
    }
    pub fn set_result(&mut self, selected: Option<usize>, count: usize) {
        self.result = (selected, count);
    }
    pub fn navigation_direction_at(&self, row: usize, col: usize) -> Option<i8> {
        if row != 2 {
            return None;
        }
        self.buttons
            .get()
            .into_iter()
            .zip([-1, 1])
            .find_map(|((a, b), dir)| (a..b).contains(&col).then_some(dir))
    }
    pub fn set_hovered_navigation_direction(&mut self, direction: Option<i8>) -> bool {
        let changed = self.hovered != direction;
        self.hovered = direction;
        changed
    }
}

impl Focusable for AltScreenSearchComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

impl Component for AltScreenSearchComponent {
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
    fn handle_input(&mut self, data: &str) {
        let previous = self.input.value().to_owned();
        self.input.handle_input(data);
        let value = self.input.value();
        if value != previous {
            (self.query_change)(value);
        }
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if event.y != 1 {
            return None;
        }
        self.input.handle_mouse(&TuiMouseEvent {
            y: 0,
            x: event.x.saturating_sub(1),
            ..*event
        })
    }

    fn render(&self, width: usize) -> Vec<String> {
        let inner = width.saturating_sub(2);
        let status = if self.input.value().is_empty() {
            String::new()
        } else if self.result.1 == 0 {
            "No matches".into()
        } else {
            format!("{}/{}", self.result.0.map_or(0, |i| i + 1), self.result.1)
        };
        let status = truncate_with_ellipsis(&status, inner.saturating_sub(3), "", false);
        let status = if status.is_empty() {
            status
        } else {
            format!("\x1b[2m {status} \x1b[22m")
        };
        let input_width = inner.saturating_sub(visible_width(&status));
        let rows = self.input.render(input_width.max(1));
        let input = truncate_with_ellipsis(
            rows.first().map_or("", String::as_str),
            input_width,
            "",
            true,
        );
        let keys = get_keybindings();
        let key = |binding| {
            keys.get_keys(binding).first().map_or_else(
                || "Unbound".into(),
                |key| {
                    key.split('+')
                        .map(|part| {
                            if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                                "Option".into()
                            } else {
                                let mut chars = part.chars();
                                let mut label: String = chars
                                    .next()
                                    .into_iter()
                                    .flat_map(char::to_uppercase)
                                    .collect();
                                label.push_str(chars.as_str());
                                label
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("+")
                },
            )
        };
        let mut up = format!("↑ {}", key("tui.altScreen.searchPrevious"));
        let mut down = format!("↓ {}", key("tui.altScreen.searchNext"));
        let mut separator = " · ";
        let available = inner.saturating_sub(3);
        if visible_width(&up) + visible_width(&down) + visible_width(separator) > available {
            up.clear();
            up.push('↑');
            down.clear();
            down.push('↓');
            separator = " ";
        }
        let controls = visible_width(&up) + visible_width(&down) + visible_width(separator);
        let show = controls <= available;
        let left = if show { inner - controls - 3 } else { inner };
        let mut bottom = format!("└{}", "─".repeat(left));
        if show {
            let start = left + 2;
            let end = start + visible_width(&up);
            let next = end + visible_width(separator);
            self.buttons
                .set([(start, end), (next, next + visible_width(&down))]);
            let style = |text: String, dir| match &self.style {
                Some(style) => style(&text, self.hovered == Some(dir)),
                None => text,
            };
            bottom.push(' ');
            bottom.push_str(&style(up, -1));
            bottom.push_str(separator);
            bottom.push_str(&style(down, 1));
            bottom.push_str(" ─");
        } else {
            self.buttons.set([(0, 0); 2]);
        }
        if width <= 1 {
            return vec!["┌".into(), "│".into(), "└".into()];
        }
        bottom.push('┘');
        vec![
            format!("┌{}┐", "─".repeat(inner)),
            format!("│{input}{status}│"),
            bottom,
        ]
    }
}
