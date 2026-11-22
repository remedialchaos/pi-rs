use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::tui::Component;

use super::{TextStyleFn, text::Text};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERVAL: Duration = Duration::from_millis(80);

#[derive(Default)]
pub struct LoaderIndicatorOptions {
    pub frames: Option<Vec<String>>,
    pub interval: Option<Duration>,
}

pub struct Loader {
    text: Text,
    spinner_color: TextStyleFn,
    message_color: TextStyleFn,
    message: String,
    frames: Vec<String>,
    interval: Duration,
    frame: usize,
    deadline: Option<Instant>,
    verbatim: bool,
    request_render: Rc<dyn Fn()>,
}

impl Loader {
    pub fn new(
        request_render: Rc<dyn Fn()>,
        spinner_color: TextStyleFn,
        message_color: TextStyleFn,
        message: String,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        let mut loader = Self {
            text: Text::new("", 1, 0, None),
            message,
            spinner_color,
            message_color,
            request_render,
            frames: Vec::new(),
            interval: INTERVAL,
            frame: 0,
            deadline: None,
            verbatim: false,
        };
        loader.set_indicator(indicator);
        loader
    }

    pub fn start(&mut self) {
        self.refresh();
        self.deadline = if self.frames.len() > 1 {
            Instant::now().checked_add(self.interval)
        } else {
            None
        };
    }

    pub fn stop(&mut self) {
        self.deadline = None;
    }

    pub fn set_message(&mut self, message: String) {
        self.message = message;
        self.refresh();
    }

    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.verbatim = indicator.is_some();
        let indicator = indicator.unwrap_or_default();
        self.frames = indicator
            .frames
            .unwrap_or_else(|| FRAMES.into_iter().map(str::to_owned).collect());
        self.interval = indicator
            .interval
            .filter(|interval| !interval.is_zero())
            .unwrap_or(INTERVAL);
        self.frame = 0;
        self.start();
    }

    fn refresh(&mut self) {
        let frame = self.frames.get(self.frame).map_or("", String::as_str);
        let mut line = if self.verbatim {
            frame.to_owned()
        } else {
            (self.spinner_color)(frame)
        };
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&(self.message_color)(&self.message));
        self.text.set_text(line);
        (self.request_render)();
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let text = self.text.render(width);
        let mut lines = Vec::with_capacity(text.len() + 1);
        lines.push(String::new());
        lines.extend(text);
        lines
    }
    fn tick(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.frame = (self.frame + 1) % self.frames.len();
            self.deadline = now.checked_add(self.interval);
            self.refresh();
            true
        } else {
            false
        }
    }
    fn invalidate(&mut self) {
        self.text.invalidate();
        self.refresh();
    }
}
