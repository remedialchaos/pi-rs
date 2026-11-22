use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::autocomplete::{
    AutocompleteItem, AutocompleteOptions, AutocompleteProvider, AutocompleteSuggestions,
};
use crate::utils::{is_autocomplete_separator, is_whitespace_char};

use super::super::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use super::Editor;

pub(super) struct Menu {
    pub list: SelectList,
    prefix: String,
    force: bool,
}

struct Request {
    deadline: Instant,
    force: bool,
    tab: bool,
}

struct Work {
    signal: Arc<AtomicBool>,
    result: Receiver<Option<AutocompleteSuggestions>>,
    lines: Arc<Vec<String>>,
    row: usize,
    col: usize,
    force: bool,
    tab: bool,
}

pub(super) struct Completion {
    provider: Option<Arc<dyn AutocompleteProvider>>,
    pub menu: Option<Menu>,
    pub triggers: Vec<char>,
    pub max: usize,
    pending: Option<Request>,
    work: Option<Work>,
}

impl Completion {
    pub(super) fn new(max: usize) -> Self {
        Self {
            provider: None,
            menu: None,
            triggers: vec!['@', '#'],
            max,
            pending: None,
            work: None,
        }
    }
    pub(super) fn set_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        self.cancel();
        self.triggers.truncate(2);
        for &trigger in provider.trigger_characters() {
            if trigger != '/'
                && trigger.len_utf16() == 1
                && !is_whitespace_char(trigger)
                && !self.triggers.contains(&trigger)
            {
                self.triggers.push(trigger);
            }
        }
        self.provider = Some(provider);
    }
    fn cancel_work(&mut self) {
        self.pending = None;
        if let Some(work) = &self.work {
            work.signal.store(true, Ordering::Release);
        }
    }
    pub(super) fn cancel(&mut self) {
        self.cancel_work();
        self.menu = None;
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.cancel_work();
    }
}

fn attachment(text: &str, triggers: &[char]) -> bool {
    let token_start = |i: usize| {
        let start = text[..i].trim_end_matches(['(', '[', '{', '<', '`']).len();
        text[..start]
            .chars()
            .next_back()
            .is_none_or(is_autocomplete_separator)
    };
    if let Some(open) = text.rfind("@\"")
        && !text[open + 2..].contains('"')
        && token_start(open)
    {
        return true;
    }
    for (index, c) in text.char_indices().rev() {
        if is_autocomplete_separator(c) {
            break;
        }
        if triggers.contains(&c) && token_start(index) {
            return true;
        }
    }
    false
}

impl Editor {
    pub(super) fn refresh_completion(&mut self, tab: bool) {
        let Some(provider) = self.completion.provider.as_ref() else {
            return;
        };
        let start = self.line_start();
        let before = &self.state.text[start..self.state.cursor];
        let slash = start == 0
            && before
                .trim_start_matches(is_whitespace_char)
                .starts_with('/');
        let command = slash && !before.trim_start_matches(is_whitespace_char).contains(' ');
        let attachment = attachment(before, &self.completion.triggers);
        let visible = self.completion.menu.as_ref();
        if !tab && !slash && !attachment && visible.is_none() {
            self.completion.cancel_work();
            return;
        }
        let force = if tab {
            !command
        } else {
            visible.is_some_and(|menu| menu.force)
        };
        if force {
            let (row, col) = self.cursor();
            let lines = self.lines().map(str::to_owned).collect::<Vec<_>>();
            if !provider.should_trigger_file_completion(&lines, row, col) {
                self.completion.cancel();
                return;
            }
        }
        self.completion.cancel_work();
        let now = Instant::now();
        let delay = if attachment && !force && !tab {
            Duration::from_millis(20)
        } else {
            Duration::ZERO
        };
        self.completion.pending = Some(Request {
            deadline: now + delay,
            force,
            tab,
        });
        self.tick_completion(now);
    }

    pub(super) fn tick_completion(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if let Some(work) = self.completion.work.take() {
            match work.result.try_recv() {
                Err(TryRecvError::Empty) => {
                    self.completion.work = Some(work);
                }
                result => {
                    let (row, col) = self.cursor();
                    if !work.signal.load(Ordering::Acquire)
                        && work.row == row
                        && work.col == col
                        && self.lines().eq(work.lines.iter().map(String::as_str))
                    {
                        self.update_menu(result.ok().flatten(), work.force, work.tab);
                        changed = true;
                    }
                }
            }
        }
        if self.completion.work.is_none()
            && self
                .completion
                .pending
                .as_ref()
                .is_some_and(|pending| pending.deadline <= now)
            && let Some(request) = self.completion.pending.take()
            && let Some(provider) = self.completion.provider.clone()
        {
            let (row, col) = self.cursor();
            let lines = Arc::new(self.lines().map(str::to_owned).collect::<Vec<_>>());
            let signal = Arc::new(AtomicBool::new(false));
            let cancelled = signal.clone();
            let (send, result) = mpsc::sync_channel(1);
            let force = request.force;
            let worker_lines = lines.clone();
            let worker = std::thread::Builder::new()
                .name("pi-tui-autocomplete".into())
                .spawn(move || {
                    let suggest = provider.get_suggestions(
                        &worker_lines,
                        row,
                        col,
                        AutocompleteOptions {
                            signal: Some(&cancelled),
                            force,
                        },
                    );
                    let _ = send.send(suggest);
                });
            if worker.is_ok() {
                self.completion.work = Some(Work {
                    signal,
                    result,
                    lines,
                    row,
                    col,
                    force,
                    tab: request.tab,
                });
            } else {
                changed |= self.completion.menu.take().is_some();
            }
        }
        if changed {
            (self.request_render)();
        }
        changed
    }

    fn update_menu(&mut self, result: Option<AutocompleteSuggestions>, force: bool, tab: bool) {
        let Some(result) = result.filter(|result| !result.items.is_empty()) else {
            self.completion.menu = None;
            return;
        };
        if force && tab && result.items.len() == 1 {
            if let Some(item) = result.items.into_iter().next()
                && self.apply_completion(&item, &result.prefix)
            {
                self.changed();
            }
            return;
        }
        let index = if result.prefix.is_empty() {
            0
        } else {
            result
                .items
                .iter()
                .position(|item| item.value == result.prefix)
                .or_else(|| {
                    result
                        .items
                        .iter()
                        .position(|item| item.value.starts_with(&result.prefix))
                })
                .unwrap_or(0)
        };
        let layout = if result.prefix.starts_with('/') {
            SelectListLayoutOptions {
                min_primary_column_width: Some(12),
                max_primary_column_width: Some(32),
                truncate_primary: None,
            }
        } else {
            SelectListLayoutOptions::default()
        };
        let items = result
            .items
            .into_iter()
            .map(|item| SelectItem {
                value: item.value,
                label: item.label,
                description: item.description,
            })
            .collect();
        let mut list = SelectList::new(
            items,
            self.completion.max,
            self.theme.select_list.clone(),
            layout,
        );
        list.set_selected_index(index);
        self.completion.menu = Some(Menu {
            list,
            prefix: result.prefix,
            force,
        });
    }

    pub(super) fn accept_completion(&mut self, via_enter: bool) -> bool {
        let Some(menu) = self.completion.menu.take() else {
            return false;
        };
        let Some(item) = menu.list.selected_item() else {
            return false;
        };
        let before = &self.state.text[self.line_start()..self.state.cursor];
        let submit = via_enter
            && menu.prefix.starts_with('/')
            && self.line_start() == 0
            && before
                .trim_start_matches(is_whitespace_char)
                .starts_with('/')
            && !before.trim_start_matches(is_whitespace_char).contains(' ');
        let item = AutocompleteItem {
            value: item.value.clone(),
            label: item.label.clone(),
            description: item.description.clone(),
        };
        if self.apply_completion(&item, &menu.prefix) {
            if !submit {
                self.changed();
            }
            submit
        } else {
            false
        }
    }

    fn apply_completion(&mut self, item: &AutocompleteItem, prefix: &str) -> bool {
        let Some(provider) = self.completion.provider.clone() else {
            return false;
        };
        let (row, col) = self.cursor();
        let lines = self.lines().map(str::to_owned).collect();
        let applied = provider.apply_completion(lines, row, col, item, prefix);
        let Some(line) = applied.lines.get(applied.cursor_line) else {
            return false;
        };
        if !line.is_char_boundary(applied.cursor_col) {
            return false;
        }
        let cursor = applied.lines[..applied.cursor_line]
            .iter()
            .fold(applied.cursor_col, |at, line| at + line.len() + 1);
        self.remember();
        self.action = None;
        self.state.text = applied.lines.join("\n");
        self.set_cursor(cursor);
        self.completion.cancel();
        true
    }
}
