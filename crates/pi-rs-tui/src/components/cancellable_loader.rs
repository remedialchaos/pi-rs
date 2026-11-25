use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::keybindings::get_keybindings;
use crate::tui::Component;

use super::loader::Loader;

#[derive(Clone, Default)]
pub struct CancellationSignal(Arc<AtomicBool>);

impl CancellationSignal {
    pub fn is_aborted(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub struct CancellableLoader {
    pub loader: Loader,
    signal: CancellationSignal,
    pub on_abort: Option<Box<dyn FnMut()>>,
}

impl CancellableLoader {
    pub fn new(loader: Loader) -> Self {
        Self {
            loader,
            signal: CancellationSignal::default(),
            on_abort: None,
        }
    }
    pub fn signal(&self) -> CancellationSignal {
        self.signal.clone()
    }
    pub fn aborted(&self) -> bool {
        self.signal.is_aborted()
    }
    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }
    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
    fn tick(&mut self, now: Instant) -> bool {
        self.loader.tick(now)
    }
    fn handle_input(&mut self, data: &str) {
        if get_keybindings().matches(data, "tui.select.cancel") {
            self.signal.0.store(true, Ordering::Release);
            if let Some(on_abort) = &mut self.on_abort {
                on_abort();
            }
        }
    }
}
