//! Layout metadata reported by viewport-aware components.

use crate::tui::ComponentRef;
use std::rc::Rc;
use std::time::Instant;

use crate::components::scroll_view::Scrollbar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackAxis {
    Horizontal,
    #[default]
    Vertical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackAlign {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackBasis {
    #[default]
    Auto,
    Cells(usize),
}

#[derive(Clone)]
pub struct StackLayoutEntry {
    pub component: ComponentRef,
    pub basis: StackBasis,
    pub grow: f64,
    pub shrink: f64,
    pub min_size: Option<usize>,
    pub max_size: Option<usize>,
    pub visible: Option<std::rc::Rc<dyn Fn(LayoutViewport) -> bool>>,
}

pub struct StackLayoutNode {
    pub axis: StackAxis,
    pub entries: Vec<StackLayoutEntry>,
    pub gap: usize,
    pub align: StackAlign,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overscroll {
    #[default]
    Chain,
    Contain,
}

pub trait ScrollLayoutState {
    fn scroll_top(&self) -> usize;
    fn follows_end(&self) -> bool;
    fn is_following_end(&self) -> bool;
    fn primary(&self) -> bool;
    fn overscroll(&self) -> Overscroll;
    fn viewport_height(&self) -> usize;
    fn content_width(&self, width: usize) -> usize;
    fn update_layout(&self, content_height: usize, viewport_height: usize);
    fn set_request_render(&self, request: Rc<dyn Fn()>);
    fn scrollbar(&self) -> Scrollbar;
    fn is_scrollbar_visible(&self) -> bool;
    fn set_scrollbar_active(&self, active: bool);
    fn expire_scrollbar(&self, now: Instant);
    fn style_scrollbar(&self, thumb: bool) -> String;
    fn scroll_to(&self, top: usize, disable_follow: bool);
    fn scroll_by(&self, lines: isize) -> isize;
}

pub struct ScrollLayoutNode {
    pub component: ComponentRef,
    pub state: std::rc::Rc<dyn ScrollLayoutState>,
}

pub enum LayoutNode {
    Stack(StackLayoutNode),
    Scroll(ScrollLayoutNode),
}

pub fn get_layout_node(component: &ComponentRef) -> Option<LayoutNode> {
    component.borrow().layout_node()
}
