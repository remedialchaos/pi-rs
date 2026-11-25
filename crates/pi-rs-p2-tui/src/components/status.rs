use crate::theme::ThemeRef;
use pi_rs_tui::{Component, Loader, truncate_to_width};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Working,
    Retry,
    Compaction,
}
pub struct StatusIndicator {
    pub kind: StatusKind,
    loader: Loader,
    retry: Option<(std::time::Instant, u64, String, String)>,
}
impl StatusIndicator {
    pub fn new(
        kind: StatusKind,
        theme: ThemeRef,
        request_render: Rc<dyn Fn()>,
        message: String,
    ) -> Self {
        let token = if kind == StatusKind::Retry {
            "warning"
        } else {
            "accent"
        };
        let spinner = theme.clone();
        let loader = Loader::new(
            request_render,
            Rc::new(move |s| spinner.current().fg(token, s)),
            Rc::new(move |s| theme.current().fg("muted", s)),
            message,
            None,
        );
        Self {
            kind,
            loader,
            retry: None,
        }
    }
    pub fn retry(
        theme: ThemeRef,
        render: Rc<dyn Fn()>,
        attempt: u64,
        deadline: std::time::Instant,
        error: String,
        cancel: String,
    ) -> Self {
        let mut status = Self::new(StatusKind::Retry, theme, render, String::new());
        status.retry = Some((deadline, attempt, error, cancel));
        status.update_retry(std::time::Instant::now());
        status
    }
    fn update_retry(&mut self, now: std::time::Instant) {
        if let Some((deadline, attempt, error, cancel)) = &self.retry {
            let seconds = deadline
                .saturating_duration_since(now)
                .as_millis()
                .div_ceil(1000);
            self.loader.set_message(format!(
                "Retrying (attempt {}) in {seconds}s: {} ({cancel} to cancel)",
                attempt.saturating_add(1),
                crate::render_context::line_text(error)
            ));
            if now >= *deadline {
                self.retry = None;
            }
        }
    }
    pub fn render_in_border(&self, width: usize) -> String {
        let rows = self.loader.render(width.saturating_add(2));
        rows.get(1)
            .map(|row| truncate_to_width(row.trim(), width, true))
            .unwrap_or_default()
    }
    pub fn set_message(&mut self, message: String) {
        self.loader.set_message(message);
    }
    pub fn dispose(&mut self) {
        self.retry = None;
        self.loader.stop();
    }
}

impl Component for StatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }
    fn tick(&mut self, now: std::time::Instant) -> bool {
        if self.loader.tick(now) {
            self.update_retry(now);
            true
        } else {
            false
        }
    }
    fn invalidate(&mut self) {
        self.loader.invalidate()
    }
}

pub struct CustomEditor {
    pub editor: pi_rs_tui::Editor,
    pub status: Option<StatusIndicator>,
    pub actions: indexmap::IndexMap<String, Box<dyn FnMut()>>,
}
impl Component for CustomEditor {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.editor.render(width);
        if let Some(status) = &self.status
            && let Some(border) = lines.first_mut()
            && width >= 5
        {
            let text = status.render_in_border(width - 4);
            let space = pi_rs_tui::visible_width(&text);
            if space > 0 && !pi_rs_tui::strip_terminal_sequences(border).contains(" more ") {
                *border = format!("── {text} {}", "─".repeat(width.saturating_sub(space + 4)));
            }
        }
        lines
    }
    fn handle_input(&mut self, data: &str) {
        use pi_rs_tui::EditorComponent;
        let keys = pi_rs_tui::get_keybindings();
        let history = ["tui.editor.historyPrevious", "tui.editor.historyNext"]
            .iter()
            .any(|id| keys.matches(data, id));
        if !history
            && let Some((_, handler)) = self.actions.iter_mut().find(|(id, _)| {
                keys.matches(data, id)
                    && (id.as_str() != "app.exit" || self.editor.text().is_empty())
                    && (id.as_str() != "app.interrupt" || !self.editor.is_showing_autocomplete())
            })
        {
            handler();
        } else {
            self.editor.handle_input(data);
        }
    }
    fn as_focusable(&mut self) -> Option<&mut dyn pi_rs_tui::Focusable> {
        self.editor.as_focusable()
    }
    fn handle_mouse(
        &mut self,
        event: &pi_rs_tui::TuiMouseEvent,
    ) -> Option<pi_rs_tui::TuiMouseEventResult> {
        self.editor.handle_mouse(event)
    }
    fn captures_viewport_input(&self) -> bool {
        self.editor.captures_viewport_input()
    }
    fn tick(&mut self, now: std::time::Instant) -> bool {
        self.editor.tick(now) | self.status.as_mut().is_some_and(|s| s.tick(now))
    }
    fn invalidate(&mut self) {
        self.editor.invalidate();
    }
}
