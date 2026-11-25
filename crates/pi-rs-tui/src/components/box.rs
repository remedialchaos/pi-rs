use crate::tui::{
    Component, ComponentRef, Container, TuiMouseDispatch, TuiMouseEvent, TuiMouseEventResult,
};
use crate::utils::visible_width;

use super::TextStyleFn;

pub struct Box {
    children: Container,
    padding_x: usize,
    padding_y: usize,
    background: Option<TextStyleFn>,
}

impl Default for Box {
    fn default() -> Self {
        Self::new(1, 1, None)
    }
}

impl Box {
    pub fn new(padding_x: usize, padding_y: usize, background: Option<TextStyleFn>) -> Self {
        Self {
            children: Container::default(),
            padding_x,
            padding_y,
            background,
        }
    }
    pub fn add_child(&mut self, child: ComponentRef) {
        self.children.add_child(child);
    }
    pub fn remove_child(&mut self, child: &ComponentRef) {
        self.children.remove_child(child);
    }
    pub fn clear(&mut self) {
        self.children.clear();
    }
    pub fn set_bg_fn(&mut self, background: Option<TextStyleFn>) {
        self.background = background;
    }
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
    }
}

impl Component for Box {
    fn children(&self) -> &[ComponentRef] {
        self.children.children()
    }
    fn invalidate(&mut self) {
        self.children.invalidate();
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        self.route_mouse(event).map(|dispatch| dispatch.result)
    }
    fn dispatch_mouse(
        &mut self,
        _owner: &ComponentRef,
        event: &TuiMouseEvent,
    ) -> Option<TuiMouseDispatch> {
        self.route_mouse(event)
    }
    fn render(&self, width: usize) -> Vec<String> {
        self.render_box(width)
    }
}

impl Box {
    fn route_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseDispatch> {
        let x = event.x.checked_sub(self.padding_x)?;
        let y = event.y.checked_sub(self.padding_y)?;
        let width = event
            .width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        if x >= width {
            return None;
        }
        self.children.dispatch_to_child(&TuiMouseEvent {
            x,
            y,
            width,
            ..*event
        })
    }
    fn render_box(&self, width: usize) -> Vec<String> {
        let content = self.children.render(
            width
                .saturating_sub(self.padding_x.saturating_mul(2))
                .max(1),
        );
        if content.is_empty() {
            return content;
        }
        let mut lines = Vec::with_capacity(content.len() + 2 * self.padding_y);
        let style = |line: String| match &self.background {
            Some(background) => background(&line),
            None => line,
        };
        for _ in 0..self.padding_y {
            lines.push(style(" ".repeat(width)));
        }
        for line in content {
            let tail = width.saturating_sub(self.padding_x + visible_width(&line));
            let mut output = String::with_capacity(self.padding_x + line.len() + tail);
            output.extend(std::iter::repeat_n(' ', self.padding_x));
            output.push_str(&line);
            output.extend(std::iter::repeat_n(' ', tail));
            lines.push(style(output));
        }
        for _ in 0..self.padding_y {
            lines.push(style(" ".repeat(width)));
        }
        lines
    }
}
