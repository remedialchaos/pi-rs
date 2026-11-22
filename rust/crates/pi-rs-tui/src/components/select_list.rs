use std::ops::Range;
use std::rc::Rc;

use crate::keybindings::{KeybindingsManager, get_keybindings, get_select_page_index};
use crate::tui::{
    Component, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::utils::{truncate_with_ellipsis, visible_width};

use super::TextStyleFn;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl SelectItem {
    fn display(&self) -> &str {
        if self.label.is_empty() {
            &self.value
        } else {
            &self.label
        }
    }
}

#[derive(Clone)]
pub struct SelectListTheme {
    pub selected_text: TextStyleFn,
    pub description: TextStyleFn,
    pub scroll_info: TextStyleFn,
    pub no_match: TextStyleFn,
}

impl Default for SelectListTheme {
    fn default() -> Self {
        let plain: TextStyleFn = Rc::new(str::to_owned);
        Self {
            selected_text: plain.clone(),
            description: plain.clone(),
            scroll_info: plain.clone(),
            no_match: plain,
        }
    }
}

pub struct TruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub selected: bool,
}

pub type TruncatePrimary = Box<dyn Fn(TruncatePrimaryContext<'_>) -> String>;
pub type SelectionHandler = Box<dyn FnMut(&SelectItem)>;

#[derive(Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<TruncatePrimary>,
}

pub(crate) fn visible_range(selected: usize, count: usize, visible: usize) -> Range<usize> {
    let start = selected
        .saturating_sub(visible / 2)
        .min(count.saturating_sub(visible));
    start..start.saturating_add(visible).min(count)
}

pub(crate) fn move_selection(
    keys: &KeybindingsManager,
    data: &str,
    selected: usize,
    count: usize,
    visible: usize,
) -> Option<usize> {
    if keys.matches(data, "tui.select.up") {
        Some(if selected == 0 {
            count.saturating_sub(1)
        } else {
            selected - 1
        })
    } else if keys.matches(data, "tui.select.down") {
        Some(if selected >= count.saturating_sub(1) {
            0
        } else {
            selected + 1
        })
    } else {
        get_select_page_index(keys, data, selected, count, visible)
    }
}

pub struct SelectList {
    items: Vec<SelectItem>,
    filtered: Vec<usize>,
    selected: usize,
    mouse_pressed: Option<usize>,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
    pub on_select: Option<SelectionHandler>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_selection_change: Option<SelectionHandler>,
}

impl SelectList {
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            mouse_pressed: None,
            max_visible,
            theme,
            layout,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        let lower = filter.to_lowercase();
        self.filtered.clear();
        self.filtered.extend(
            self.items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| item.value.to_lowercase().starts_with(&lower).then_some(i)),
        );
        self.selected = 0;
        self.mouse_pressed = None;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.filtered.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered
            .get(self.selected)
            .map(|&index| &self.items[index])
    }

    fn notify(&mut self, confirm: bool) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        let callback = if confirm {
            &mut self.on_select
        } else {
            &mut self.on_selection_change
        };
        if let Some(callback) = callback {
            callback(&self.items[index]);
        }
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let text = item.display();
        if let Some(truncate) = &self.layout.truncate_primary {
            let text = truncate(TruncatePrimaryContext {
                text,
                max_width,
                column_width,
                item,
                selected,
            });
            truncate_with_ellipsis(&text, max_width, "", false)
        } else {
            truncate_with_ellipsis(text, max_width, "", false)
        }
    }

    fn render_item(
        &self,
        item: &SelectItem,
        selected: bool,
        width: usize,
        column: usize,
    ) -> String {
        let prefix = if selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);
        let mut label = String::from(prefix);
        if width > 40
            && let Some(description) = &item.description
        {
            let column = column.min(width.saturating_sub(prefix_width + 4)).max(1);
            let primary =
                self.truncate_primary(item, selected, column.saturating_sub(2).max(1), column);
            let primary_width = visible_width(&primary);
            let spacing = column.saturating_sub(primary_width).max(1);
            let remaining = width.saturating_sub(prefix_width + primary_width + spacing + 2);
            if remaining > 10 {
                let description = description.trim();
                if !description.is_empty() {
                    let mut normalized = String::new();
                    let mut break_before = false;
                    for c in description.chars() {
                        if matches!(c, '\r' | '\n') {
                            break_before = true;
                        } else {
                            if break_before {
                                normalized.push(' ');
                                break_before = false;
                            }
                            normalized.push(c);
                        }
                    }
                    let mut tail = " ".repeat(spacing);
                    tail.push_str(&truncate_with_ellipsis(&normalized, remaining, "", false));
                    label.push_str(&primary);
                    if selected {
                        label.push_str(&tail);
                        return (self.theme.selected_text)(&label);
                    }
                    label.push_str(&(self.theme.description)(&tail));
                    return label;
                }
            }
        }
        let max = width.saturating_sub(prefix_width + 2);
        label.push_str(&self.truncate_primary(item, selected, max, max));
        if selected {
            (self.theme.selected_text)(&label)
        } else {
            label
        }
    }
}

impl Component for SelectList {
    fn captures_viewport_input(&self) -> bool {
        true
    }

    fn render(&self, width: usize) -> Vec<String> {
        if self.filtered.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }
        let a = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(32);
        let b = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(32);
        let column = self
            .filtered
            .iter()
            .map(|&i| visible_width(self.items[i].display()) + 2)
            .max()
            .unwrap_or(0)
            .clamp(a.min(b).max(1), a.max(b).max(1));
        let range = visible_range(self.selected, self.filtered.len(), self.max_visible);
        let mut lines: Vec<_> = range
            .clone()
            .map(|i| {
                self.render_item(
                    &self.items[self.filtered[i]],
                    i == self.selected,
                    width,
                    column,
                )
            })
            .collect();
        if range.start > 0 || range.end < self.filtered.len() {
            let info = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push((self.theme.scroll_info)(&truncate_with_ellipsis(
                &info,
                width.saturating_sub(2),
                "",
                false,
            )));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let keys = get_keybindings();
        if let Some(index) = move_selection(
            &keys,
            data,
            self.selected,
            self.filtered.len(),
            self.max_visible,
        ) {
            let previous = self.selected;
            self.selected = index;
            if previous != index
                || keys.matches(data, "tui.select.up")
                || keys.matches(data, "tui.select.down")
            {
                self.notify(false);
            }
        } else if keys.matches(data, "tui.select.confirm") {
            self.notify(true);
        } else if keys.matches(data, "tui.select.cancel")
            && let Some(callback) = &mut self.on_cancel
        {
            callback();
        }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if self.filtered.is_empty() {
            return None;
        }
        let previous = self.selected;
        if event.event_type == TuiMouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|d| *d != 0)
        {
            self.set_selected_index(self.selected.saturating_add_signed(delta.signum() as isize));
            let changed = self.selected != previous;
            if changed {
                self.notify(false);
            }
            return Some(TuiMouseEventResult {
                handled: true,
                render: Some(changed),
                ..Default::default()
            });
        }
        let range = visible_range(self.selected, self.filtered.len(), self.max_visible);
        let index = range.start.saturating_add(event.y);
        if event.button != TuiMouseButton::Left || !range.contains(&index) {
            return None;
        }
        let press = match event.event_type {
            TuiMouseEventType::Press => {
                self.mouse_pressed = Some(index);
                self.selected = index;
                true
            }
            TuiMouseEventType::Click => {
                self.selected = self.mouse_pressed.take().unwrap_or(index);
                false
            }
            _ => return None,
        };
        if self.selected != previous {
            self.notify(false);
        }
        if !press {
            self.notify(true);
        }
        Some(TuiMouseEventResult {
            handled: true,
            focus: press,
            ..Default::default()
        })
    }
}
