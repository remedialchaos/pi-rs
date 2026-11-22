use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use indexmap::IndexMap;
use pi_rs_tui::{
    Component, Editor, EditorComponent, Focusable, Loader, SelectListTheme, TuiMouseEvent,
    TuiMouseEventResult, get_keybindings, is_key_release, parse_key, replace_control_characters,
    truncate_to_width, visible_width,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::core::{
    agent_session::{AgentSession, SessionStats},
    footer_data::resolve_git_branch,
    keybindings::{AppKeybinding, KeybindingsManager},
};

use super::{UiEvent, theme::Theme, transcript::Transcript};

pub(super) struct TranscriptView(pub Rc<RefCell<Transcript>>);

impl Component for TranscriptView {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.borrow_mut().render(width)
    }
    fn invalidate(&mut self) {
        self.0.borrow_mut().invalidate();
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        self.0.borrow_mut().handle_mouse(event)
    }
}

pub(super) struct DynamicText(pub Rc<RefCell<Vec<String>>>);

impl Component for DynamicText {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .borrow()
            .iter()
            .flat_map(|line| pi_rs_tui::wrap_text_with_ansi(line, width.max(1)))
            .collect()
    }
}

pub(super) struct CustomEditor {
    pub(super) editor: Editor,
    pub(super) keys: KeybindingsManager,
    pub(super) shortcuts: Vec<String>,
    pub(super) events: UnboundedSender<UiEvent>,
}

const ACTIONS: &[AppKeybinding] = &[
    AppKeybinding::Interrupt,
    AppKeybinding::Clear,
    AppKeybinding::Exit,
    AppKeybinding::Suspend,
    AppKeybinding::ThinkingCycle,
    AppKeybinding::ThinkingSave,
    AppKeybinding::ModelCycleForward,
    AppKeybinding::ModelCycleBackward,
    AppKeybinding::ModelSelect,
    AppKeybinding::ToolsExpand,
    AppKeybinding::ThinkingToggle,
    AppKeybinding::EditorExternal,
    AppKeybinding::MessageCopy,
    AppKeybinding::MessageFollowUp,
    AppKeybinding::MessageDequeue,
    AppKeybinding::ClipboardPasteImage,
    AppKeybinding::SessionNew,
    AppKeybinding::SessionTree,
    AppKeybinding::SessionFork,
    AppKeybinding::SessionResume,
];

impl Component for CustomEditor {
    fn render(&self, width: usize) -> Vec<String> {
        self.editor.render(width)
    }
    fn handle_input(&mut self, data: &str) {
        if let Some(key) = parse_key(data) {
            if is_key_release(&key) {
                return;
            }
            if let Some(shortcut) = self
                .shortcuts
                .iter()
                .find(|id| pi_rs_tui::matches_key(&key, id))
            {
                let _ = self.events.send(UiEvent::Shortcut(shortcut.clone()));
                return;
            }
        }
        let tui = get_keybindings();
        if tui.matches(data, "tui.editor.historyPrevious")
            || tui.matches(data, "tui.editor.historyNext")
            || self.editor.is_showing_autocomplete()
                && [
                    "tui.select.pageUp",
                    "tui.select.pageDown",
                    "tui.select.cancel",
                ]
                .into_iter()
                .any(|id| tui.matches(data, id))
        {
            self.editor.handle_input(data);
            return;
        }
        if let Some(action) = ACTIONS.iter().copied().find(|action| {
            (*action != AppKeybinding::Exit || self.editor.text().is_empty())
                && self.keys.matches(data, action.as_str())
        }) {
            let _ = self.events.send(UiEvent::Key(action));
        } else {
            self.editor.handle_input(data);
        }
    }
    fn tick(&mut self, now: Instant) -> bool {
        self.editor.tick(now)
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        self.editor.handle_mouse(event)
    }
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        self.editor.as_focusable()
    }
    fn captures_viewport_input(&self) -> bool {
        self.editor.captures_viewport_input()
    }
    fn invalidate(&mut self) {
        self.editor.invalidate();
    }
}

pub(super) fn select_theme(theme: &Theme) -> SelectListTheme {
    let text = theme.clone();
    let description = theme.clone();
    let scroll = theme.clone();
    let empty = theme.clone();
    SelectListTheme {
        selected_text: Rc::new(move |s| text.fg("accent", s)),
        description: Rc::new(move |s| description.fg("muted", s)),
        scroll_info: Rc::new(move |s| scroll.fg("dim", s)),
        no_match: Rc::new(move |s| empty.fg("warning", s)),
    }
}

pub(super) struct Status {
    loader: Option<Loader>,
    retry: Option<(Instant, usize, usize)>,
    theme: Theme,
}

impl Status {
    pub(super) fn new(theme: Theme) -> Self {
        Self {
            loader: None,
            retry: None,
            theme,
        }
    }
    pub(super) fn clear(&mut self) {
        self.loader = None;
        self.retry = None;
    }
    pub(super) fn show(&mut self, message: &str, warning: bool) {
        self.retry = None;
        let spinner = self.theme.clone();
        let text = self.theme.clone();
        let loader = Loader::new(
            Rc::new(|| {}),
            Rc::new(move |s| spinner.fg(if warning { "warning" } else { "accent" }, s)),
            Rc::new(move |s| text.fg("muted", s)),
            message.to_owned(),
            None,
        );
        self.loader = Some(loader);
    }
    pub(super) fn retry(&mut self, attempt: usize, max: usize, delay: u64) {
        let deadline = Instant::now() + Duration::from_millis(delay);
        self.show(&retry_message(attempt, max, delay), true);
        self.retry = Some((deadline, attempt, max));
    }
}

fn retry_message(attempt: usize, max: usize, delay: u64) -> String {
    format!(
        "Retrying ({attempt}/{max}) in {}s... (esc to cancel)",
        delay.div_ceil(1000)
    )
}

impl Component for Status {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader
            .as_ref()
            .map_or_else(Vec::new, |loader| loader.render(width))
    }
    fn tick(&mut self, now: Instant) -> bool {
        if let Some(loader) = &mut self.loader {
            if let Some((deadline, attempt, max)) = self.retry {
                let delay = deadline.saturating_duration_since(now).as_millis() as u64;
                loader.set_message(retry_message(attempt, max, delay));
            }
            loader.tick(now)
        } else {
            false
        }
    }
}

pub(super) struct Footer {
    theme: Theme,
    location: String,
    left: Vec<(String, &'static str)>,
    model: String,
    provider: String,
    pub(super) statuses: IndexMap<String, String>,
}

impl Footer {
    pub(super) fn new(theme: Theme, session: &AgentSession) -> Self {
        let mut footer = Self {
            theme,
            location: String::new(),
            left: Vec::new(),
            model: String::new(),
            provider: String::new(),
            statuses: IndexMap::new(),
        };
        footer.refresh(session, true);
        footer
    }

    pub(super) fn refresh(&mut self, session: &AgentSession, check_location: bool) {
        if check_location {
            let cwd = session.cwd();
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
            self.location = if let Some(relative) = home
                .as_ref()
                .and_then(|home| Path::new(&cwd).strip_prefix(home).ok())
            {
                if relative.as_os_str().is_empty() {
                    "~".into()
                } else {
                    format!(
                        "~{}{relative}",
                        std::path::MAIN_SEPARATOR,
                        relative = relative.display()
                    )
                }
            } else {
                cwd.clone()
            };
            if let Some(branch) = resolve_git_branch(Path::new(&cwd)) {
                self.location.push_str(&format!(" ({branch})"));
            }
            if let Some(name) = session_name(session) {
                self.location.push_str(&format!(" • {name}"));
            }
            self.location = replace_control_characters(&self.location);
        }
        let SessionStats {
            tokens,
            cost,
            context_usage,
            ..
        } = session.stats();
        self.left.clear();
        for (prefix, count) in [
            ("↑", tokens.input),
            ("↓", tokens.output),
            ("R", tokens.cache_read),
            ("W", tokens.cache_write),
        ] {
            if count > 0. {
                self.left
                    .push((format!("{prefix}{}", format_tokens(count)), "dim"));
            }
        }
        if cost > 0. {
            self.left.push((format!("${cost:.3}"), "dim"));
        }
        let model = session.model();
        let usage = context_usage.as_ref().and_then(|usage| usage.percent);
        let window = context_usage.map_or(model.context_window, |usage| usage.context_window);
        let percent = usage.map_or_else(|| "?".into(), |n| format!("{n:.1}%"));
        let auto = if session.settings().compaction_enabled() {
            " (auto)"
        } else {
            ""
        };
        let color = match usage {
            Some(n) if n > 90. => "error",
            Some(n) if n > 70. => "warning",
            _ => "dim",
        };
        self.left.push((
            format!("{percent}/{}{auto}", format_tokens(window as f64)),
            color,
        ));
        self.model = replace_control_characters(&model.id);
        self.provider.clear();
        if session
            .available_models()
            .iter()
            .any(|other| other.provider != model.provider)
        {
            self.provider = format!("({}) ", replace_control_characters(&model.provider));
        }
        if model.reasoning {
            let level = session.thinking_level();
            if level == pi_rs_ai::ThinkingLevel::Off {
                self.model.push_str(" • thinking off");
            } else {
                self.model.push_str(" • ");
                self.model.push_str(level.as_str());
            }
        }
    }
}

impl Component for Footer {
    fn render(&self, width: usize) -> Vec<String> {
        let mut stats = String::new();
        for (text, color) in &self.left {
            if !stats.is_empty() {
                stats.push(' ');
            }
            stats.push_str(&self.theme.fg(color, text));
        }
        let mut stats = truncate_to_width(&stats, width, true);
        let available = width.saturating_sub(visible_width(&stats) + 2);
        if available > 0 {
            let model = if visible_width(&self.provider) + visible_width(&self.model) <= available {
                format!("{}{}", self.provider, self.model)
            } else {
                truncate_to_width(&self.model, available, true)
            };
            stats.push_str(&" ".repeat(available.saturating_sub(visible_width(&model)) + 2));
            stats.push_str(&self.theme.fg("dim", &model));
        }
        let mut lines = vec![
            self.theme
                .fg("dim", &truncate_to_width(&self.location, width, true)),
            stats,
        ];
        if !self.statuses.is_empty() {
            let mut statuses = self.statuses.iter().collect::<Vec<_>>();
            statuses.sort_unstable_by_key(|(key, _)| *key);
            let text = statuses
                .into_iter()
                .flat_map(|(_, text)| text.split_whitespace())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(truncate_to_width(&text, width, true));
        }
        lines
    }
}

pub(super) fn session_name(session: &AgentSession) -> Option<String> {
    session
        .session_manager()
        .disk()
        .and_then(|disk| disk.lock().get_session_name().map(str::to_owned))
}

fn format_tokens(count: f64) -> String {
    if count < 1_000. {
        format!("{count:.0}")
    } else if count < 10_000. {
        format!("{:.1}k", count / 1_000.)
    } else if count < 1_000_000. {
        format!("{:.0}k", (count / 1_000.).round())
    } else if count < 10_000_000. {
        format!("{:.1}M", count / 1_000_000.)
    } else {
        format!("{:.0}M", (count / 1_000_000.).round())
    }
}
