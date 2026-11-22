use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::{TuiAltScreen, highlight};
use crate::layout::{
    LayoutFrame, LayoutRect, ScrollbarGeometry, get_layout_boxes_at, get_scroll_view_box,
    get_scroll_views_at, get_scrollbar_geometry,
};
use crate::layout_node::{Overscroll, ScrollLayoutState};
use crate::terminal_image::is_image_line;
use crate::tui::{
    ComponentRef, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::utils::{
    extract_ansi_code, get_grapheme_cell_range, get_osc8_link_at_column, slice_by_column,
    strip_terminal_sequences, visible_width,
};
use crate::word_navigation::segment_words;

const MULTI_CLICK: Duration = Duration::from_millis(500);
const SCROLL_TICK: Duration = Duration::from_millis(50);

type View = Rc<dyn ScrollLayoutState>;

#[derive(Clone)]
struct Target {
    component: ComponentRef,
    rect: LayoutRect,
}

impl Target {
    fn from_dispatch(dispatch: crate::tui::TuiMouseDispatch) -> (Self, TuiMouseEventResult) {
        let event = dispatch.event;
        (
            Self {
                component: dispatch.target,
                rect: LayoutRect {
                    x: event.screen_x.saturating_sub(event.x),
                    y: event.screen_y as isize - event.y as isize,
                    width: event.width,
                    height: event.height,
                },
            },
            dispatch.result,
        )
    }
}

struct Gesture {
    target: Target,
    x: usize,
    y: usize,
    button: TuiMouseButton,
    moved: bool,
}

struct ComponentClick {
    target: ComponentRef,
    x: usize,
    y: usize,
    time: Instant,
    count: u32,
}

#[derive(Clone)]
struct Point {
    view: Option<View>,
    row: isize,
    column: usize,
    boundary: bool,
}

impl Point {
    fn position(&self) -> (isize, usize) {
        (self.row, self.column)
    }
    fn same_view(&self, other: &Self) -> bool {
        match (&self.view, &other.view) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

type Range = (Point, Point);

#[derive(Clone, Copy, Default)]
enum Granularity {
    #[default]
    Cell,
    Word,
    Line,
}

struct TextClick {
    word: Range,
    time: Instant,
    count: u32,
}
struct AutoScroll {
    x: usize,
    y: usize,
    direction: isize,
    next: Instant,
}

#[derive(Default)]
pub(super) struct MouseState {
    gesture: Option<Gesture>,
    click: Option<ComponentClick>,
    hover: Option<View>,
    scrollbar: Option<(View, isize)>,
    selection: Option<Range>,
    initial_selection: Option<Range>,
    granularity: Granularity,
    text_click: Option<TextClick>,
    selecting: bool,
    dragged: bool,
    url: Option<String>,
    auto_scroll: Option<AutoScroll>,
}

impl MouseState {
    fn clear_selection(&mut self) {
        self.selection = None;
        self.initial_selection = None;
        self.granularity = Granularity::Cell;
        self.selecting = false;
        self.dragged = false;
        self.url = None;
        self.auto_scroll = None;
    }
    fn bounds(&self) -> Option<(&Point, &Point)> {
        let (a, b) = self.selection.as_ref()?;
        if !a.same_view(b) || a.position() == b.position() {
            return None;
        }
        if a.position() < b.position() {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }
}

#[derive(Clone, Copy)]
struct Raw {
    button: u32,
    x: usize,
    y: usize,
    release: bool,
}

impl Raw {
    fn parse(data: &str) -> Option<Self> {
        if let Some(body) = data.strip_prefix("\x1b[<") {
            let release = body.ends_with('m');
            let body = body.strip_suffix(['M', 'm'])?;
            let (button, rest) = body.split_once(';')?;
            let (x, y) = rest.split_once(';')?;
            return Some(Self {
                button: button.parse().ok()?,
                x: x.parse::<usize>().ok()?.saturating_sub(1),
                y: y.parse::<usize>().ok()?.saturating_sub(1),
                release,
            });
        }
        let mut chars = data.strip_prefix("\x1b[M")?.chars();
        let button = (chars.next()? as u32).checked_sub(32)?;
        let x = (chars.next()? as usize).checked_sub(33)?;
        let y = (chars.next()? as usize).checked_sub(33)?;
        if chars.next().is_some() || button & 64 == 0 {
            return None;
        }
        Some(Self {
            button,
            x,
            y,
            release: false,
        })
    }
    fn motion(self) -> bool {
        self.button & 32 != 0
    }
    fn press(self) -> bool {
        !self.release && !self.motion()
    }
    fn left(self) -> bool {
        self.button & 3 == 0
    }
    fn wheel(self) -> Option<i8> {
        if self.button & 64 == 0 {
            return None;
        }
        match self.button & 3 {
            0 => Some(-1),
            1 => Some(1),
            _ => None,
        }
    }
    fn event(self, width: usize, height: usize, delta: Option<i32>) -> TuiMouseEvent {
        let button = if delta.is_some() {
            TuiMouseButton::None
        } else {
            match self.button & 3 {
                0 => TuiMouseButton::Left,
                1 => TuiMouseButton::Middle,
                2 => TuiMouseButton::Right,
                _ => TuiMouseButton::None,
            }
        };
        let event_type = if delta.is_some() {
            TuiMouseEventType::Wheel
        } else if self.release {
            TuiMouseEventType::Release
        } else if self.motion() && button == TuiMouseButton::None {
            TuiMouseEventType::Move
        } else if self.motion() {
            TuiMouseEventType::Drag
        } else {
            TuiMouseEventType::Press
        };
        TuiMouseEvent {
            event_type,
            button,
            x: self.x,
            y: self.y,
            screen_x: self.x,
            screen_y: self.y,
            width,
            height,
            shift: self.button & 4 != 0,
            alt: self.button & 8 != 0,
            ctrl: self.button & 16 != 0,
            wheel_delta: delta,
            click_count: None,
        }
    }
}

impl TuiAltScreen {
    pub(super) fn clear_mouse_gestures(&mut self) {
        if let Some(view) = self.mouse.hover.take() {
            view.set_scrollbar_active(false);
        }
        self.mouse = MouseState::default();
    }

    pub fn reset_text_selection(&mut self) {
        self.mouse.clear_selection();
        self.mouse.text_click = None;
        self.base.request_view_render();
    }

    pub fn has_active_selection(&self) -> bool {
        self.selected_text().is_some()
    }

    pub fn copy_active_selection_to_clipboard(&mut self) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        let result = if let Some(copy) = &mut self.options.copy_selection {
            copy(&text)
        } else {
            let encoded = STANDARD.encode(text);
            self.base
                .terminal
                .write(&format!("\x1b]52;c;{encoded}\x07"));
            Ok(())
        };
        match result {
            Ok(()) => {
                self.flash("Copied!".into(), None);
                true
            }
            Err(error) => {
                self.flash(
                    error.unwrap_or_else(|| "Copy failed".into()),
                    Some(Duration::from_secs(5)),
                );
                false
            }
        }
    }

    pub(super) fn handle_mouse_input(&mut self, input: &str) -> bool {
        if input == "\x1b[I" {
            return true;
        }
        if input == "\x1b[O" {
            if self.mouse.selecting {
                self.reset_text_selection();
            }
            self.mouse.auto_scroll = None;
            self.mouse.scrollbar = None;
            self.mouse.gesture = None;
            self.mouse.click = None;
            self.mouse.text_click = None;
            self.set_hover(None);
            if let Some(search) = &self.search
                && search
                    .component
                    .borrow_mut()
                    .set_hovered_navigation_direction(None)
            {
                self.base.request_view_render();
            }
            return true;
        }
        let Some(raw) = Raw::parse(input) else {
            return input.starts_with("\x1b[M") && input.chars().count() == 6;
        };
        if !self.options.mouse {
            return true;
        }
        let (width, height) = (
            self.base.terminal.width().max(1),
            self.base.terminal.height().max(1),
        );
        if let Some(dir) = raw.wheel() {
            let speed = self
                .wheel
                .next(dir, self.clock.elapsed().as_secs_f64() * 1000.);
            let multiplier = if raw.button & 8 != 0 { 5 } else { 1 };
            let delta =
                speed.saturating_mul(multiplier).min(i32::MAX as usize) as i32 * i32::from(dir);
            let event = raw.event(width, height, Some(delta));
            if let Some((target, result)) = self.dispatch(&event) {
                self.apply_result(&target, &event, result);
            } else {
                self.route_wheel(raw, delta as isize);
            }
        } else if raw.button & 64 == 0 {
            self.route_mouse(raw, raw.event(width, height, None));
        }
        true
    }

    fn route_wheel(&mut self, raw: Raw, mut delta: isize) {
        let primary = self.primary();
        let mut saw_primary = false;
        if let Some(layout) = &self.layout {
            for view in get_scroll_views_at(layout, raw.x, raw.y) {
                saw_primary |= Rc::ptr_eq(primary, view);
                delta = view.scroll_by(delta);
                if delta == 0 || view.overscroll() == Overscroll::Contain {
                    break;
                }
            }
        }
        if delta != 0 && !saw_primary {
            primary.scroll_by(delta);
        }
        self.update_scrollbar_hover(raw);
        self.base.request_view_render();
    }

    fn route_mouse(&mut self, raw: Raw, event: TuiMouseEvent) {
        if let Some(mut gesture) = self.mouse.gesture.take() {
            if raw.x != gesture.x || raw.y != gesture.y {
                gesture.moved = true;
                self.mouse.click = None;
            }
            if let Some((target, result)) = self.send(&gesture.target, &event) {
                self.apply_result(&target, &event, result);
            }
            if raw.release {
                if !gesture.moved {
                    let count = self.click_count(&gesture.target, raw);
                    let event = TuiMouseEvent {
                        event_type: TuiMouseEventType::Click,
                        button: gesture.button,
                        click_count: Some(count),
                        ..event
                    };
                    if let Some((target, result)) = self.send(&gesture.target, &event) {
                        self.apply_result(&target, &event, result);
                    }
                }
            } else {
                self.mouse.gesture = Some(gesture);
            }
            return;
        }
        if self.search_mouse(raw) {
            return;
        }
        let overlay = self.base.overlay_mouse_target(raw.x, raw.y).is_some();
        if !overlay {
            if raw.press()
                && raw.left()
                && self
                    .end_indicator
                    .is_some_and(|b| raw.y == b.row && (b.col..b.col + b.width).contains(&raw.x))
            {
                self.scroll_to_bottom();
                return;
            }
            let handled = self.scrollbar_mouse(raw);
            if self.mouse.scrollbar.is_none() {
                self.update_scrollbar_hover(raw);
            }
            if handled {
                return;
            }
        } else {
            self.set_hover(None);
        }
        if let Some((target, result)) = self.dispatch(&event) {
            self.apply_result(&target, &event, result);
            if raw.press() {
                self.mouse.clear_selection();
                self.mouse.gesture = Some(Gesture {
                    target,
                    x: raw.x,
                    y: raw.y,
                    button: event.button,
                    moved: false,
                });
            }
            return;
        }
        if cfg!(windows)
            && raw.press()
            && raw.button == 2
            && !std::env::var("TERM_PROGRAM").is_ok_and(|term| term.eq_ignore_ascii_case("vscode"))
            && let Some(paste) = &mut self.options.on_right_click_paste
        {
            paste();
            return;
        }
        self.selection_mouse(raw, event);
    }

    fn search_mouse(&mut self, raw: Raw) -> bool {
        let Some(search) = &self.search else {
            return false;
        };
        let dir = self.base.overlay_bounds(search.overlay).and_then(|bounds| {
            if !(bounds.col..bounds.col + bounds.width).contains(&raw.x)
                || !(bounds.row..bounds.row + bounds.height).contains(&raw.y)
            {
                return None;
            }
            search
                .component
                .borrow()
                .navigation_direction_at(raw.y - bounds.row, raw.x - bounds.col)
        });
        if search
            .component
            .borrow_mut()
            .set_hovered_navigation_direction(dir)
        {
            self.base.request_view_render();
        }
        if let Some(dir) = dir
            && raw.press()
            && raw.left()
        {
            self.navigate_search(dir);
            true
        } else {
            false
        }
    }

    fn send(
        &mut self,
        target: &Target,
        event: &TuiMouseEvent,
    ) -> Option<(Target, TuiMouseEventResult)> {
        let local = TuiMouseEvent {
            x: event.screen_x.saturating_sub(target.rect.x),
            y: (event.screen_y as isize)
                .saturating_sub(target.rect.y)
                .max(0) as usize,
            width: target.rect.width,
            height: target.rect.height,
            ..*event
        };
        let dispatch = if target.component == self.implicit_doc {
            self.base.content.dispatch_to_child(&local)
        } else {
            target
                .component
                .borrow_mut()
                .dispatch_mouse(&target.component, &local)
        };
        dispatch.map(Target::from_dispatch)
    }

    fn dispatch(&mut self, event: &TuiMouseEvent) -> Option<(Target, TuiMouseEventResult)> {
        if let Some((component, rect)) = self
            .base
            .overlay_mouse_target(event.screen_x, event.screen_y)
        {
            let target = Target {
                component,
                rect: LayoutRect {
                    x: rect.col,
                    y: rect.row as isize,
                    width: rect.width,
                    height: rect.height,
                },
            };
            return self.send(&target, event);
        }
        let targets = if let Some(layout) = &self.layout {
            let mut visited = HashSet::new();
            get_layout_boxes_at(layout, event.screen_x, event.screen_y)
                .into_iter()
                .filter(|node| {
                    node.scroll_view.is_none() && visited.insert(node.component.identity())
                })
                .map(|node| Target {
                    component: node.component.clone(),
                    rect: node.rect,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        targets
            .into_iter()
            .find_map(|target| self.send(&target, event))
    }

    fn apply_result(
        &mut self,
        target: &Target,
        event: &TuiMouseEvent,
        result: TuiMouseEventResult,
    ) {
        let mut change = false;
        if result.focus {
            let target = self.base.mouse_focus_target(&target.component);
            if Some(&target) != self.base.focused() {
                self.base.set_focus(Some(target));
                change = true;
            }
        }
        if result.render.unwrap_or(
            change
                || !matches!(
                    event.event_type,
                    TuiMouseEventType::Move | TuiMouseEventType::Release
                ),
        ) {
            self.base.request_render(false);
        }
    }

    fn click_count(&mut self, target: &Target, raw: Raw) -> u32 {
        let now = Instant::now();
        let count = self
            .mouse
            .click
            .as_ref()
            .filter(|click| {
                now.duration_since(click.time) <= MULTI_CLICK
                    && click.target == target.component
                    && click.x == raw.x
                    && click.y == raw.y
            })
            .map_or(1, |click| (click.count % 3) + 1);
        self.mouse.click = Some(ComponentClick {
            target: target.component.clone(),
            x: raw.x,
            y: raw.y,
            time: now,
            count,
        });
        count
    }

    fn scrollbar_at(&self, raw: Raw, include_hidden: bool) -> Option<(View, ScrollbarGeometry)> {
        let layout = self.layout.as_ref()?;
        if self.base.has_overlay() {
            return None;
        }
        get_layout_boxes_at(layout, raw.x, raw.y)
            .into_iter()
            .find_map(|node| {
                let geometry = get_scrollbar_geometry(node, include_hidden)?;
                (raw.x == geometry.column)
                    .then(|| (node.scroll_view.as_ref().cloned(), geometry))
                    .and_then(|(view, g)| Some((view?, g)))
            })
    }

    fn set_hover(&mut self, next: Option<View>) {
        if matches!((&self.mouse.hover, &next), (Some(a), Some(b)) if Rc::ptr_eq(a, b)) {
            return;
        }
        if let Some(previous) = &self.mouse.hover {
            previous.set_scrollbar_active(false);
        }
        if let Some(next) = &next {
            next.set_scrollbar_active(true);
        }
        self.mouse.hover = next;
    }

    fn update_scrollbar_hover(&mut self, raw: Raw) {
        self.set_hover(self.scrollbar_at(raw, true).map(|(view, _)| view));
    }

    fn scrollbar_mouse(&mut self, raw: Raw) -> bool {
        let move_thumb = |view: &View, geometry: ScrollbarGeometry, grab: isize| {
            let space = geometry.track_height - geometry.thumb_height;
            let thumb = (raw.y as isize - geometry.track_top - grab)
                .max(0)
                .min(space as isize);
            let top = if space == 0 {
                0
            } else {
                (thumb as f64 / space as f64 * geometry.max_scroll_top as f64).round() as usize
            };
            view.scroll_to(top, false);
        };
        if let Some((view, grab)) = &self.mouse.scrollbar {
            if raw.release {
                self.mouse.scrollbar = None;
            } else if let Some(geometry) = self
                .layout
                .as_ref()
                .and_then(|f| get_scroll_view_box(f, view))
                .and_then(|node| get_scrollbar_geometry(node, false))
            {
                move_thumb(view, geometry, *grab);
            }
            return true;
        }
        if !raw.press() || !raw.left() {
            return false;
        }
        let Some((view, geometry)) = self.scrollbar_at(raw, false) else {
            return false;
        };
        self.reset_text_selection();
        self.set_hover(Some(view.clone()));
        let offset = raw.y as isize - geometry.thumb_top;
        let grab = if (0..geometry.thumb_height as isize).contains(&offset) {
            offset
        } else {
            let grab = (geometry.thumb_height / 2) as isize;
            move_thumb(&view, geometry, grab);
            grab
        };
        self.mouse.scrollbar = Some((view, grab));
        true
    }

    fn point(&self, x: usize, y: usize, view: Option<&View>) -> Point {
        if let Some(view) = view
            && let Some(node) = self
                .layout
                .as_ref()
                .and_then(|layout| get_scroll_view_box(layout, view))
            && node.clip.height > 0
        {
            let top = node.clip.y.max(0);
            let bottom = node.clip.bottom().max(top + 1) - 1;
            let content_len = node
                .scroll_content_lines
                .as_ref()
                .map_or(0, |lines| lines.len());
            return Point {
                row: (view.scroll_top() as isize + (y as isize).clamp(top, bottom) - node.rect.y)
                    .clamp(0, content_len.saturating_sub(1) as isize),
                column: x
                    .saturating_sub(node.rect.x)
                    .min(node.rect.width.saturating_sub(1)),
                view: Some(view.clone()),
                boundary: false,
            };
        }
        Point {
            row: y.min(self.base.terminal.height().saturating_sub(1)) as isize,
            column: x.min(self.base.terminal.width().saturating_sub(1)),
            view: None,
            boundary: false,
        }
    }

    fn source(&self, view: Option<&View>) -> &[String] {
        if let Some(view) = view {
            self.layout
                .as_ref()
                .and_then(|layout| get_scroll_view_box(layout, view))
                .and_then(|node| node.scroll_content_lines.as_deref())
                .unwrap_or(&[])
        } else {
            &self.previous
        }
    }

    fn source_line(&self, point: &Point) -> &str {
        self.source(point.view.as_ref())
            .get(point.row as usize)
            .map_or("", String::as_str)
    }

    fn word_range(&self, point: &Point) -> Option<Range> {
        let line = strip_terminal_sequences(self.source_line(point));
        let mut col = 0;
        let words: Vec<_> = segment_words(&line)
            .map(|word| {
                let text = word.text;
                let from = col;
                col += visible_width(text);
                let joiner = matches!(text, "/" | "-" | "+" | "\\" | "~" | "_" | ".");
                (from, col, word.word_like || joiner, joiner)
            })
            .collect();
        let clicked = words
            .iter()
            .position(|&(a, b, _, _)| (a..b).contains(&point.column))?;
        let joined = |a: usize, b: usize| words[a].2 && words[b].2 && (words[a].3 || words[b].3);
        let mut start = clicked;
        let mut end = clicked;
        while start != 0 && joined(start - 1, start) {
            start -= 1;
        }
        while end + 1 < words.len() && joined(end, end + 1) {
            end += 1;
        }
        Some((
            Point {
                column: words[start].0,
                ..point.clone()
            },
            Point {
                column: words[end].1,
                boundary: true,
                ..point.clone()
            },
        ))
    }

    fn line_range(&self, point: &Point) -> Range {
        (
            Point {
                column: 0,
                ..point.clone()
            },
            Point {
                column: visible_width(self.source_line(point)),
                boundary: true,
                ..point.clone()
            },
        )
    }

    fn select_to(&mut self, point: Point) {
        if let Some(initial) = &self.mouse.initial_selection {
            let range = match self.mouse.granularity {
                Granularity::Cell => None,
                Granularity::Word => self.word_range(&point),
                Granularity::Line => Some(self.line_range(&point)),
            };
            if let Some((start, end)) = range {
                self.mouse.selection = Some(if start.position() < initial.0.position() {
                    (initial.1.clone(), start)
                } else {
                    (initial.0.clone(), end)
                });
            }
        } else if let Some((_, focus)) = &mut self.mouse.selection {
            *focus = point;
        }
    }

    fn selection_mouse(&mut self, raw: Raw, event: TuiMouseEvent) {
        if !raw.left() && !(raw.release && raw.button & 3 == 3) {
            return;
        }
        if raw.release {
            if !self.mouse.selecting {
                return;
            }
            self.mouse.selecting = false;
            self.mouse.auto_scroll = None;
            let Some((anchor, _)) = &self.mouse.selection else {
                return;
            };
            let point = self.point(raw.x, raw.y, anchor.view.as_ref());
            let click = !self.mouse.dragged
                && point.same_view(anchor)
                && point.position() == anchor.position();
            self.select_to(point);
            let url = self.mouse.url.take();
            if click {
                if let Some(url) = url
                    && let Some(open) = &mut self.options.open_url
                {
                    open(&url);
                    self.mouse.clear_selection();
                    self.base.request_view_render();
                    return;
                }
                let event = TuiMouseEvent {
                    event_type: TuiMouseEventType::Click,
                    button: TuiMouseButton::Left,
                    click_count: Some(
                        self.mouse
                            .text_click
                            .as_ref()
                            .map_or(1, |click| click.count),
                    ),
                    ..event
                };
                if let Some((target, result)) = self.dispatch(&event) {
                    self.apply_result(&target, &event, result);
                    self.mouse.clear_selection();
                    return;
                }
            }
            if self.options.copy_on_select {
                self.copy_active_selection_to_clipboard();
            }
            self.base.request_view_render();
            return;
        }
        if raw.motion() {
            let Some((anchor, _)) = self
                .mouse
                .selection
                .as_ref()
                .filter(|_| self.mouse.selecting)
            else {
                return;
            };
            let view = anchor.view.clone();
            let point = self.point(raw.x, raw.y, view.as_ref());
            self.mouse.dragged = true;
            self.mouse.url = None;
            self.mouse.text_click = None;
            self.select_to(point);
            self.mouse.auto_scroll = view.as_ref().and_then(|view| {
                let node = get_scroll_view_box(self.layout.as_ref()?, view)?;
                if node.clip.height == 0 {
                    return None;
                }
                let direction = if raw.y as isize <= node.clip.y {
                    -1
                } else if raw.y as isize >= node.clip.bottom() - 1 {
                    1
                } else {
                    return None;
                };
                Some(AutoScroll {
                    x: raw.x,
                    y: raw.y,
                    direction,
                    next: self
                        .mouse
                        .auto_scroll
                        .as_ref()
                        .map_or_else(|| Instant::now() + SCROLL_TICK, |s| s.next),
                })
            });
            self.base.request_view_render();
            return;
        }
        let view = if self.base.has_overlay() {
            None
        } else {
            self.layout
                .as_ref()
                .and_then(|f| get_scroll_views_at(f, raw.x, raw.y).into_iter().next())
        };
        let point = self.point(raw.x, raw.y, view);
        let word = self.word_range(&point);
        let now = Instant::now();
        let count = self
            .mouse
            .text_click
            .as_ref()
            .filter(|click| {
                word.as_ref().is_some_and(|(a, b)| {
                    now.duration_since(click.time) <= MULTI_CLICK
                        && a.same_view(&click.word.0)
                        && a.position() == click.word.0.position()
                        && b.position() == click.word.1.position()
                })
            })
            .map_or(1, |click| click.count % 3 + 1);
        self.mouse.clear_selection();
        self.mouse.text_click = word.clone().map(|word| TextClick {
            word,
            time: now,
            count,
        });
        self.mouse.selecting = true;
        let initial = match count {
            2 => {
                self.mouse.granularity = Granularity::Word;
                word
            }
            3 => {
                self.mouse.granularity = Granularity::Line;
                Some(self.line_range(&point))
            }
            _ => None,
        };
        if initial.is_none() {
            self.mouse.url = self
                .previous
                .get(raw.y)
                .and_then(|line| get_osc8_link_at_column(line, raw.x))
                .map(str::to_owned);
        }
        self.mouse.selection = initial.clone().or_else(|| Some((point.clone(), point)));
        self.mouse.initial_selection = initial;
        self.base.request_view_render();
    }

    pub(super) fn advance_selection_scroll(&mut self, now: Instant) {
        let Some(scroll) = self
            .mouse
            .auto_scroll
            .as_mut()
            .filter(|scroll| now >= scroll.next)
        else {
            return;
        };
        let Some(view) = self
            .mouse
            .selection
            .as_ref()
            .and_then(|(anchor, _)| anchor.view.clone())
        else {
            return;
        };
        if view.scroll_by(scroll.direction) == scroll.direction {
            self.mouse.auto_scroll = None;
            return;
        }
        scroll.next = now + SCROLL_TICK;
        let (x, y) = (scroll.x, scroll.y);
        let point = self.point(x, y, Some(&view));
        self.select_to(point);
        self.base.request_view_render();
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.mouse.bounds()?;
        let lines = self.source(start.view.as_ref());
        let mut selected = String::new();
        for row in start.row..=end.row {
            if row > start.row {
                selected.push('\n');
            }
            let line = lines.get(row as usize).map_or("", String::as_str);
            let (a, b) = selection_cells(line, row, start, end, 0, visible_width(line));
            let text =
                strip_terminal_sequences(&slice_by_column(line, a, b.saturating_sub(a), true));
            selected.push_str(text.trim_end());
        }
        (!selected.is_empty()).then_some(selected)
    }

    pub(super) fn paint_selection(&self, screen: &mut [String], layout: &LayoutFrame) {
        let Some((start, end)) = self.mouse.bounds() else {
            return;
        };
        let mut start = start.clone();
        let mut end = end.clone();
        let mut bounds = LayoutRect {
            x: 0,
            y: 0,
            width: layout.width,
            height: screen.len(),
        };
        if let Some(view) = &start.view {
            let Some(node) = get_scroll_view_box(layout, view) else {
                return;
            };
            let offset = node.rect.y - view.scroll_top() as isize;
            bounds = node.clip;
            start.row += offset;
            end.row += offset;
            start.column += node.rect.x;
            end.column += node.rect.x;
        }
        let first = start.row.max(bounds.y).max(0) as usize;
        let last = end.row.saturating_add(1).min(bounds.bottom()).max(0) as usize;
        for (row, line) in screen.iter_mut().enumerate().take(last).skip(first) {
            if is_image_line(line) {
                continue;
            }
            let (a, b) =
                selection_cells(line, row as isize, &start, &end, bounds.x, bounds.right());
            highlight(line, a, b, |text| {
                let mut painted = String::from("\x1b[7m");
                let mut i = 0;
                while let Some(index) = text[i..].find('\x1b').map(|index| i + index) {
                    painted.push_str(&text[i..index]);
                    if let Some(code) = extract_ansi_code(text, index) {
                        painted.push_str(code);
                        if code.ends_with('m') {
                            painted.push_str("\x1b[7m");
                        }
                        i = index + code.len();
                    } else {
                        painted.push('\x1b');
                        i = index + 1;
                    }
                }
                painted.push_str(&text[i..]);
                painted.push_str("\x1b[27m");
                painted
            });
        }
    }
}

fn selection_cells(
    line: &str,
    row: isize,
    start: &Point,
    end: &Point,
    min_col: usize,
    max_col: usize,
) -> (usize, usize) {
    let width = visible_width(line);
    let a = if row == start.row {
        get_grapheme_cell_range(line, start.column).map_or(start.column.min(width), |g| g.start)
    } else {
        min_col
    };
    let b = if row == end.row {
        if end.boundary {
            end.column.min(width)
        } else {
            get_grapheme_cell_range(line, end.column)
                .map_or(end.column.saturating_add(1).min(width), |g| g.end)
        }
    } else {
        width
    };
    (a.max(min_col), b.min(max_col))
}
