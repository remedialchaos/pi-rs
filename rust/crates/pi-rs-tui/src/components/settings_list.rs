use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::fuzzy::fuzzy_filter;
use crate::keybindings::get_keybindings;
use crate::tui::{
    Component, ComponentRef, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::utils::{truncate_with_ellipsis, visible_width, wrap_text_with_ansi};

use super::{
    TextStyleFn,
    input::Input,
    select_list::{move_selection, visible_range},
};

#[derive(Default)]
pub struct SubmenuResult {
    pub value: Option<String>,
    pub navigate_to: Option<String>,
}

pub type SubmenuDone = Box<dyn FnOnce(SubmenuResult)>;
pub type SettingSubmenu = Box<dyn FnMut(&str, SubmenuDone) -> ComponentRef>;
pub type SettingChange = Box<dyn FnMut(&str, &str)>;
pub type SettingStyle = Rc<dyn Fn(&str, bool) -> String>;

pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
    pub submenu: Option<SettingSubmenu>,
}

pub struct SettingsListTheme {
    pub label: SettingStyle,
    pub value: SettingStyle,
    pub description: TextStyleFn,
    pub cursor: String,
    pub hint: TextStyleFn,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        let style: SettingStyle = Rc::new(|text, _| text.to_owned());
        let plain: TextStyleFn = Rc::new(str::to_owned);
        Self {
            label: style.clone(),
            value: style,
            description: plain.clone(),
            cursor: "→ ".into(),
            hint: plain,
        }
    }
}

#[derive(Default)]
pub struct SettingsListOptions {
    pub enable_search: bool,
}

struct Submenu {
    child: ComponentRef,
    completion: Rc<RefCell<Option<SubmenuResult>>>,
    index: usize,
    item: usize,
}

pub struct SettingsList {
    items: Vec<SettingItem>,
    filtered: Vec<usize>,
    selected: usize,
    max_visible: usize,
    mouse_pressed: Option<usize>,
    theme: SettingsListTheme,
    on_change: SettingChange,
    on_cancel: Box<dyn FnMut()>,
    search: Option<Input>,
    submenu: Option<Submenu>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: SettingChange,
        on_cancel: Box<dyn FnMut()>,
        options: SettingsListOptions,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            max_visible,
            mouse_pressed: None,
            theme,
            on_change,
            on_cancel,
            search: options.enable_search.then(Input::default),
            submenu: None,
        }
    }

    pub fn update_value(&mut self, id: &str, new_value: String) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.current_value = new_value;
        }
    }

    pub fn select_item(&mut self, id: &str) {
        if let Some(index) = self.filtered.iter().position(|&i| self.items[i].id == id) {
            self.selected = index;
        }
    }

    fn activate(&mut self) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        let item = &mut self.items[index];
        if let Some(make_submenu) = &mut item.submenu {
            let completion = Rc::new(RefCell::new(None));
            let result = completion.clone();
            let child = make_submenu(
                &item.current_value,
                Box::new(move |value| *result.borrow_mut() = Some(value)),
            );
            self.submenu = Some(Submenu {
                child,
                completion,
                index: self.selected,
                item: index,
            });
        } else if !item.values.is_empty() {
            let next = item
                .values
                .iter()
                .position(|value| value == &item.current_value)
                .map_or(0, |index| (index + 1) % item.values.len());
            item.current_value.clone_from(&item.values[next]);
            (self.on_change)(&item.id, &item.current_value);
        }
    }

    fn apply_completion(&mut self) -> bool {
        let Some(submenu) = &self.submenu else {
            return false;
        };
        let Some(result) = submenu.completion.borrow_mut().take() else {
            return false;
        };
        if let Some(value) = result.value {
            let item = &mut self.items[submenu.item];
            item.current_value = value;
            (self.on_change)(&item.id, &item.current_value);
        }
        let index = submenu.index;
        self.submenu = None;
        if let Some(id) = &result.navigate_to {
            self.select_item(id);
            self.activate();
        } else {
            self.selected = index;
        }
        true
    }

    fn hint(&self, width: usize, lines: &mut Vec<String>) {
        lines.push(String::new());
        let text = if self.search.is_some() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(truncate_with_ellipsis(
            &(self.theme.hint)(text),
            width,
            "…",
            false,
        ));
    }
}

impl Component for SettingsList {
    fn captures_viewport_input(&self) -> bool {
        true
    }
    fn children(&self) -> &[ComponentRef] {
        self.submenu
            .as_ref()
            .map_or(&[], |menu| std::slice::from_ref(&menu.child))
    }
    fn invalidate(&mut self) {
        if let Some(menu) = &self.submenu {
            menu.child.borrow_mut().invalidate();
        }
    }
    fn tick(&mut self, _now: Instant) -> bool {
        self.apply_completion()
    }

    fn render(&self, width: usize) -> Vec<String> {
        if let Some(submenu) = &self.submenu {
            return submenu.child.borrow().render(width);
        }
        let mut lines = self.search.as_ref().map_or_else(Vec::new, |search| {
            let mut lines = search.render(width);
            lines.push(String::new());
            lines
        });
        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            if self.search.is_some() {
                self.hint(width, &mut lines);
            }
            return lines;
        }
        if self.filtered.is_empty() {
            lines.push(truncate_with_ellipsis(
                &(self.theme.hint)("  No matching settings"),
                width,
                "…",
                false,
            ));
            self.hint(width, &mut lines);
            return lines;
        }
        let label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(36);
        let range = visible_range(self.selected, self.filtered.len(), self.max_visible);
        for i in range.clone() {
            let item = &self.items[self.filtered[i]];
            let selected = i == self.selected;
            let prefix = if selected { &self.theme.cursor } else { "  " };
            let mut label = item.label.clone();
            label.extend(std::iter::repeat_n(
                ' ',
                label_width.saturating_sub(visible_width(&label)),
            ));
            let remaining = width.saturating_sub(visible_width(prefix) + label_width + 4);
            let label = (self.theme.label)(&label, selected);
            let value = (self.theme.value)(
                &truncate_with_ellipsis(&item.current_value, remaining, "", false),
                selected,
            );
            lines.push(truncate_with_ellipsis(
                &format!("{prefix}{label}  {value}"),
                width,
                "…",
                false,
            ));
        }
        if range.start > 0 || range.end < self.filtered.len() {
            let info = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push((self.theme.hint)(&truncate_with_ellipsis(
                &info,
                width.saturating_sub(2),
                "",
                false,
            )));
        }
        if let Some(&index) = self.filtered.get(self.selected)
            && let Some(description) = &self.items[index].description
        {
            lines.push(String::new());
            lines.extend(
                wrap_text_with_ansi(description, width.saturating_sub(4).max(1))
                    .into_iter()
                    .map(|line| (self.theme.description)(&format!("  {line}"))),
            );
        }
        self.hint(width, &mut lines);
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(submenu) = &self.submenu {
            submenu.child.borrow_mut().handle_input(data);
            self.apply_completion();
            return;
        }
        let keys = get_keybindings();
        if let Some(index) = move_selection(
            &keys,
            data,
            self.selected,
            self.filtered.len(),
            self.max_visible,
        ) {
            self.selected = index;
        } else if keys.matches(data, "tui.select.confirm")
            || (data == " " && self.search.as_ref().is_none_or(|s| s.value().is_empty()))
        {
            self.activate();
            self.apply_completion();
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        } else if let Some(search) = &mut self.search {
            let previous = search.value().to_owned();
            search.handle_input(data);
            if previous != search.value() {
                let indices: Vec<_> = (0..self.items.len()).collect();
                self.filtered =
                    fuzzy_filter(&indices, search.value(), |&i| self.items[i].label.as_str());
                self.selected = 0;
            }
        }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if let Some(submenu) = &self.submenu {
            let result = submenu.child.borrow_mut().handle_mouse(event);
            self.apply_completion();
            return result.map(|result| TuiMouseEventResult {
                focus: true,
                ..result
            });
        }
        let row = if let Some(search) = &mut self.search {
            if event.y == 0 {
                return search.handle_mouse(event);
            }
            event.y.checked_sub(2)?
        } else {
            event.y
        };
        if self.filtered.is_empty() {
            return None;
        }
        if event.event_type == TuiMouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|d| *d != 0)
        {
            let previous = self.selected;
            self.selected = self
                .selected
                .saturating_add_signed(delta.signum() as isize)
                .min(self.filtered.len() - 1);
            return Some(TuiMouseEventResult {
                handled: true,
                render: Some(previous != self.selected),
                ..Default::default()
            });
        }
        let range = visible_range(self.selected, self.filtered.len(), self.max_visible);
        let index = range.start.saturating_add(row);
        if event.button != TuiMouseButton::Left || !range.contains(&index) {
            return None;
        }
        match event.event_type {
            TuiMouseEventType::Press => {
                self.mouse_pressed = Some(index);
                self.selected = index;
                Some(TuiMouseEventResult {
                    handled: true,
                    focus: true,
                    ..Default::default()
                })
            }
            TuiMouseEventType::Click => {
                self.selected = self.mouse_pressed.take().unwrap_or(index);
                self.activate();
                self.apply_completion();
                Some(TuiMouseEventResult {
                    handled: true,
                    ..Default::default()
                })
            }
            _ => None,
        }
    }
}
