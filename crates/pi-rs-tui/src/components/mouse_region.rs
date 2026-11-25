use crate::tui::{Component, ComponentRef, TuiMouseDispatch, TuiMouseEvent, TuiMouseEventResult};

pub type MouseRegionHandler = Box<dyn FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult>>;

pub struct MouseRegion {
    child: ComponentRef,
    on_mouse: MouseRegionHandler,
}

impl MouseRegion {
    pub fn new(child: ComponentRef, on_mouse: MouseRegionHandler) -> Self {
        Self { child, on_mouse }
    }
}

impl Component for MouseRegion {
    fn children(&self) -> &[ComponentRef] {
        std::slice::from_ref(&self.child)
    }
    fn render(&self, width: usize) -> Vec<String> {
        self.child.borrow().render(width)
    }
    fn invalidate(&mut self) {
        self.child.borrow_mut().invalidate();
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        let result = self.child.borrow_mut().handle_mouse(event);
        result
            .filter(|r| r.handled || r.capture || r.focus)
            .or_else(|| (self.on_mouse)(event))
    }
    fn dispatch_mouse(
        &mut self,
        owner: &ComponentRef,
        event: &TuiMouseEvent,
    ) -> Option<TuiMouseDispatch> {
        let child = self.child.borrow_mut().dispatch_mouse(&self.child, event);
        child.or_else(|| TuiMouseDispatch::new(owner, event, (self.on_mouse)(event)?))
    }
}
