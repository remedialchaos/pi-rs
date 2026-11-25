use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::layout_node::{LayoutNode, Overscroll, ScrollLayoutNode, ScrollLayoutState};
use crate::tui::{Component, ComponentRef, TuiMouseEvent, TuiMouseEventResult};

use super::TextStyleFn;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scrollbar {
    #[default]
    Hidden,
    Auto,
    Always,
}

pub struct ScrollViewOptions {
    pub follow_end: bool,
    pub primary: bool,
    pub overscroll: Overscroll,
    pub scrollbar: Scrollbar,
    pub scrollbar_track_style: Option<TextStyleFn>,
    pub scrollbar_thumb_style: Option<TextStyleFn>,
    pub scrollbar_hide_delay: Duration,
}

impl Default for ScrollViewOptions {
    fn default() -> Self {
        Self {
            follow_end: false,
            primary: false,
            overscroll: Overscroll::Chain,
            scrollbar: Scrollbar::Hidden,
            scrollbar_track_style: None,
            scrollbar_thumb_style: None,
            scrollbar_hide_delay: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ScrollPosition {
    top: usize,
    content: usize,
    viewport: usize,
    following: bool,
    suppress_follow: bool,
}

impl ScrollPosition {
    fn max_top(self) -> usize {
        self.content.saturating_sub(self.viewport)
    }
}

pub struct ScrollState {
    options: ScrollViewOptions,
    position: Cell<ScrollPosition>,
    scrollbar: Cell<Scrollbar>,
    active: Cell<bool>,
    visible_until: Cell<Option<Instant>>,
    request_render: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ScrollState {
    pub fn scroll_top(&self) -> usize {
        self.position.get().top
    }
    pub fn is_following_end(&self) -> bool {
        self.position.get().following
    }
    pub fn viewport_height(&self) -> usize {
        self.position.get().viewport
    }
    pub fn scrollbar(&self) -> Scrollbar {
        self.scrollbar.get()
    }
    pub fn is_scrollbar_active(&self) -> bool {
        self.active.get()
    }
    pub fn is_scrollbar_visible(&self) -> bool {
        let position = self.position.get();
        match self.scrollbar() {
            Scrollbar::Hidden => false,
            Scrollbar::Auto => {
                position.content > position.viewport
                    && (self.active.get()
                        || self.visible_until.get().is_some_and(|t| t > Instant::now()))
            }
            Scrollbar::Always => position.viewport > 0,
        }
    }

    pub fn expire_scrollbar(&self, now: Instant) {
        if self
            .visible_until
            .get()
            .is_some_and(|deadline| now >= deadline)
        {
            self.visible_until.set(None);
            self.request_render();
        }
    }

    pub fn set_scrollbar(&self, scrollbar: Scrollbar) {
        if self.scrollbar.replace(scrollbar) != scrollbar {
            self.visible_until.set(None);
            self.request_render();
        }
    }

    pub fn set_scrollbar_active(&self, active: bool) {
        if self.active.replace(active) != active {
            self.mark_scroll_activity();
            self.request_render();
        }
    }

    pub fn style_scrollbar(&self, thumb: bool) -> String {
        let (style, text, color) = if thumb {
            (
                self.options.scrollbar_thumb_style.as_ref(),
                if self.active.get() { "█" } else { "┃" },
                37,
            )
        } else {
            (self.options.scrollbar_track_style.as_ref(), "│", 90)
        };
        style.map_or_else(
            || format!("\x1b[{color}m{text}\x1b[39m"),
            |style| style(text),
        )
    }

    pub fn scroll_to(&self, top: usize, disable_follow: bool) {
        let mut next = self.position.get();
        let previous = next;
        next.top = top.min(next.max_top());
        next.suppress_follow = disable_follow && next.top == next.max_top();
        next.following =
            self.options.follow_end && next.top == next.max_top() && !next.suppress_follow;
        self.position.set(next);
        if previous.top != next.top {
            self.mark_scroll_activity();
        }
        if previous.top != next.top
            || previous.following != next.following
            || previous.suppress_follow != next.suppress_follow
        {
            self.request_render();
        }
    }

    pub fn scroll_by(&self, lines: isize) -> isize {
        if lines == 0 {
            return 0;
        }
        let position = self.position.get();
        let start = if position.following {
            position.max_top()
        } else {
            position.top
        };
        let target = start.saturating_add_signed(lines).min(position.max_top());
        self.scroll_to(target, false);
        lines - (target as isize - start as isize)
    }

    pub fn scroll_to_start(&self) {
        self.scroll_to(0, false);
    }
    pub fn scroll_to_end(&self) {
        self.scroll_to(usize::MAX, false);
    }

    fn mark_scroll_activity(&self) {
        let position = self.position.get();
        if self.scrollbar() == Scrollbar::Auto && position.content > position.viewport {
            self.visible_until.set(if self.active.get() {
                None
            } else {
                Instant::now().checked_add(self.options.scrollbar_hide_delay)
            });
        }
    }

    fn request_render(&self) {
        if let Some(callback) = self.request_render.borrow().as_ref() {
            callback();
        }
    }
}

impl ScrollLayoutState for ScrollState {
    fn follows_end(&self) -> bool {
        self.options.follow_end
    }
    fn is_following_end(&self) -> bool {
        self.is_following_end()
    }
    fn set_request_render(&self, request: Rc<dyn Fn()>) {
        *self.request_render.borrow_mut() = Some(request);
    }
    fn scrollbar(&self) -> Scrollbar {
        self.scrollbar()
    }
    fn is_scrollbar_visible(&self) -> bool {
        self.is_scrollbar_visible()
    }
    fn set_scrollbar_active(&self, active: bool) {
        self.set_scrollbar_active(active);
    }
    fn expire_scrollbar(&self, now: Instant) {
        self.expire_scrollbar(now);
    }
    fn style_scrollbar(&self, thumb: bool) -> String {
        self.style_scrollbar(thumb)
    }
    fn scroll_to(&self, top: usize, disable_follow: bool) {
        self.scroll_to(top, disable_follow);
    }
    fn scroll_by(&self, lines: isize) -> isize {
        self.scroll_by(lines)
    }
    fn primary(&self) -> bool {
        self.options.primary
    }
    fn overscroll(&self) -> Overscroll {
        self.options.overscroll
    }
    fn viewport_height(&self) -> usize {
        self.viewport_height()
    }
    fn scroll_top(&self) -> usize {
        self.scroll_top()
    }
    fn content_width(&self, width: usize) -> usize {
        if self.scrollbar() == Scrollbar::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }
    fn update_layout(&self, content_height: usize, viewport_height: usize) {
        let mut position = self.position.get();
        position.content = content_height;
        position.viewport = viewport_height;
        let max = position.max_top();
        position.top = if position.following {
            max
        } else {
            position.top.min(max)
        };
        if position.top < max {
            position.suppress_follow = false;
        }
        if self.options.follow_end && position.top == max && !position.suppress_follow {
            position.following = true;
        }
        if max == 0 {
            self.visible_until.set(None);
        }
        self.position.set(position);
    }
}

pub struct ScrollView {
    child: ComponentRef,
    pub state: Rc<ScrollState>,
}

impl ScrollView {
    pub fn new(child: ComponentRef, options: ScrollViewOptions) -> Self {
        let position = Cell::new(ScrollPosition {
            following: options.follow_end,
            ..Default::default()
        });
        let scrollbar = Cell::new(options.scrollbar);
        Self {
            child,
            state: Rc::new(ScrollState {
                options,
                position,
                scrollbar,
                active: Cell::new(false),
                visible_until: Cell::new(None),
                request_render: RefCell::new(None),
            }),
        }
    }
}

impl Component for ScrollView {
    fn children(&self) -> &[ComponentRef] {
        std::slice::from_ref(&self.child)
    }
    fn invalidate(&mut self) {
        self.child.borrow_mut().invalidate();
    }
    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Scroll(ScrollLayoutNode {
            component: self.child.clone(),
            state: self.state.clone(),
        }))
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        let width = self.state.content_width(event.width);
        if event.x >= width {
            return None;
        }
        self.child
            .borrow_mut()
            .handle_mouse(&TuiMouseEvent { width, ..*event })
    }
    fn render(&self, width: usize) -> Vec<String> {
        let content_width = self.state.content_width(width);
        let mut lines = self.child.borrow().render(content_width);
        if content_width != width {
            for line in &mut lines {
                line.push(' ');
            }
        }
        lines
    }
}
