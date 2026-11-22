use std::sync::{Arc, Mutex};

use pi_rs_tui::Terminal;

#[derive(Clone)]
pub struct VirtualTerminal {
    pub parser: Arc<Mutex<vt100::Parser>>,
}

impl VirtualTerminal {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 100))),
        }
    }
}

impl Terminal for VirtualTerminal {
    fn width(&self) -> usize {
        usize::from(
            self.parser
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .screen()
                .size()
                .1,
        )
    }
    fn height(&self) -> usize {
        usize::from(
            self.parser
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .screen()
                .size()
                .0,
        )
    }
    fn is_tty(&self) -> bool {
        true
    }
    fn read_available(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn write(&mut self, data: &str) {
        self.parser
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .process(data.as_bytes());
    }
}
