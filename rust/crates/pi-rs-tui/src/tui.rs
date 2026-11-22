//! Shared component, focus, overlay, and terminal state for both renderers.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use regex::Regex;

use crate::keys::{is_key_release_data, matches_key_data};
use crate::layout_node::LayoutNode;
use crate::terminal::{
    KeyboardProtocolNegotiationSequence, Terminal, parse_keyboard_protocol_negotiation_sequence,
};
use crate::terminal_colors::{
    OscColorTarget, RgbColor, TerminalColorScheme, TerminalColors, parse_osc_color_response,
    parse_terminal_color_scheme_report,
};
use crate::terminal_image::{
    CellDimensions, get_capabilities, get_image_line_extent, get_max_kitty_image_rows,
    is_image_line, set_cell_dimensions, strip_image_sequences,
};
use crate::utils::{
    extract_segments, normalize_terminal_output, slice_by_column, slice_with_width, visible_width,
};

pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";
pub const FAKE_CURSOR_START: &str = "\x1b_pi:fc\x07";
pub const FAKE_CURSOR_END: &str = "\x1b_pi:/fc\x07";
pub(crate) const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub fn render_fake_cursor(text: &str) -> String {
    format!("{FAKE_CURSOR_START}{text}{FAKE_CURSOR_END}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiMouseEventType {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiMouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuiMouseEvent {
    pub event_type: TuiMouseEventType,
    pub button: TuiMouseButton,
    pub x: usize,
    pub y: usize,
    pub screen_x: usize,
    pub screen_y: usize,
    pub width: usize,
    pub height: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub wheel_delta: Option<i32>,
    pub click_count: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TuiMouseEventResult {
    pub handled: bool,
    pub capture: bool,
    pub focus: bool,
    pub render: Option<bool>,
}

/// The component and local geometry that actually handled a routed mouse event.
pub struct TuiMouseDispatch {
    pub target: ComponentRef,
    pub event: TuiMouseEvent,
    pub result: TuiMouseEventResult,
}

impl TuiMouseDispatch {
    pub(crate) fn new(
        target: &ComponentRef,
        event: &TuiMouseEvent,
        result: TuiMouseEventResult,
    ) -> Option<Self> {
        if !(result.handled || result.capture || result.focus) {
            return None;
        }
        Some(Self {
            target: target.clone(),
            event: *event,
            result: TuiMouseEventResult {
                handled: true,
                ..result
            },
        })
    }
}

pub trait Focusable {
    fn is_focused(&self) -> bool;
    fn set_focused(&mut self, focused: bool);
}

pub trait Component {
    fn render(&self, width: usize) -> Vec<String>;
    fn handle_input(&mut self, _data: &str) {}
    fn handle_mouse(&mut self, _event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        None
    }
    fn dispatch_mouse(
        &mut self,
        owner: &ComponentRef,
        event: &TuiMouseEvent,
    ) -> Option<TuiMouseDispatch> {
        TuiMouseDispatch::new(owner, event, self.handle_mouse(event)?)
    }
    fn wants_key_release(&self) -> bool {
        false
    }
    fn captures_viewport_input(&self) -> bool {
        false
    }
    fn invalidate(&mut self) {}
    fn tick(&mut self, _now: Instant) -> bool {
        false
    }
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        None
    }
    fn layout_node(&self) -> Option<LayoutNode> {
        None
    }
    fn children(&self) -> &[ComponentRef] {
        &[]
    }
}

/// Identity handle allowing focus and overlays to refer to components owned elsewhere in the tree.
#[derive(Clone)]
pub struct ComponentRef(Rc<RefCell<dyn Component>>);

impl ComponentRef {
    pub fn new(component: impl Component + 'static) -> Self {
        Self(Rc::new(RefCell::new(component)))
    }

    pub fn from_shared<T: Component + 'static>(component: Rc<RefCell<T>>) -> Self {
        Self(component)
    }

    pub fn borrow(&self) -> Ref<'_, dyn Component> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> RefMut<'_, dyn Component> {
        self.0.borrow_mut()
    }

    pub(crate) fn identity(&self) -> *const () {
        Rc::as_ptr(&self.0).cast()
    }

    pub(crate) fn tick_tree(&self, now: Instant) -> bool {
        let mut component = self.borrow_mut();
        let mut dirty = component.tick(now);
        for child in component.children() {
            dirty |= child.tick_tree(now);
        }
        dirty
    }

    pub fn contains(&self, target: &Self) -> bool {
        self == target
            || self
                .borrow()
                .children()
                .iter()
                .any(|child| child.contains(target))
    }
}

impl PartialEq for ComponentRef {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ComponentRef {}

impl std::hash::Hash for ComponentRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.identity(), state);
    }
}

#[derive(Default)]
pub struct Container {
    children: Vec<ComponentRef>,
    /// Row counts from the last render, keyed by viewport width.
    heights: RefCell<Option<(usize, Vec<usize>)>>,
}

impl Container {
    pub fn add_child(&mut self, child: ComponentRef) {
        self.children.push(child);
        self.heights.get_mut().take();
    }
    pub fn remove_child(&mut self, child: &ComponentRef) {
        if let Some(i) = self
            .children
            .iter()
            .position(|candidate| candidate == child)
        {
            self.children.remove(i);
            self.heights.get_mut().take();
        }
    }
    pub fn clear(&mut self) {
        self.children.clear();
        self.heights.get_mut().take();
    }

    pub(crate) fn dispatch_to_child(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseDispatch> {
        if event.y >= event.height {
            return None;
        }
        let mut y = event.y;
        for (i, child) in self.children.iter().enumerate() {
            let height = self
                .heights
                .get_mut()
                .as_ref()
                .filter(|(width, _)| *width == event.width)
                .and_then(|(_, heights)| heights.get(i))
                .copied()
                .unwrap_or_else(|| child.borrow().render(event.width).len());
            if y < height {
                return child.borrow_mut().dispatch_mouse(
                    child,
                    &TuiMouseEvent {
                        y,
                        height,
                        ..*event
                    },
                );
            }
            y -= height;
        }
        None
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut cache = self.heights.borrow_mut();
        let (_, heights) = cache.get_or_insert_with(|| (width, Vec::new()));
        heights.clear();
        let mut lines = Vec::new();
        for child in &self.children {
            let child_lines = child.borrow().render(width);
            heights.push(child_lines.len());
            lines.extend(child_lines);
        }
        if let Some((previous_width, _)) = cache.as_mut() {
            *previous_width = width;
        }
        lines
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        self.dispatch_to_child(event)
            .map(|dispatch| dispatch.result)
    }
    fn dispatch_mouse(
        &mut self,
        _owner: &ComponentRef,
        event: &TuiMouseEvent,
    ) -> Option<TuiMouseDispatch> {
        self.dispatch_to_child(event)
    }
    fn invalidate(&mut self) {
        self.heights.get_mut().take();
        for child in &self.children {
            child.borrow_mut().invalidate();
        }
    }
    fn children(&self) -> &[ComponentRef] {
        &self.children
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

impl From<usize> for OverlayMargin {
    fn from(cells: usize) -> Self {
        Self {
            top: cells,
            right: cells,
            bottom: cells,
            left: cells,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Cells(isize),
    Percent(f64),
}

impl SizeValue {
    fn resolve(self, reference: usize) -> isize {
        match self {
            Self::Cells(n) => n,
            Self::Percent(pct) => (reference as f64 * pct.max(0.0) / 100.0).floor() as isize,
        }
    }
}

pub type OverlayVisibility = Box<dyn Fn(usize, usize) -> bool>;

#[derive(Default)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: isize,
    pub offset_y: isize,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: OverlayMargin,
    pub visible: Option<OverlayVisibility>,
    pub non_capturing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverlayHandle(u64);

pub struct OverlayUnfocusOptions {
    pub target: Option<ComponentRef>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayBounds {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

impl OverlayOptions {
    pub fn resolve(
        &self,
        height: usize,
        columns: usize,
        rows: usize,
    ) -> (OverlayBounds, Option<usize>) {
        let OverlayMargin {
            top,
            right,
            bottom,
            left,
        } = self.margin;
        let available_width = columns.saturating_sub(left.saturating_add(right)).max(1);
        let available_height = rows.saturating_sub(top.saturating_add(bottom)).max(1);
        let width = self
            .width
            .map_or(80, |width| width.resolve(columns))
            .max(self.min_width.unwrap_or(1) as isize)
            .clamp(1, available_width as isize) as usize;
        let max_height = self
            .max_height
            .map(|h| h.resolve(rows).clamp(1, available_height as isize) as usize);
        let height = height.min(max_height.unwrap_or(height));
        let max_row = available_height.saturating_sub(height);
        let max_col = available_width.saturating_sub(width);
        use OverlayAnchor::*;
        let anchor_row = match self.anchor {
            TopLeft | TopRight | TopCenter => 0,
            BottomLeft | BottomRight | BottomCenter => max_row,
            _ => max_row / 2,
        };
        let anchor_col = match self.anchor {
            TopLeft | BottomLeft | LeftCenter => 0,
            TopRight | BottomRight | RightCenter => max_col,
            _ => max_col / 2,
        };
        let position = |value, offset: isize, margin: usize, max: usize, anchor: usize| {
            let pos = match value {
                None => margin.saturating_add(anchor) as isize,
                Some(SizeValue::Cells(cells)) => cells,
                Some(pct) => (margin as isize).saturating_add(pct.resolve(max)),
            };
            pos.saturating_add(offset)
                .clamp(margin as isize, margin.saturating_add(max) as isize) as usize
        };
        let row = position(self.row, self.offset_y, top, max_row, anchor_row);
        let col = position(self.col, self.offset_x, left, max_col, anchor_col);
        (
            OverlayBounds {
                row,
                col,
                width,
                height,
            },
            max_height,
        )
    }
}

struct Overlay {
    id: OverlayHandle,
    component: ComponentRef,
    options: OverlayOptions,
    pre_focus: Option<ComponentRef>,
    hidden: bool,
    order: u64,
    bounds: Option<OverlayBounds>,
}

impl Overlay {
    fn visible(&self, width: usize, height: usize) -> bool {
        !self.hidden
            && self
                .options
                .visible
                .as_ref()
                .is_none_or(|test| test(width, height))
    }
}

struct FocusRestore {
    overlay: OverlayHandle,
    blocker: Option<ComponentRef>,
    target: Option<Option<ComponentRef>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TuiStopOptions {
    pub preserve_screen: bool,
}

pub trait Tui: Component {
    fn mode(&self) -> TuiMode;
    fn base(&self) -> &TuiBase;
    fn base_mut(&mut self) -> &mut TuiBase;
    fn start(&mut self) -> io::Result<()>;
    fn stop(&mut self, options: TuiStopOptions) -> io::Result<()>;
    fn render_now(&mut self, force: bool) -> io::Result<()>;
    fn request_render(&mut self, force: bool) {
        self.base_mut().request_render(force);
    }
}

pub trait ViewportTui: Tui {
    fn set_layout_root(&mut self, component: Option<ComponentRef>);
}

#[derive(Default)]
pub struct TuiInputListenerResult {
    pub consume: bool,
    pub data: Option<String>,
}
pub type TuiInputListener = Box<dyn FnMut(&str) -> TuiInputListenerResult>;
pub type ColorSchemeListener = Box<dyn FnMut(TerminalColorScheme)>;
pub type ColorQueryListener = Box<dyn FnOnce(TerminalColors)>;

struct ColorQuery {
    colors: TerminalColors,
    palette: [Option<RgbColor>; 16],
    replies: u32,
    deadline: Instant,
    on_time: Option<ColorQueryListener>,
    on_late: Option<ColorQueryListener>,
}

impl ColorQuery {
    fn report(&self) -> TerminalColors {
        TerminalColors {
            palette: self.palette.into_iter().collect(),
            ..self.colors.clone()
        }
    }
    fn deliver(&mut self) {
        if let Some(callback) = self.on_time.take().or_else(|| self.on_late.take()) {
            self.on_late = None;
            callback(self.report());
        }
    }
}

pub struct TuiBase {
    pub terminal: Box<dyn Terminal>,
    pub content: Container,
    pub on_debug: Option<Box<dyn FnMut()>>,
    focused: Option<ComponentRef>,
    overlays: Vec<Overlay>,
    restore: Option<FocusRestore>,
    order: u64,
    input_listeners: IndexMap<u64, TuiInputListener>,
    color_listeners: IndexMap<u64, ColorSchemeListener>,
    color_queries: VecDeque<ColorQuery>,
    color_notifications: bool,
    exit_frame: (&'static str, &'static str),
    #[cfg(unix)]
    suspend: Option<crate::terminal::SuspendSignal>,
    #[cfg(unix)]
    on_suspend: Option<Box<dyn FnMut()>>,
    #[cfg(unix)]
    owns_terminal: bool,
    show_hardware_cursor: bool,
    clear_on_shrink: bool,
    pub(crate) stopped: bool,
    pub(crate) full_redraws: usize,
    pub(crate) render_requested: bool,
    pub(crate) force_render: bool,
    pub(crate) content_changed: bool,
    last_render: Option<Instant>,
}

impl TuiBase {
    pub(crate) fn tick_components(&mut self, now: Instant) {
        #[cfg(unix)]
        if self
            .suspend
            .as_ref()
            .is_some_and(crate::terminal::SuspendSignal::take)
            && let Some(handler) = &mut self.on_suspend
        {
            handler();
        }
        let mut dirty = false;
        for component in self.content.children() {
            dirty |= component.tick_tree(now);
        }
        for overlay in &self.overlays {
            if self.visible(overlay) {
                dirty |= overlay.component.tick_tree(now);
            }
        }
        if dirty {
            self.request_render(false);
        }
    }

    pub fn new(terminal: Box<dyn Terminal>, show_hardware_cursor: bool) -> Self {
        Self {
            terminal,
            content: Container::default(),
            on_debug: None,
            focused: None,
            overlays: Vec::new(),
            restore: None,
            order: 0,
            input_listeners: IndexMap::new(),
            color_listeners: IndexMap::new(),
            color_queries: VecDeque::new(),
            color_notifications: false,
            exit_frame: ("", ""),
            #[cfg(unix)]
            suspend: None,
            #[cfg(unix)]
            on_suspend: None,
            #[cfg(unix)]
            owns_terminal: false,
            show_hardware_cursor,
            clear_on_shrink: false,
            stopped: false,
            full_redraws: 0,
            render_requested: false,
            force_render: false,
            content_changed: true,
            last_render: None,
        }
    }

    pub fn full_redraws(&self) -> usize {
        self.full_redraws
    }
    pub fn show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
    }
    pub fn clear_on_shrink(&self) -> bool {
        self.clear_on_shrink
    }
    pub fn set_clear_on_shrink(&mut self, clear: bool) {
        self.clear_on_shrink = clear;
    }
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.show_hardware_cursor != enabled {
            self.show_hardware_cursor = enabled;
            if !enabled && !self.stopped {
                self.terminal.write("\x1b[?25l");
            }
            self.request_render(false);
        }
    }

    pub fn request_render(&mut self, force: bool) {
        self.content_changed = true;
        self.render_requested = true;
        self.force_render |= force;
    }

    pub fn request_view_render(&mut self) {
        self.render_requested = true;
    }
    pub fn render_due(&self, now: Instant) -> bool {
        !self.stopped
            && self.render_requested
            && (self.force_render
                || self
                    .last_render
                    .is_none_or(|last| now.saturating_duration_since(last) >= FRAME_INTERVAL))
    }
    pub(crate) fn begin_frame(&mut self, force: bool) -> bool {
        self.last_render = Some(Instant::now());
        self.render_requested = false;
        std::mem::take(&mut self.force_render) || force
    }
    pub fn focused(&self) -> Option<&ComponentRef> {
        self.focused.as_ref()
    }
    pub fn has_overlay_entries(&self) -> bool {
        !self.overlays.is_empty()
    }
    fn visible(&self, entry: &Overlay) -> bool {
        entry.visible(self.terminal.width(), self.terminal.height())
    }
    pub fn has_overlay(&self) -> bool {
        self.overlays.iter().any(|entry| self.visible(entry))
    }
    pub fn is_overlay_focused(&self) -> bool {
        self.overlays
            .iter()
            .any(|entry| Some(&entry.component) == self.focused.as_ref() && self.visible(entry))
    }

    fn overlay(&self, id: OverlayHandle) -> Option<&Overlay> {
        self.overlays.iter().find(|entry| entry.id == id)
    }
    fn top_overlay(&self, exclude: Option<OverlayHandle>) -> Option<&Overlay> {
        self.overlays
            .iter()
            .filter(|o| Some(o.id) != exclude && !o.options.non_capturing && self.visible(o))
            .max_by_key(|o| o.order)
    }
    fn apply_focus(&mut self, next: Option<ComponentRef>) {
        self.content_changed = true;
        if let Some(old) = self.focused.take()
            && let Some(focusable) = old.borrow_mut().as_focusable()
        {
            focusable.set_focused(false);
        }
        self.focused = next;
        if let Some(next) = &self.focused
            && let Some(focusable) = next.borrow_mut().as_focusable()
        {
            focusable.set_focused(true);
        }
        if let Some(entry) = self
            .overlays
            .iter()
            .find(|entry| Some(&entry.component) == self.focused.as_ref() && self.visible(entry))
        {
            self.restore = Some(FocusRestore {
                overlay: entry.id,
                blocker: None,
                target: None,
            });
        }
    }

    pub fn set_focus(&mut self, mut next: Option<ComponentRef>) {
        let next_overlay = self
            .overlays
            .iter()
            .any(|entry| Some(&entry.component) == next.as_ref());
        if !next_overlay && let Some(mut restore) = self.restore.take() {
            let current = self
                .overlay(restore.overlay)
                .filter(|entry| self.visible(entry));
            if let Some(current) = current {
                let blocker = restore
                    .blocker
                    .as_ref()
                    .filter(|owner| Some(*owner) == self.focused.as_ref());
                if let Some(blocker) = blocker {
                    if next.is_none()
                        || restore.target.is_some()
                        || !self
                            .content
                            .children()
                            .iter()
                            .any(|root| root.contains(blocker))
                    {
                        next = restore
                            .target
                            .take()
                            .unwrap_or_else(|| Some(current.component.clone()));
                    } else {
                        restore.blocker.clone_from(&next);
                        self.restore = Some(restore);
                    }
                } else if let Some(candidate) = next.as_ref() {
                    if Some(&current.component) == self.focused.as_ref()
                        && !self.focus_ancestor(current, candidate)
                    {
                        restore.blocker = Some(candidate.clone());
                    }
                    self.restore = Some(restore);
                }
            }
        }
        self.apply_focus(next);
    }

    fn focus_ancestor(&self, overlay: &Overlay, target: &ComponentRef) -> bool {
        let mut current = overlay.pre_focus.as_ref();
        for _ in 0..=self.overlays.len() {
            let Some(component) = current else {
                return false;
            };
            if component == target {
                return true;
            }
            current = self
                .overlays
                .iter()
                .find(|entry| &entry.component == component)
                .and_then(|entry| entry.pre_focus.as_ref());
        }
        false
    }

    pub fn show_overlay(
        &mut self,
        component: ComponentRef,
        options: OverlayOptions,
    ) -> OverlayHandle {
        self.order += 1;
        let id = OverlayHandle(self.order);
        let entry = Overlay {
            id,
            pre_focus: self.focused.clone(),
            component,
            options,
            hidden: false,
            order: self.order,
            bounds: None,
        };
        let focus =
            (!entry.options.non_capturing && self.visible(&entry)).then(|| entry.component.clone());
        self.overlays.push(entry);
        if let Some(focus) = focus {
            self.set_focus(Some(focus));
        }
        if !self.stopped {
            self.terminal.write("\x1b[?25l");
        }
        self.request_render(false);
        id
    }

    pub fn hide_overlay(&mut self, handle: OverlayHandle) {
        let Some(i) = self.overlays.iter().position(|e| e.id == handle) else {
            return;
        };
        let entry = self.overlays.remove(i);
        for overlay in &mut self.overlays {
            if overlay.pre_focus.as_ref() == Some(&entry.component) {
                overlay.pre_focus.clone_from(&entry.pre_focus);
            }
        }
        if self
            .restore
            .as_ref()
            .is_some_and(|restore| restore.overlay == handle)
        {
            self.restore = None;
        }
        if Some(&entry.component) == self.focused.as_ref() {
            self.set_focus(
                self.top_overlay(None)
                    .map(|e| e.component.clone())
                    .or(entry.pre_focus),
            );
        }
        if self.overlays.is_empty() && !self.stopped {
            self.terminal.write("\x1b[?25l");
        }
        self.request_render(false);
    }

    pub fn hide_top_overlay(&mut self) {
        if let Some(entry) = self.overlays.last() {
            self.hide_overlay(entry.id);
        }
    }

    pub fn set_overlay_hidden(&mut self, handle: OverlayHandle, hidden: bool) {
        let Some(entry) = self.overlays.iter_mut().find(|e| e.id == handle) else {
            return;
        };
        if entry.hidden == hidden {
            return;
        }
        entry.hidden = hidden;
        if hidden {
            if self
                .restore
                .as_ref()
                .is_some_and(|restore| restore.overlay == handle)
            {
                self.restore = None;
            }
            if Some(&entry.component) == self.focused.as_ref() {
                let previous = entry.pre_focus.clone();
                self.set_focus(
                    self.top_overlay(None)
                        .map(|e| e.component.clone())
                        .or(previous),
                );
            }
        } else if !entry.options.non_capturing {
            self.focus_overlay(handle);
        }
        self.request_render(false);
    }

    pub fn focus_overlay(&mut self, handle: OverlayHandle) {
        let width = self.terminal.width();
        let height = self.terminal.height();
        if let Some(entry) = self
            .overlays
            .iter_mut()
            .find(|e| e.id == handle && e.visible(width, height))
        {
            self.order += 1;
            entry.order = self.order;
            let component = entry.component.clone();
            self.set_focus(Some(component));
            self.request_render(false);
        }
    }

    pub fn unfocus_overlay(
        &mut self,
        handle: OverlayHandle,
        options: Option<OverlayUnfocusOptions>,
    ) {
        let Some(entry) = self.overlay(handle) else {
            return;
        };
        let focused = Some(&entry.component) == self.focused.as_ref();
        let previous = entry.pre_focus.clone();
        let has_restore = self
            .restore
            .as_ref()
            .is_some_and(|restore| restore.overlay == handle);
        if !focused && !has_restore {
            return;
        }
        if let Some(restore) = &mut self.restore
            && restore.overlay == handle
            && restore.blocker.is_some()
            && restore.blocker == self.focused
        {
            if let Some(options) = options {
                restore.target = Some(options.target);
            } else {
                self.restore = None;
            }
        } else {
            self.restore = None;
            if focused || options.is_some() {
                let fallback = self
                    .top_overlay(Some(handle))
                    .map(|e| e.component.clone())
                    .or(previous);
                self.set_focus(options.map_or(fallback, |options| options.target));
            }
        }
        self.request_render(false);
    }

    pub fn overlay_hidden(&self, id: OverlayHandle) -> bool {
        self.overlay(id).is_none_or(|e| e.hidden)
    }
    pub fn overlay_focused(&self, id: OverlayHandle) -> bool {
        self.overlay(id)
            .is_some_and(|e| Some(&e.component) == self.focused.as_ref())
    }
    pub fn overlay_bounds(&self, id: OverlayHandle) -> Option<OverlayBounds> {
        let entry = self.overlay(id)?;
        self.visible(entry).then_some(entry.bounds).flatten()
    }

    pub fn dispatch_mouse_to_overlay(
        &mut self,
        event: &TuiMouseEvent,
    ) -> (bool, Option<TuiMouseEventResult>) {
        let Some((target, bounds)) = self.overlay_mouse_target(event.screen_x, event.screen_y)
        else {
            return (false, None);
        };
        let dispatch = target.borrow_mut().dispatch_mouse(
            &target,
            &TuiMouseEvent {
                x: event.screen_x - bounds.col,
                y: event.screen_y - bounds.row,
                width: bounds.width,
                height: bounds.height,
                ..*event
            },
        );
        if dispatch.as_ref().is_some_and(|d| d.result.focus) {
            self.set_focus(Some(target));
        }
        (true, dispatch.map(|d| d.result))
    }

    pub(crate) fn mouse_focus_target(&self, target: &ComponentRef) -> ComponentRef {
        self.overlays
            .iter()
            .rev()
            .filter(|entry| self.visible(entry))
            .find(|entry| entry.component.contains(target))
            .map_or_else(|| target.clone(), |entry| entry.component.clone())
    }

    pub(crate) fn overlay_mouse_target(
        &self,
        x: usize,
        y: usize,
    ) -> Option<(ComponentRef, OverlayBounds)> {
        self.overlays
            .iter()
            .filter(|entry| self.visible(entry))
            .filter_map(|entry| {
                let bounds = entry.bounds?;
                ((bounds.col..bounds.col + bounds.width).contains(&x)
                    && (bounds.row..bounds.row + bounds.height).contains(&y))
                .then_some((entry, bounds))
            })
            .max_by_key(|(entry, _)| entry.order)
            .map(|(entry, bounds)| (entry.component.clone(), bounds))
    }

    pub fn composite_overlays(
        &mut self,
        mut output: Vec<String>,
        width: usize,
        height: usize,
    ) -> Vec<String> {
        if self.overlays.is_empty() {
            return output;
        }
        for entry in &mut self.overlays {
            entry.bounds = None;
        }
        let mut visible: Vec<_> = self
            .overlays
            .iter_mut()
            .filter(|entry| entry.visible(width, height))
            .collect();
        visible.sort_by_key(|entry| entry.order);
        let mut rendered = Vec::with_capacity(visible.len());
        let mut needed = output.len().max(height);
        for entry in visible {
            let (initial, max_height) = entry.options.resolve(0, width, height);
            let mut lines = entry.component.borrow().render(initial.width);
            if let Some(max_height) = max_height {
                lines.truncate(max_height);
            }
            for line in &mut lines {
                resolve_fake_cursors(line, self.show_hardware_cursor);
            }
            let (bounds, _) = entry.options.resolve(lines.len(), width, height);
            needed = needed.max(bounds.row + bounds.height);
            entry.bounds = Some(bounds);
            rendered.push((bounds, lines));
        }
        output.resize_with(needed, String::new);
        let top = needed.saturating_sub(height);
        for (bounds, lines) in rendered {
            hide_images_under_overlay(&mut output, bounds, top);
            for (i, line) in lines.iter().enumerate() {
                if let Some(base) = output.get_mut(top + bounds.row + i) {
                    *base = composite_tui_line(base, line, bounds.col, bounds.width, width);
                }
            }
        }
        output
    }

    pub fn invalidate(&mut self) {
        self.content.invalidate();
        for overlay in &self.overlays {
            overlay.component.borrow_mut().invalidate();
        }
        self.content_changed = true;
    }

    pub fn add_input_listener(&mut self, listener: TuiInputListener) -> u64 {
        self.order += 1;
        self.input_listeners.insert(self.order, listener);
        self.order
    }
    pub fn remove_input_listener(&mut self, id: u64) {
        self.input_listeners.shift_remove(&id);
    }
    pub fn on_terminal_color_scheme_change(&mut self, listener: ColorSchemeListener) -> u64 {
        self.order += 1;
        self.color_listeners.insert(self.order, listener);
        self.order
    }
    pub fn remove_color_scheme_listener(&mut self, id: u64) {
        self.color_listeners.shift_remove(&id);
    }
    pub fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
        if self.color_notifications != enabled {
            self.color_notifications = enabled;
            self.update_exit_frame();
            if !self.stopped {
                self.terminal.write(if enabled {
                    "\x1b[?2031h"
                } else {
                    "\x1b[?2031l"
                });
            }
        }
    }

    pub fn query_terminal_colors(
        &mut self,
        timeout: Duration,
        complete: ColorQueryListener,
        late: Option<ColorQueryListener>,
    ) {
        let now = Instant::now();
        self.color_queries.push_back(ColorQuery {
            colors: TerminalColors::default(),
            palette: [None; 16],
            replies: 0,
            deadline: now.checked_add(timeout).unwrap_or(now),
            on_time: Some(complete),
            on_late: late,
        });
        let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
        for i in 0..16 {
            let _ = write!(query, "\x1b]4;{i};?\x07");
        }
        query.push_str("\x1b[c");
        self.terminal.write(&query);
    }

    pub fn expire_color_queries(&mut self, now: Instant) {
        for query in &mut self.color_queries {
            if now >= query.deadline
                && let Some(complete) = query.on_time.take()
            {
                complete(query.report());
            }
        }
    }

    pub fn filter_input<'a>(&mut self, data: &'a str) -> Option<std::borrow::Cow<'a, str>> {
        self.expire_color_queries(Instant::now());
        if let Some(query) = self.color_queries.front_mut() {
            if parse_keyboard_protocol_negotiation_sequence(data)
                == Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
            {
                if let Some(mut query) = self.color_queries.pop_front() {
                    query.deliver();
                }
                return None;
            }
            if let Some(reply) = parse_osc_color_response(data) {
                let (bit, field) = match reply.target {
                    OscColorTarget::Foreground => (16, &mut query.colors.foreground),
                    OscColorTarget::Background => (17, &mut query.colors.background),
                    OscColorTarget::Index(i @ 0..=15) => {
                        (u32::from(i), &mut query.palette[usize::from(i)])
                    }
                    OscColorTarget::Index(_) => return None,
                };
                if query.replies & (1 << bit) == 0 {
                    query.replies |= 1 << bit;
                    *field = reply.rgb;
                    if query.replies.count_ones() == 18 {
                        query.deliver();
                    }
                }
                return None;
            }
        }
        if let Some(scheme) = parse_terminal_color_scheme_report(data) {
            for listener in self.color_listeners.values_mut() {
                listener(scheme);
            }
            return None;
        }
        let mut current = std::borrow::Cow::Borrowed(data);
        for listener in self.input_listeners.values_mut() {
            let result = listener(&current);
            if result.consume {
                return None;
            }
            if let Some(next) = result.data {
                current = next.into();
            }
        }
        if !self.input_listeners.is_empty() && current.is_empty() {
            return None;
        }
        if let Some(size) = current
            .strip_prefix("\x1b[6;")
            .and_then(|s| s.strip_suffix('t'))
            && let Some((height, width)) = size.split_once(';')
            && let (Ok(height_px), Ok(width_px)) = (height.parse(), width.parse())
        {
            if height_px > 0 && width_px > 0 {
                set_cell_dimensions(CellDimensions {
                    width_px,
                    height_px,
                });
                self.invalidate();
                self.request_render(false);
            }
            return None;
        }
        if let Some(debug) = &mut self.on_debug
            && matches_key_data(&current, "shift+ctrl+d")
        {
            debug();
            return None;
        }
        Some(current)
    }

    pub fn deliver_input(&mut self, data: &str) {
        let old = self
            .overlays
            .iter()
            .find(|entry| Some(&entry.component) == self.focused.as_ref() && !self.visible(entry));
        if let Some(old) = old {
            self.apply_focus(
                self.top_overlay(None)
                    .map(|entry| entry.component.clone())
                    .or_else(|| old.pre_focus.clone()),
            );
        }
        if !self.is_overlay_focused()
            && let Some(restore) = &self.restore
            && (restore.blocker.is_none() || restore.blocker != self.focused)
            && let Some(entry) = self
                .overlay(restore.overlay)
                .filter(|entry| self.visible(entry))
        {
            let next = restore
                .target
                .clone()
                .unwrap_or_else(|| Some(entry.component.clone()));
            self.apply_focus(next);
        }
        if let Some(component) = self.focused.as_ref() {
            let mut component = component.borrow_mut();
            if is_key_release_data(data) && !component.wants_key_release() {
                return;
            }
            component.handle_input(data);
            drop(component);
            self.request_render(false);
            self.last_render = None;
        }
    }

    pub fn handle_input(&mut self, data: &str) {
        if let Some(data) = self.filter_input(data) {
            self.deliver_input(&data);
        }
    }

    pub(crate) fn set_exit_frame(&mut self, before: &'static str, after: &'static str) {
        self.exit_frame = (before, after);
        self.update_exit_frame();
    }

    fn update_exit_frame(&mut self) {
        let (before, after) = self.exit_frame;
        let color = if self.color_notifications {
            "\x1b[?2031l"
        } else {
            ""
        };
        self.terminal
            .set_exit_restore(&format!("{before}{color}\x1b[?25h"), after);
    }

    /// Intercept job-control suspend requests only while this TUI owns the terminal.
    #[cfg(unix)]
    pub fn set_suspend_signal_handler(
        &mut self,
        handler: Option<Box<dyn FnMut()>>,
    ) -> io::Result<()> {
        if handler.is_some() && self.owns_terminal && self.suspend.is_none() {
            self.suspend = Some(crate::terminal::SuspendSignal::register()?);
        } else if handler.is_none() {
            self.suspend.take();
        }
        self.on_suspend = handler;
        Ok(())
    }

    pub fn start(&mut self) -> io::Result<()> {
        self.update_exit_frame();
        self.terminal.start_session()?;
        #[cfg(unix)]
        {
            if self.on_suspend.is_some() && self.suspend.is_none() && self.terminal.is_tty() {
                match crate::terminal::SuspendSignal::register() {
                    Ok(signal) => self.suspend = Some(signal),
                    Err(error) => {
                        let _ = self.terminal.stop_session();
                        return Err(error);
                    }
                }
            }
            self.owns_terminal = self.terminal.is_tty();
        }
        self.stopped = false;
        self.terminal.write("\x1b[?25l");
        if self.color_notifications {
            self.terminal.write("\x1b[?2031h");
        }
        if get_capabilities().images.is_some() {
            self.terminal.write("\x1b[16t");
        }
        self.request_render(false);
        Ok(())
    }

    pub fn begin_stop(&mut self) {
        #[cfg(unix)]
        {
            self.suspend.take();
            self.owns_terminal = false;
        }
        self.stopped = true;
        self.render_requested = false;
        if self.color_notifications {
            self.terminal.write("\x1b[?2031l");
        }
    }
    pub fn finish_stop(&mut self) -> io::Result<()> {
        self.terminal.write("\x1b[?25h");
        self.terminal.stop_session()
    }
}

pub fn composite_tui_line(
    base: &str,
    overlay: &str,
    col: usize,
    width: usize,
    total: usize,
) -> String {
    if is_image_line(base) {
        return base.to_owned();
    }
    let end = col.saturating_add(width);
    let segments = extract_segments(base, col, end, total.saturating_sub(end), true);
    let overlay = slice_with_width(overlay, 0, width, true);
    let mut result = segments.before;
    result.extend(std::iter::repeat_n(
        ' ',
        col.saturating_sub(segments.before_width),
    ));
    result.push_str(SEGMENT_RESET);
    result.push_str(&overlay.text);
    result.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(overlay.width),
    ));
    result.push_str(SEGMENT_RESET);
    result.push_str(&segments.after);
    result.extend(std::iter::repeat_n(
        ' ',
        total
            .saturating_sub(end)
            .saturating_sub(segments.after_width),
    ));
    if visible_width(&result) <= total {
        result
    } else {
        slice_by_column(&result, 0, total, true)
    }
}

fn hide_images_under_overlay(lines: &mut [String], bounds: OverlayBounds, top: usize) {
    if bounds.height == 0 {
        return;
    }
    let start = top.saturating_sub(get_max_kitty_image_rows());
    for (i, line) in lines.iter_mut().enumerate().skip(start) {
        let Some(extent) = get_image_line_extent(line) else {
            continue;
        };
        let row = i as isize - top as isize;
        if row + extent.first_row < (bounds.row + bounds.height) as isize
            && row + extent.last_row >= bounds.row as isize
            && extent.start_col < bounds.col + bounds.width
            && extent.end_col.is_none_or(|end| end > bounds.col)
        {
            *line = strip_image_sequences(line).into_owned();
        }
    }
}

pub(crate) fn strip_prompt_markers(mut line: &str) -> &str {
    while let Some(rest) = line.strip_prefix("\x1b]133;") {
        let Some(rest) = rest.strip_prefix(['A', 'B', 'C']) else {
            break;
        };
        let Some(rest) = rest
            .strip_prefix('\x07')
            .or_else(|| rest.strip_prefix("\x1b\\"))
        else {
            break;
        };
        line = rest;
    }
    line
}

pub(crate) fn resolve_fake_cursors(line: &mut String, hardware: bool) {
    if !line.contains(FAKE_CURSOR_START) {
        return;
    }
    if hardware {
        static CURSOR: LazyLock<Regex> = LazyLock::new(|| {
            // INFALLIBLE: The cursor expression matches fixed APC markers.
            Regex::new(r"(?s)(\x1b_pi:c\x07)\x1b_pi:fc\x07(.*?)(?:\x1b_pi:/fc\x07|$)")
                .expect("valid APC regex")
        });
        if let std::borrow::Cow::Owned(resolved) = CURSOR.replace(line, "$1$2") {
            *line = resolved;
        }
    }
    *line = line
        .replace(FAKE_CURSOR_START, "\x1b[7m")
        .replace(FAKE_CURSOR_END, "\x1b[27m");
}

pub(crate) fn extract_cursor_position(
    lines: &mut [String],
    height: usize,
) -> Option<(usize, usize)> {
    let top = lines.len().saturating_sub(height);
    for (i, line) in lines.iter_mut().enumerate().skip(top).rev() {
        if let Some(index) = line.find(CURSOR_MARKER) {
            let width = visible_width(&line[..index]);
            *line = line.replace(CURSOR_MARKER, "");
            return Some((i, width));
        }
    }
    None
}

pub(crate) fn reset_line(line: &mut String) {
    if is_image_line(line) {
        return;
    }
    if line.contains(CURSOR_MARKER) {
        *line = line.replace(CURSOR_MARKER, "");
    }
    if let std::borrow::Cow::Owned(clean) = normalize_terminal_output(line) {
        *line = clean;
    }
    line.push_str(SEGMENT_RESET);
}
