//! Index-preserving pickers. Dialogs own presentation; their callers own actions.

use std::cell::Cell;
use std::time::{Duration, Instant};

use pi_rs_tui::{
    Component, Focusable, Input, InputOptions, Text, fuzzy_filter, get_keybindings,
    get_select_page_index, truncate_to_width,
};

use super::theme::Theme;

mod auth;
mod choices;
mod configuration;
mod sessions;
mod settings;
mod setup;
mod tree;

pub use auth::*;
pub use choices::*;
pub use configuration::*;
pub use sessions::*;
pub use settings::*;
pub use setup::*;
pub use tree::*;

pub struct PickerItem<T> {
    pub label: String,
    pub search: String,
    pub value: T,
}

impl<T> PickerItem<T> {
    pub fn new(label: String, value: T) -> Self {
        Self {
            search: label.clone(),
            label,
            value,
        }
    }
}

const DEFAULT_VISIBLE: usize = 10;
type Rank = Box<dyn FnMut(&str) -> Vec<usize>>;
type Pick = Box<dyn FnMut(Option<usize>)>;
type Highlight = Box<dyn FnMut(usize)>;

pub struct Picker {
    title: String,
    description: String,
    hint: String,
    labels: Vec<String>,
    order: Vec<usize>,
    selected: usize,
    visible: usize,
    rank: Rank,
    theme: Theme,
    search: Input,
    filter: bool,
    focused: bool,
    wrapping: bool,
    finished: bool,
    timeout: Option<Instant>,
    seconds_left: Option<u64>,
    select: Pick,
    highlight: Option<Highlight>,
    rendered_start: Cell<usize>,
}

impl Picker {
    pub fn new(
        title: String,
        items: Vec<String>,
        theme: Theme,
        select: impl FnMut(Option<usize>) + 'static,
    ) -> Self {
        let searchable = items.clone();
        Self::ranked(title, items, theme, fuzzy_rank(searchable), select)
    }

    pub fn with_items<T: 'static>(
        title: String,
        items: Vec<PickerItem<T>>,
        theme: Theme,
        mut select: impl FnMut(Option<&T>) + 'static,
    ) -> Self {
        let (mut labels, mut searchable, mut values) = (Vec::new(), Vec::new(), Vec::new());
        for item in items {
            labels.push(item.label);
            searchable.push(item.search);
            values.push(item.value);
        }
        Self::ranked(title, labels, theme, fuzzy_rank(searchable), move |index| {
            select(index.and_then(|index| values.get(index)));
        })
    }

    pub fn ranked(
        title: String,
        labels: Vec<String>,
        theme: Theme,
        rank: impl FnMut(&str) -> Vec<usize> + 'static,
        select: impl FnMut(Option<usize>) + 'static,
    ) -> Self {
        let mut picker = Self {
            title,
            description: String::new(),
            hint: "Type to filter · ↑↓ navigate · Enter select · Esc cancel".into(),
            order: Vec::new(),
            labels,
            selected: 0,
            visible: DEFAULT_VISIBLE,
            rank: Box::new(rank),
            theme,
            search: Input::new(InputOptions {
                prompt: Some("Search: ".into()),
                ..InputOptions::default()
            }),
            filter: true,
            focused: false,
            wrapping: true,
            finished: false,
            timeout: None,
            seconds_left: None,
            select: Box::new(select),
            highlight: None,
            rendered_start: Cell::new(0),
        };
        picker.refilter(None);
        picker
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn preselect(mut self, index: usize) -> Self {
        self.select_index(index);
        self
    }

    pub fn with_query(mut self, query: &str) -> Self {
        self.search.set_value(query.to_owned());
        self.search.set_cursor(query.len());
        self.refilter(None);
        self
    }

    pub fn without_filter(mut self) -> Self {
        self.filter = false;
        self.hint = "↑↓ navigate · Enter select · Esc cancel".into();
        self
    }

    pub fn without_wrap(mut self) -> Self {
        self.wrapping = false;
        self
    }

    pub fn max_visible(mut self, count: usize) -> Self {
        self.visible = count.max(1);
        self
    }

    pub fn timeout(mut self, duration: Duration) -> Self {
        if !duration.is_zero() {
            self.timeout = Instant::now().checked_add(duration);
            self.seconds_left = Some(seconds_ceil(duration));
        }
        self
    }

    pub fn on_highlight(mut self, highlight: impl FnMut(usize) + 'static) -> Self {
        self.highlight = Some(Box::new(highlight));
        self
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.order.get(self.selected).copied()
    }

    pub fn visible_order(&self) -> &[usize] {
        &self.order
    }

    pub fn query(&self) -> &str {
        self.search.value()
    }

    fn select_index(&mut self, index: usize) {
        if let Some(position) = self.order.iter().position(|candidate| *candidate == index) {
            self.set_position(position);
        }
    }

    fn set_position(&mut self, position: usize) {
        let old = self.selected_index();
        self.selected = position;
        self.notify_highlight(old);
    }

    fn notify_highlight(&mut self, old: Option<usize>) {
        if let Some(index) = self.selected_index()
            && Some(index) != old
            && let Some(highlight) = &mut self.highlight
        {
            highlight(index);
        }
    }

    fn refilter(&mut self, keep: Option<usize>) {
        let old = self.selected_index();
        self.order = (self.rank)(self.search.value());
        self.order.retain(|&index| index < self.labels.len());
        self.selected = keep
            .and_then(|keep| self.order.iter().position(|&index| index == keep))
            .unwrap_or(0);
        self.notify_highlight(old);
    }

    fn replace_rows(&mut self, labels: Vec<String>, rank: Rank, keep: Option<usize>) {
        self.labels = labels;
        self.rank = rank;
        self.refilter(keep);
    }

    fn finish(&mut self, index: Option<usize>) {
        if !self.finished {
            self.finished = true;
            (self.select)(index);
        }
    }
}

fn fuzzy_rank(searchable: Vec<String>) -> impl FnMut(&str) -> Vec<usize> {
    let indices: Vec<usize> = (0..searchable.len()).collect();
    move |query| fuzzy_filter(&indices, query, |index| &searchable[*index])
}

fn seconds_ceil(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

fn window(position: usize, length: usize, visible: usize) -> std::ops::Range<usize> {
    let start = position
        .saturating_sub(visible / 2)
        .min(length.saturating_sub(visible));
    start..length.min(start + visible)
}

fn append_text(lines: &mut Vec<String>, width: usize, theme: &Theme, color: &str, text: &str) {
    lines.extend(Text::new(theme.fg(color, text), 0, 0, None).render(width));
}

impl Component for Picker {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![self.theme.fg("border", &"─".repeat(width))];
        if let Some(seconds) = self.seconds_left {
            append_text(
                &mut lines,
                width,
                &self.theme,
                "accent",
                &format!("{} ({seconds}s)", self.title),
            );
        } else {
            append_text(&mut lines, width, &self.theme, "accent", &self.title);
        }
        if !self.description.is_empty() {
            append_text(&mut lines, width, &self.theme, "muted", &self.description);
        }
        if self.filter {
            lines.extend(self.search.render(width));
        }
        self.rendered_start.set(lines.len());
        let visible = window(self.selected, self.order.len(), self.visible);
        if self.order.is_empty() {
            append_text(
                &mut lines,
                width,
                &self.theme,
                "muted",
                "  No matching items",
            );
        }
        for position in visible.clone() {
            let item = &self.labels[self.order[position]];
            let selected = position == self.selected;
            let prefix = if selected { "› " } else { "  " };
            let token = if selected { "accent" } else { "text" };
            lines.push(truncate_to_width(
                &self.theme.fg(token, &format!("{prefix}{item}")),
                width,
                true,
            ));
        }
        if visible.len() < self.order.len() {
            append_text(
                &mut lines,
                width,
                &self.theme,
                "muted",
                &format!("  ({}/{})", self.selected + 1, self.order.len()),
            );
        }
        append_text(&mut lines, width, &self.theme, "dim", &self.hint);
        lines.push(self.theme.fg("border", &"─".repeat(width)));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if self.finished {
            return;
        }
        let keys = get_keybindings();
        let last = self.order.len().saturating_sub(1);
        if keys.matches(data, "tui.select.up") || (!self.filter && data == "k") {
            self.set_position(if self.selected == 0 && self.wrapping {
                last
            } else {
                self.selected.saturating_sub(1)
            });
        } else if keys.matches(data, "tui.select.down") || (!self.filter && data == "j") {
            self.set_position(if self.selected == last && self.wrapping {
                0
            } else {
                (self.selected + 1).min(last)
            });
        } else if let Some(index) =
            get_select_page_index(&keys, data, self.selected, self.order.len(), self.visible)
        {
            self.set_position(index);
        } else if keys.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(index) = self.selected_index() {
                self.finish(Some(index));
            }
        } else if keys.matches(data, "tui.select.cancel") {
            self.finish(None);
        } else if self.filter {
            let previous = self.search.value().to_owned();
            self.search.handle_input(data);
            if previous != self.search.value() {
                self.refilter(if self.search.value().is_empty() {
                    self.selected_index()
                } else {
                    None
                });
            }
        }
    }

    fn handle_mouse(
        &mut self,
        event: &pi_rs_tui::TuiMouseEvent,
    ) -> Option<pi_rs_tui::TuiMouseEventResult> {
        use pi_rs_tui::{TuiMouseButton, TuiMouseEventResult, TuiMouseEventType};
        if self.finished
            || event.event_type != TuiMouseEventType::Press
            || event.button != TuiMouseButton::Left
        {
            return None;
        }
        let start = self.rendered_start.get();
        if self.filter && event.y + 1 == start {
            let mut local = *event;
            local.y = 0;
            return self.search.handle_mouse(&local);
        }
        let row = event.y.checked_sub(start)?;
        let display = window(self.selected, self.order.len(), self.visible);
        if row >= display.len() {
            return None;
        }
        self.set_position(display.start + row);
        Some(TuiMouseEventResult {
            handled: true,
            focus: true,
            ..TuiMouseEventResult::default()
        })
    }

    fn tick(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.timeout.filter(|_| !self.finished) else {
            return false;
        };
        if now >= deadline {
            self.finish(None);
            return true;
        }
        let next = Some(seconds_ceil(deadline.duration_since(now)));
        let changed = next != self.seconds_left;
        self.seconds_left = next;
        changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }
}

impl Focusable for Picker {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search.set_focused(focused);
    }
}

#[cfg(test)]
mod tests;
