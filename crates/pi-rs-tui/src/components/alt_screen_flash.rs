use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{tui::Component, utils::truncate_with_ellipsis};

pub struct AltScreenFlashContainer {
    entries: Vec<(String, Option<Instant>)>,
    request_render: Rc<dyn Fn()>,
}

impl AltScreenFlashContainer {
    pub fn new(request_render: Rc<dyn Fn()>) -> Self {
        Self {
            entries: Vec::new(),
            request_render,
        }
    }
    pub fn flash(&mut self, message: String, duration: Option<Duration>) {
        let deadline = Instant::now().checked_add(duration.unwrap_or(Duration::from_secs(1)));
        self.entries.push((message, deadline));
        (self.request_render)();
    }
    pub fn dispose(&mut self) {
        self.entries.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Component for AltScreenFlashContainer {
    fn render(&self, width: usize) -> Vec<String> {
        self.entries
            .iter()
            .map(|(message, _)| {
                let message = truncate_with_ellipsis(&format!(" {message} "), width, "", false);
                format!("\x1b[7m{message}\x1b[27m")
            })
            .collect()
    }
    fn tick(&mut self, now: Instant) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|(_, deadline)| deadline.is_none_or(|end| end > now));
        let changed = before != self.entries.len();
        if changed {
            (self.request_render)();
        }
        changed
    }
}
