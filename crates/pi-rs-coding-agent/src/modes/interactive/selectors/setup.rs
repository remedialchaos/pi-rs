use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pi_rs_tui::{Component, Focusable, Input, InputOptions, get_keybindings};

use super::{Picker, PickerItem, append_text, seconds_ceil};
use crate::{
    config::APP_NAME,
    core::project_trust::{
        ProjectTrustOption, ProjectTrustStoreEntry, get_project_trust_options, normalize_cwd,
    },
    modes::interactive::theme::Theme,
};
use std::path::Path;

type InputSubmit = Box<dyn FnMut(Option<String>)>;

pub struct InputDialog {
    title: String,
    description: String,
    theme: Theme,
    input: Input,
    submit: Option<InputSubmit>,
    timeout: Option<Instant>,
    remaining: u64,
    masked: bool,
}

impl InputDialog {
    pub fn new(
        title: String,
        placeholder: Option<String>,
        theme: Theme,
        submit: impl FnMut(Option<String>) + 'static,
    ) -> Self {
        let color = theme.clone();
        let input = Input::new(InputOptions {
            placeholder: placeholder.unwrap_or_default(),
            placeholder_style: Some(Rc::new(move |text| color.fg("muted", text))),
            ..InputOptions::default()
        });
        Self {
            title,
            description: String::new(),
            theme,
            input,
            submit: Some(Box::new(submit)),
            timeout: None,
            remaining: 0,
            masked: false,
        }
    }

    pub fn initial_value(mut self, value: String) -> Self {
        let end = value.len();
        self.input.set_value(value);
        self.input.set_cursor(end);
        self
    }

    pub fn description(mut self, description: String) -> Self {
        self.description = description;
        self
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn timeout(mut self, duration: Duration) -> Self {
        if !duration.is_zero() {
            self.timeout = Instant::now().checked_add(duration);
            self.remaining = seconds_ceil(duration);
        }
        self
    }

    fn finish(&mut self, value: Option<String>) {
        if let Some(mut submit) = self.submit.take() {
            submit(value);
        }
    }
}

impl Component for InputDialog {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let mut out = vec![self.theme.fg("border", &"─".repeat(width))];
        if self.timeout.is_some() {
            append_text(
                &mut out,
                width,
                &self.theme,
                "accent",
                &format!("{} ({}s)", self.title, self.remaining),
            );
        } else {
            append_text(&mut out, width, &self.theme, "accent", &self.title);
        }
        if !self.description.is_empty() {
            append_text(&mut out, width, &self.theme, "muted", &self.description);
        }
        if self.masked {
            let cursor = if self.input.is_focused() {
                pi_rs_tui::CURSOR_MARKER
            } else {
                ""
            };
            let visible = self
                .input
                .value()
                .chars()
                .count()
                .min(width.saturating_sub(3));
            let mask = "•".repeat(visible);
            out.push(pi_rs_tui::truncate_to_width(
                &format!("> {mask}{cursor} "),
                width,
                true,
            ));
        } else {
            out.extend(self.input.render(width));
        }
        append_text(
            &mut out,
            width,
            &self.theme,
            "dim",
            "Enter submit · Esc cancel",
        );
        out.push(self.theme.fg("border", &"─".repeat(width)));
        out
    }

    fn handle_input(&mut self, data: &str) {
        if self.submit.is_none() {
            return;
        }
        let keys = get_keybindings();
        if keys.matches(data, "tui.select.confirm") || data == "\n" {
            self.finish(Some(self.input.value().to_owned()));
        } else if keys.matches(data, "tui.select.cancel") {
            self.finish(None);
        } else {
            self.input.handle_input(data);
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn tick(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.timeout.filter(|_| self.submit.is_some()) else {
            return false;
        };
        if now >= deadline {
            self.finish(None);
            return true;
        }
        let next = seconds_ceil(deadline.duration_since(now));
        let changed = next != self.remaining;
        self.remaining = next;
        changed
    }
}

impl Focusable for InputDialog {
    fn is_focused(&self) -> bool {
        self.input.is_focused()
    }
    fn set_focused(&mut self, focused: bool) {
        self.input.set_focused(focused);
    }
}

pub fn trust_items(
    cwd: &Path,
    saved: Option<&ProjectTrustStoreEntry>,
    include_session_only: bool,
) -> (Vec<PickerItem<ProjectTrustOption>>, usize) {
    let mut initial = 0;
    let rows = get_project_trust_options(cwd, include_session_only)
        .into_iter()
        .enumerate()
        .map(|(index, option)| {
            let current = saved.is_some_and(|entry| {
                entry.decision == option.trusted
                    && option.saved_path.as_deref() == Some(&entry.path)
            });
            if current {
                initial = index;
            }
            let marker = if current { "✓ " } else { "  " };
            PickerItem {
                label: format!("{marker}{}", option.label),
                search: option.label.clone(),
                value: option,
            }
        })
        .collect();
    (rows, initial)
}

pub fn trust_picker(
    cwd: &Path,
    saved: Option<&ProjectTrustStoreEntry>,
    project_trusted: bool,
    theme: Theme,
    select: impl FnMut(Option<&ProjectTrustOption>) + 'static,
) -> Picker {
    let (rows, initial) = trust_items(cwd, saved, false);
    let cwd = normalize_cwd(cwd);
    let decision = saved.map_or_else(
        || "none".to_owned(),
        |entry| {
            let label = if entry.decision {
                "trusted"
            } else {
                "untrusted"
            };
            if Path::new(&entry.path) == cwd {
                format!("{label} ({})", entry.path)
            } else {
                format!("{label} (inherited from {})", entry.path)
            }
        },
    );
    Picker::with_items("Project trust".into(), rows, theme, select)
        .without_filter()
        .without_wrap()
        .preselect(initial)
        .description(format!(
            "{}\nSaved decision: {decision}\nCurrent session: {}",
            cwd.display(),
            if project_trusted {
                "trusted"
            } else {
                "untrusted"
            }
        ))
        .hint("↑↓ navigate · Enter save · Esc cancel")
}

const THEMES: [(&str, &str); 3] = [
    ("system", "System (matches your terminal colors)"),
    ("dark", "Dark"),
    ("light", "Light"),
];
const ANALYTICS: [&str; 2] = ["Share anonymous usage data", "Don't share"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstTimeSetupResult {
    pub theme: &'static str,
    pub share_analytics: bool,
}

type SetupComplete = Box<dyn FnMut(Option<FirstTimeSetupResult>)>;

pub struct FirstTimeSetup {
    picker: Picker,
    theme: Theme,
    chosen_theme: Option<&'static str>,
    result: Rc<Cell<Option<Option<usize>>>>,
    complete: SetupComplete,
}

impl FirstTimeSetup {
    pub fn new(theme: Theme, complete: impl FnMut(Option<FirstTimeSetupResult>) + 'static) -> Self {
        let result = Rc::new(Cell::new(None));
        let selection = Rc::clone(&result);
        let preview = theme.clone();
        let picker = Picker::new(
            format!("Welcome to {APP_NAME}, the minimal coding agent."),
            THEMES
                .iter()
                .map(|(_, label)| (*label).to_owned())
                .collect(),
            theme.clone(),
            move |index| selection.set(Some(index)),
        )
        .description("Pick a theme.")
        .without_filter()
        .without_wrap()
        .hint("↑↓ navigate · Enter continue · Esc skip setup")
        .on_highlight(move |index| {
            if let Some((name, _)) = THEMES.get(index) {
                let _ = preview.select(name);
            }
        });
        Self {
            picker,
            theme,
            chosen_theme: None,
            result,
            complete: Box::new(complete),
        }
    }
}

impl Component for FirstTimeSetup {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.picker.handle_input(data);
        let Some(index) = self.result.take() else {
            return;
        };
        match (self.chosen_theme, index) {
            (_, None) => (self.complete)(None),
            (Some(theme), Some(index)) => (self.complete)(Some(FirstTimeSetupResult {
                theme,
                share_analytics: index == 0,
            })),
            (None, Some(index)) => {
                let Some((theme, _)) = THEMES.get(index) else {
                    return;
                };
                self.chosen_theme = Some(theme);
                let selection = Rc::clone(&self.result);
                let focused = self.picker.is_focused();
                self.picker = Picker::new("Opt-in to anonymous usage data sharing?".into(), ANALYTICS.iter().map(|label| (*label).to_owned()).collect(), self.theme.clone(), move |index| selection.set(Some(index)))
                    .description("Opting in stores a tracking identifier in settings.json and enables anonymous usage analytics. You can see what is shared using /privacy.")
                    .without_filter().without_wrap()
                    .hint("↑↓ navigate · Enter finish · Esc skip setup");
                self.picker.set_focused(focused);
            }
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(&mut self.picker)
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
}
