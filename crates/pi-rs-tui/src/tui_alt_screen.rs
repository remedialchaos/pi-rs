//! Differential alternate-screen renderer and viewport control.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use indexmap::{IndexMap, IndexSet};

use crate::alt_screen_search::{
    AltScreenSearchComponent, AltScreenSearchIndex, NavigationButtonStyle,
    get_alt_screen_search_match_key,
};
use crate::components::{
    TextStyleFn,
    alt_screen_flash::AltScreenFlashContainer,
    input::SubmitHandler,
    scroll_view::{ScrollView, ScrollViewOptions},
};
use crate::keybindings::get_keybindings;
use crate::keys::is_key_release_data;
use crate::layout::{
    LayoutFrame, LayoutRenderCache, expire_scrollbars, get_scroll_view_box, get_scrollbar_geometry,
    render_layout_frame,
};
use crate::layout_node::ScrollLayoutState;
use crate::terminal::{Terminal, TerminalEvent};
use crate::terminal_image::{
    ImageProtocol, TerminalCapabilities, delete_all_kitty_images, delete_all_kitty_placements,
    delete_kitty_image, get_capabilities, get_kitty_image_placement,
    get_kitty_image_placement_rows, is_image_line, set_capabilities,
};
use crate::tui::{
    Component, ComponentRef, OverlayAnchor, OverlayBounds, OverlayHandle, OverlayOptions,
    SizeValue, Tui, TuiBase, TuiMode, TuiStopOptions, ViewportTui, composite_tui_line,
    extract_cursor_position, reset_line, resolve_fake_cursors, strip_prompt_markers,
};
use crate::tui_main_screen::{Output, SYNC_OFF, SYNC_ON};
use crate::utils::{extract_ansi_code, slice_by_column, truncate_with_ellipsis, visible_width};
use crate::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

mod mouse;
use mouse::MouseState;

const ENTER: &str = "\x1b[?1049h";
const EXIT: &str = "\x1b[?1049l";
const NO_WRAP: &str = "\x1b[?7l";
const WRAP: &str = "\x1b[?7h";
const MOUSE_MOTION: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
const MOUSE_BUTTONS: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
const NO_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

pub type CopySelection = Box<dyn FnMut(&str) -> Result<(), Option<String>>>;

pub struct TuiAltScreenOptions {
    pub wheel_scroll_lines: WheelScrollLines,
    pub mouse: bool,
    pub search_match_style: Option<TextStyleFn>,
    pub search_current_match_style: Option<TextStyleFn>,
    pub search_navigation_button_style: Option<NavigationButtonStyle>,
    pub scroll_to_end_indicator: Option<Box<dyn Fn() -> String>>,
    pub open_url: Option<SubmitHandler>,
    pub on_right_click_paste: Option<Box<dyn FnMut()>>,
    pub copy_on_select: bool,
    pub copy_selection: Option<CopySelection>,
}

impl Default for TuiAltScreenOptions {
    fn default() -> Self {
        Self {
            wheel_scroll_lines: WheelScrollLines::Fixed(1.),
            mouse: true,
            search_match_style: None,
            search_current_match_style: None,
            search_navigation_button_style: None,
            scroll_to_end_indicator: None,
            open_url: None,
            on_right_click_paste: None,
            copy_on_select: true,
            copy_selection: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Query,
    Retain,
    Next,
    Previous,
}

type MatchKey = (usize, usize, usize, usize);

struct ActiveSearch {
    component: Rc<RefCell<AltScreenSearchComponent>>,
    overlay: OverlayHandle,
    index: AltScreenSearchIndex,
    pending: Rc<RefCell<Option<String>>>,
    query: String,
    selected: Option<usize>,
    key: Option<MatchKey>,
    anchor_row: usize,
    mode: SearchMode,
}

struct Document(Rc<RefCell<Rc<[String]>>>);

impl Component for Document {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.borrow().to_vec()
    }
}

#[derive(Clone, Copy)]
struct UploadedImage {
    generation: u64,
    transmission: usize,
    decoded: usize,
}

pub struct TuiAltScreen {
    pub base: TuiBase,
    options: TuiAltScreenOptions,
    root: Option<ComponentRef>,
    implicit_root: ComponentRef,
    implicit_doc: ComponentRef,
    implicit_lines: Rc<RefCell<Rc<[String]>>>,
    implicit_scroll: Rc<dyn ScrollLayoutState>,
    layout: Option<LayoutFrame>,
    previous: Vec<String>,
    dimensions: (usize, usize),
    view_dirty: Rc<Cell<bool>>,
    wake: Rc<dyn Fn()>,
    wheel: WheelScrollAccelerator,
    clock: Instant,
    active: bool,
    flashes: AltScreenFlashContainer,
    search: Option<ActiveSearch>,
    end_indicator: Option<OverlayBounds>,
    mouse: MouseState,
    image_protocol: Option<ImageProtocol>,
    saved_caps: Option<TerminalCapabilities>,
    uploaded: IndexMap<u32, UploadedImage>,
}

impl TuiAltScreen {
    pub fn new(
        terminal: impl Terminal + 'static,
        hardware_cursor: bool,
        options: TuiAltScreenOptions,
    ) -> Self {
        let implicit_lines = Rc::new(RefCell::new(Rc::from([])));
        let implicit_doc = ComponentRef::new(Document(implicit_lines.clone()));
        let scroll = ScrollView::new(
            implicit_doc.clone(),
            ScrollViewOptions {
                follow_end: true,
                primary: true,
                ..Default::default()
            },
        );
        let implicit_scroll = scroll.state.clone();
        let implicit_root = ComponentRef::new(scroll);
        let view_dirty = Rc::new(Cell::new(false));
        let flag = view_dirty.clone();
        let wake: Rc<dyn Fn()> = Rc::new(move || flag.set(true));
        let mut wheel = WheelScrollAccelerator::default();
        wheel.set_lines(options.wheel_scroll_lines);
        Self {
            base: TuiBase::new(Box::new(terminal), hardware_cursor),
            options,
            root: None,
            implicit_root,
            implicit_doc,
            implicit_lines,
            implicit_scroll,
            layout: None,
            previous: Vec::new(),
            dimensions: (0, 0),
            view_dirty,
            wake: wake.clone(),
            wheel,
            clock: Instant::now(),
            active: false,
            flashes: AltScreenFlashContainer::new(wake),
            search: None,
            end_indicator: None,
            mouse: MouseState::default(),
            image_protocol: None,
            saved_caps: None,
            uploaded: IndexMap::new(),
        }
    }

    pub fn step(&mut self, timeout: Duration) -> io::Result<()> {
        let events = self
            .base
            .terminal
            .poll_input(timeout.min(Duration::from_millis(16)))?;
        for event in events {
            match event {
                TerminalEvent::Input(input) => self.handle_input(&input),
                TerminalEvent::Resize { .. } => self.base.request_render(false),
            }
        }
        let now = Instant::now();
        self.base.tick_components(now);
        if self.root.as_ref().is_some_and(|root| root.tick_tree(now)) {
            self.base.request_render(false);
        }
        self.flashes.tick(now);
        if let Some(layout) = &self.layout {
            expire_scrollbars(&layout.root, now);
        }
        self.advance_selection_scroll(now);
        self.base.expire_color_queries(now);
        if self.view_dirty.replace(false) {
            self.base.request_view_render();
        }
        if self.base.render_due(now) {
            self.render_now(false)?;
        }
        Ok(())
    }

    pub fn set_wheel_scroll_lines(&mut self, lines: WheelScrollLines) {
        self.wheel.set_lines(lines);
    }
    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.options.copy_on_select = enabled;
    }
    pub fn copy_on_select(&self) -> bool {
        self.options.copy_on_select
    }
    pub fn flash(&mut self, message: String, duration: Option<Duration>) {
        self.flashes.flash(message, duration);
    }
    pub fn screen_lines(&self) -> &[String] {
        &self.previous
    }
    pub fn viewport_top(&self) -> usize {
        self.primary().scroll_top()
    }
    pub fn is_following_output(&self) -> bool {
        self.primary().is_following_end()
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.primary().scroll_by(lines);
        self.base.request_view_render();
    }

    pub fn scroll_to_top(&mut self) {
        self.primary().scroll_to(0, false);
        self.base.request_view_render();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.primary().scroll_to(usize::MAX, false);
        self.base.request_view_render();
    }

    fn primary(&self) -> &Rc<dyn ScrollLayoutState> {
        self.layout
            .as_ref()
            .and_then(|layout| layout.primary_scroll_view.as_ref())
            .unwrap_or(&self.implicit_scroll)
    }

    fn scroll_to_prompt(&mut self, after: bool) {
        let Some(lines) = self
            .layout
            .as_ref()
            .and_then(|f| get_scroll_view_box(f, self.primary()))
            .and_then(|b| b.scroll_content_lines.as_ref())
        else {
            return;
        };
        let prompt = |line: &String| {
            line.strip_prefix("\x1b]133;A")
                .is_some_and(|rest| rest.starts_with('\x07') || rest.starts_with("\x1b\\"))
        };
        let top = self.viewport_top();
        let row = if after {
            lines
                .iter()
                .enumerate()
                .skip(top + 1)
                .find(|(_, line)| prompt(line))
                .map(|(i, _)| i)
        } else {
            lines[..top.min(lines.len())].iter().rposition(prompt)
        };
        if let Some(row) = row {
            self.primary().scroll_to(row, false);
            self.base.request_view_render();
        }
    }

    fn close_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.base.hide_overlay(search.overlay);
        }
    }

    fn toggle_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
            return;
        }
        let pending = Rc::new(RefCell::new(None));
        let query = pending.clone();
        let component = Rc::new(RefCell::new(AltScreenSearchComponent::new(
            Box::new(move |next| *query.borrow_mut() = Some(next.to_owned())),
            self.options.search_navigation_button_style.clone(),
        )));
        let overlay = self.base.show_overlay(
            ComponentRef::from_shared(component.clone()),
            OverlayOptions {
                anchor: OverlayAnchor::TopRight,
                width: Some(SizeValue::Percent(40.)),
                min_width: Some(32),
                margin: 1.into(),
                ..Default::default()
            },
        );
        self.search = Some(ActiveSearch {
            component,
            overlay,
            index: AltScreenSearchIndex::default(),
            pending,
            query: String::new(),
            selected: None,
            key: None,
            anchor_row: self.viewport_top(),
            mode: SearchMode::Query,
        });
    }

    fn navigate_search(&mut self, direction: i8) {
        if let Some(search) = &mut self.search
            && !search.query.is_empty()
        {
            search.mode = if direction < 0 {
                SearchMode::Previous
            } else {
                SearchMode::Next
            };
            self.base.request_view_render();
        }
    }

    fn refresh_search(&mut self, layout: &LayoutFrame) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        let primary = layout
            .primary_scroll_view
            .as_ref()
            .unwrap_or(&self.implicit_scroll);
        if let Some(query) = search.pending.borrow_mut().take()
            && query != search.query
        {
            search.anchor_row = search
                .key
                .map_or(primary.scroll_top(), |(row, _, _, _)| row);
            search.query = query;
            search.mode = SearchMode::Query;
        }
        let lines = get_scroll_view_box(layout, primary)
            .and_then(|node| node.scroll_content_lines.clone())
            .unwrap_or_else(|| Rc::from([]));
        let result = search.index.search(lines, &search.query);
        if !result.changed && search.mode == SearchMode::Retain {
            return false;
        }
        let matches = result.matches;
        let previous = if result.changed {
            search.key.and_then(|key| {
                matches
                    .iter()
                    .position(|m| get_alt_screen_search_match_key(m) == Some(key))
            })
        } else {
            search.selected
        }
        .or(search
            .selected
            .map(|i| i.min(matches.len().saturating_sub(1))));
        let reveal = search.mode != SearchMode::Retain;
        search.selected = if matches.is_empty() {
            None
        } else {
            Some(match search.mode {
                SearchMode::Query => {
                    let index = matches.partition_point(|hit| {
                        hit.segments
                            .first()
                            .is_some_and(|s| s.row < search.anchor_row)
                    });
                    if index == matches.len() { 0 } else { index }
                }
                SearchMode::Next => previous.map_or(0, |i| (i + 1) % matches.len()),
                SearchMode::Previous => previous
                    .filter(|&i| i > 0)
                    .map_or(matches.len() - 1, |i| i - 1),
                SearchMode::Retain => previous.unwrap_or(0),
            })
        };
        search.key = search
            .selected
            .and_then(|i| get_alt_screen_search_match_key(&matches[i]));
        search.mode = SearchMode::Retain;
        search
            .component
            .borrow_mut()
            .set_result(search.selected, matches.len());
        if let Some((first, _, last, _)) = search
            .key
            .filter(|_| reveal && primary.viewport_height() != 0)
        {
            let before = primary.scroll_top();
            let height = primary.viewport_height();
            let target = if first < before || last >= before + height {
                first.saturating_sub(height / 3)
            } else {
                before
            };
            primary.scroll_to(target, true);
            return primary.scroll_top() != before;
        }
        false
    }

    fn paint_search(&self, screen: &mut [String], layout: &LayoutFrame) {
        let Some(search) = &self.search else { return };
        let Some(selected) = search.selected else {
            return;
        };
        let Some(primary) = &layout.primary_scroll_view else {
            return;
        };
        let Some(node) = get_scroll_view_box(layout, primary) else {
            return;
        };
        let first = node.clip.y.max(0);
        let last = node.clip.bottom().min(screen.len() as isize);
        let min_col = node.clip.x;
        let max_col = node
            .clip
            .right()
            .min(get_scrollbar_geometry(node, false).map_or(layout.width, |g| g.column));
        let offset = node.rect.y - primary.scroll_top() as isize;
        let hit = search.index.matches();
        let mut ranges = vec![Vec::new(); (last - first).max(0) as usize];
        let start = hit.partition_point(|m| {
            m.segments
                .last()
                .is_some_and(|s| s.row as isize + offset < first)
        });
        for (i, m) in hit
            .iter()
            .enumerate()
            .skip(start)
            .take_while(|(_, m)| m.segments[0].row as isize + offset < last)
        {
            for s in &m.segments {
                let row = offset + s.row as isize;
                let a = (node.rect.x + s.start_col).max(min_col);
                let b = (node.rect.x + s.end_col).min(max_col);
                if (first..last).contains(&row) && b > a {
                    ranges[(row - first) as usize].push((a, b, i == selected));
                }
            }
        }
        for (i, mut ranges) in ranges.into_iter().enumerate() {
            let line = &mut screen[first as usize + i];
            if is_image_line(line) {
                continue;
            }
            ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
            for (a, b, selected) in ranges {
                let style = |text: &str| {
                    if selected {
                        self.options
                            .search_current_match_style
                            .as_ref()
                            .map_or_else(
                                || format!("\x1b[1;7m{text}\x1b[22;27m"),
                                |func| func(text),
                            )
                    } else {
                        self.options
                            .search_match_style
                            .as_ref()
                            .map_or_else(|| format!("\x1b[4m{text}\x1b[24m"), |func| func(text))
                    }
                };
                highlight(line, a, b, |text| style_plain_runs(text, style));
            }
        }
    }

    fn paint_end(&mut self, screen: &mut [String], layout: &LayoutFrame) {
        self.end_indicator = None;
        let Some(label) = &self.options.scroll_to_end_indicator else {
            return;
        };
        let Some(view) = &layout.primary_scroll_view else {
            return;
        };
        if !view.follows_end() || view.is_following_end() {
            return;
        }
        let Some(node) = get_scroll_view_box(layout, view) else {
            return;
        };
        if node.clip.width == 0 || node.clip.height == 0 {
            return;
        }
        let row = node.clip.bottom().saturating_sub(1) as usize;
        let Some(line) = screen.get_mut(row).filter(|line| !is_image_line(line)) else {
            return;
        };
        let label = truncate_with_ellipsis(&label(), node.clip.width, "", false);
        let col = node.clip.x + node.clip.width.saturating_sub(visible_width(&label)) / 2;
        let right = get_scrollbar_geometry(node, false).map_or(node.clip.right(), |g| g.column);
        let label = truncate_with_ellipsis(&label, right.saturating_sub(col), "", false);
        let width = visible_width(&label);
        if width == 0 {
            return;
        }
        *line = composite_tui_line(line, &label, col, width, layout.width);
        self.end_indicator = Some(OverlayBounds {
            row,
            col,
            width,
            height: 1,
        });
    }

    fn handle_viewport_input(&mut self, data: &str) -> bool {
        if self.handle_mouse_input(data) {
            return true;
        }
        let keys = get_keybindings();
        let is = |id| keys.matches(data, id);
        let press = !is_key_release_data(data);
        if is("tui.altScreen.search") {
            if press {
                self.toggle_search();
            }
            return true;
        }
        let searching = self
            .search
            .as_ref()
            .is_some_and(|s| self.base.overlay_focused(s.overlay));
        if searching {
            if is("tui.altScreen.searchNext") {
                if press {
                    self.navigate_search(1);
                }
                return true;
            }
            if is("tui.altScreen.searchPrevious") {
                if press {
                    self.navigate_search(-1);
                }
                return true;
            }
            if is("tui.altScreen.searchClose") {
                if press {
                    self.close_search();
                }
                return true;
            }
        }
        if self
            .base
            .focused()
            .is_some_and(|c| c.borrow().captures_viewport_input())
            || self.base.is_overlay_focused() && !searching
        {
            return false;
        }
        let height = self.primary().viewport_height();
        let page = height.saturating_sub(4).max(1) as isize;
        let half = (height / 2).max(1) as isize;
        let action: fn(&mut Self) = if is("tui.altScreen.top") {
            Self::scroll_to_top
        } else if is("tui.altScreen.bottom") {
            Self::scroll_to_bottom
        } else if is("tui.altScreen.previousPrompt") {
            |s| s.scroll_to_prompt(false)
        } else if is("tui.altScreen.nextPrompt") {
            |s| s.scroll_to_prompt(true)
        } else {
            let delta = if is("tui.altScreen.pageUp") {
                -page
            } else if is("tui.altScreen.pageDown") {
                page
            } else if is("tui.altScreen.halfPageUp") {
                -half
            } else if is("tui.altScreen.halfPageDown") {
                half
            } else if is("tui.altScreen.lineUp") {
                -1
            } else if is("tui.altScreen.lineDown") {
                1
            } else {
                return false;
            };
            if press {
                self.scroll_by(delta);
            }
            return true;
        };
        if press {
            action(self);
        }
        true
    }

    fn draw(&mut self, force: bool) {
        if !self.active || self.base.stopped {
            return;
        }
        let (width, height) = (
            self.base.terminal.width().max(1),
            self.base.terminal.height().max(1),
        );
        let changed =
            std::mem::take(&mut self.base.content_changed) || self.dimensions != (width, height);
        let mut seed = LayoutRenderCache::default();
        let reusable = if self.root.is_none() && changed {
            let lines: Rc<[String]> = self.base.content.render(width).into();
            *self.implicit_lines.borrow_mut() = lines.clone();
            seed.insert(self.implicit_doc.clone(), width, lines);
            Some(&seed)
        } else {
            self.layout
                .as_ref()
                .filter(|_| !changed)
                .map(|l| &l.render_cache)
        };
        let root = self.root.as_ref().unwrap_or(&self.implicit_root);
        let mut layout =
            render_layout_frame(root.clone(), width, height, self.wake.clone(), reusable);
        if self.refresh_search(&layout) {
            let root = self.root.as_ref().unwrap_or(&self.implicit_root);
            layout = render_layout_frame(
                root.clone(),
                width,
                height,
                self.wake.clone(),
                Some(&layout.render_cache),
            );
        }
        let mut screen = std::mem::take(&mut layout.lines);
        for line in &mut screen {
            resolve_fake_cursors(line, self.base.show_hardware_cursor());
        }
        self.paint_search(&mut screen, &layout);
        self.paint_end(&mut screen, &layout);
        screen = self.base.composite_overlays(screen, width, height);
        self.paint_selection(&mut screen, &layout);
        let flash = self.flashes.render(width);
        let take = flash.len().saturating_sub(height);
        for (row, line) in screen.iter_mut().zip(flash.into_iter().skip(take)) {
            let cells = visible_width(&line);
            if cells != 0 {
                *row = composite_tui_line(
                    row,
                    &line,
                    width.saturating_sub(cells),
                    cells.min(width),
                    width,
                );
            }
        }
        let cursor = extract_cursor_position(&mut screen, height);
        for line in &mut screen {
            reset_line(line);
            if !is_image_line(line) && visible_width(line) > width {
                *line = slice_by_column(line, 0, width, true);
            }
        }
        self.write_screen(&screen, width, height, cursor, force);
        self.previous = screen;
        self.layout = Some(layout);
        self.dimensions = (width, height);
    }

    fn write_screen(
        &mut self,
        screen: &[String],
        width: usize,
        height: usize,
        cursor: Option<(usize, usize)>,
        force: bool,
    ) {
        let full = force || self.previous.is_empty() || self.dimensions != (width, height);
        let changed = |row: usize| self.previous.get(row) != screen.get(row);
        let kitty = self.image_protocol == Some(ImageProtocol::Kitty);
        let redraw = screen.iter().enumerate().any(|(row, line)| {
            if changed(row)
                && (is_image_line(line)
                    || self.previous.get(row).is_some_and(|old| is_image_line(old)))
            {
                return true;
            }
            kitty
                && get_kitty_image_placement_rows(line)
                    .is_some_and(|count| (row..(row + count).min(height)).any(changed))
        });
        let had_uploads = !self.uploaded.is_empty();
        let show_cursor = self.base.show_hardware_cursor();
        let mut out = Output::new(self.base.terminal.as_mut());
        out.push(SYNC_ON);
        if full {
            self.base.full_redraws += 1;
            if kitty && had_uploads {
                out.push(delete_all_kitty_placements());
            } else {
                if kitty {
                    out.push(delete_all_kitty_images());
                }
                out.push("\x1b[2J");
            }
        } else if redraw {
            if kitty {
                out.push(delete_all_kitty_placements());
            } else if self.image_protocol == Some(ImageProtocol::Iterm2) {
                out.push("\x1b[2J");
            }
        }
        let mut placements = Vec::new();
        if kitty && (full || redraw) {
            let mut visible = IndexSet::new();
            for (row, line) in screen
                .iter()
                .enumerate()
                .filter(|(_, line)| is_image_line(line))
            {
                let replacement = get_kitty_image_placement(line).and_then(|placement| {
                    visible.insert(placement.image_id);
                    let next = UploadedImage {
                        generation: placement.transmission_generation,
                        transmission: placement.transmission_bytes,
                        decoded: placement.estimated_decoded_bytes,
                    };
                    let previous = self.uploaded.shift_remove(&placement.image_id);
                    self.uploaded.insert(placement.image_id, next);
                    previous
                        .filter(|last| last.generation == next.generation)
                        .map(|_| placement.replacement_line)
                });
                placements.push((row, replacement));
            }
            let mut usage = self
                .uploaded
                .iter()
                .filter(|(id, _)| !visible.contains(*id))
                .fold((0, 0_usize, 0_usize), |(n, t, d), (_, entry)| {
                    (
                        n + 1,
                        t.saturating_add(entry.transmission),
                        d.saturating_add(entry.decoded),
                    )
                });
            self.uploaded.retain(|id, entry| {
                if visible.contains(id)
                    || (usage.0 <= 16 && usage.1 <= 32 * 1024 * 1024 && usage.2 <= 64 * 1024 * 1024)
                {
                    return true;
                }
                out.push(&delete_kitty_image(*id));
                usage = (
                    usage.0 - 1,
                    usage.1 - entry.transmission,
                    usage.2 - entry.decoded,
                );
                false
            });
        }
        if placements.is_empty() {
            for (row, line) in screen
                .iter()
                .enumerate()
                .filter(|(row, _)| full || redraw || changed(*row))
            {
                out.push(&format!("\x1b[{};1H\x1b[2K", row + 1));
                out.push(line);
            }
        } else {
            for row in 1..=height {
                out.push(&format!("\x1b[{row};1H\x1b[2K"));
            }
            for (row, line) in screen
                .iter()
                .enumerate()
                .filter(|(_, line)| !is_image_line(line))
            {
                out.push(&format!("\x1b[{};1H", row + 1));
                out.push(line);
            }
            for (row, line) in placements {
                out.push(&format!("\x1b[{};1H", row + 1));
                out.push(line.as_deref().unwrap_or(&screen[row]));
            }
        }
        if let Some((row, col)) = cursor {
            out.push(&format!("\x1b[{};{}H", row + 1, col.min(width) + 1));
            out.push(if show_cursor {
                "\x1b[?25h"
            } else {
                "\x1b[?25l"
            });
        } else {
            out.push("\x1b[?25l");
        }
        out.push(SYNC_OFF);
        out.flush();
    }
}

fn highlight(line: &mut String, start: usize, end: usize, style: impl FnOnce(&str) -> String) {
    let width = visible_width(line);
    let end = end.min(width);
    if end <= start {
        return;
    }
    let mut before = slice_by_column(line, 0, start, true);
    before.push_str(&style(&slice_by_column(line, start, end - start, true)));
    before.push_str(&slice_by_column(line, end, width - end, true));
    *line = before;
}

fn style_plain_runs(text: &str, style: impl Fn(&str) -> String) -> String {
    let mut result = String::new();
    let mut i = 0;
    while let Some(index) = text[i..].find('\x1b').map(|index| i + index) {
        if index != i {
            result.push_str(&style(&text[i..index]));
        }
        if let Some(code) = extract_ansi_code(text, index) {
            result.push_str(code);
            i = index + code.len();
        } else {
            result.push('\x1b');
            i = index + 1;
        }
    }
    if i < text.len() {
        result.push_str(&style(&text[i..]));
    }
    result
}

impl Component for TuiAltScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.root.as_ref().map_or_else(
            || self.base.content.render(width),
            |root| root.borrow().render(width),
        )
    }
    fn handle_input(&mut self, data: &str) {
        if self.handle_viewport_input(data) {
            return;
        }
        if let Some(data) = self.base.filter_input(data) {
            self.base.deliver_input(&data);
        }
    }
    fn invalidate(&mut self) {
        self.base.invalidate();
        if let Some(root) = &self.root {
            root.borrow_mut().invalidate();
        }
        self.base.request_render(false);
    }
}

impl Tui for TuiAltScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Fullscreen
    }
    fn base(&self) -> &TuiBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }
    fn start(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        let term = std::env::var("TERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let multiplexed = term.starts_with("tmux")
            || term.starts_with("screen")
            || ["TMUX", "ZELLIJ", "STY"]
                .into_iter()
                .any(|name| std::env::var_os(name).is_some());
        let mouse = if !self.options.mouse {
            ""
        } else if multiplexed {
            MOUSE_BUTTONS
        } else {
            MOUSE_MOTION
        };
        self.base
            .terminal
            .write(&format!("{ENTER}{NO_WRAP}{mouse}\x1b[2J\x1b[H\x1b[?25l"));
        self.active = true;
        let caps = get_capabilities();
        self.image_protocol = caps.images;
        if caps.images == Some(ImageProtocol::Iterm2) {
            self.saved_caps = Some(caps);
            set_capabilities(TerminalCapabilities {
                images: None,
                ..caps
            });
            self.invalidate();
        }
        self.uploaded.clear();
        self.previous.clear();
        self.layout = None;
        self.flashes.dispose();
        self.clear_mouse_gestures();
        self.base.set_exit_frame(
            if self.options.mouse {
                concat!(
                    "\x1b[?2026l",
                    "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                    "\x1b[?7h"
                )
            } else {
                "\x1b[?2026l\x1b[?7h"
            },
            "\x1b[?1049l\x1b[?25h",
        );
        if let Err(error) = self.base.start() {
            let _ = self.stop(TuiStopOptions {
                preserve_screen: true,
            });
            return Err(error);
        }
        Ok(())
    }

    fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.close_search();
        self.clear_mouse_gestures();
        self.flashes.dispose();
        self.base.begin_stop();
        let images = if self.image_protocol == Some(ImageProtocol::Kitty) {
            delete_all_kitty_images()
        } else {
            ""
        };
        let mouse = if self.options.mouse { NO_MOUSE } else { "" };
        self.base
            .terminal
            .write(&format!("{SYNC_ON}{images}{mouse}{WRAP}{SYNC_OFF}"));
        let stop = self.base.finish_stop();
        self.active = false;
        self.uploaded.clear();
        if options.preserve_screen {
            self.base
                .terminal
                .write(&format!("{SYNC_ON}{EXIT}\x1b[?25h{SYNC_OFF}"));
        } else {
            let width = self.base.terminal.width().max(1);
            let lines = self.render(width);
            let mut out = Output::new(self.base.terminal.as_mut());
            out.push(&format!("{SYNC_ON}{EXIT}{NO_WRAP}"));
            for (row, mut line) in lines.into_iter().enumerate() {
                if row != 0 {
                    out.push("\r\n");
                }
                let prefix = line.len() - strip_prompt_markers(&line).len();
                line.drain(..prefix);
                resolve_fake_cursors(&mut line, false);
                reset_line(&mut line);
                out.push("\r\x1b[2K");
                if !is_image_line(&line) && visible_width(&line) > width {
                    out.push(&slice_by_column(&line, 0, width, true));
                } else {
                    out.push(&line);
                }
            }
            out.push(&format!("\x1b[0m{WRAP}\r\n\x1b[?25h{SYNC_OFF}"));
            out.flush();
        }
        if let Some(caps) = self.saved_caps.take() {
            set_capabilities(caps);
        }
        stop
    }

    fn render_now(&mut self, force: bool) -> io::Result<()> {
        let force = self.base.begin_frame(force);
        self.view_dirty.set(false);
        self.draw(force);
        Ok(())
    }
}

impl ViewportTui for TuiAltScreen {
    fn set_layout_root(&mut self, root: Option<ComponentRef>) {
        if self.root != root {
            self.root = root;
            self.layout = None;
            self.base.request_render(false);
        }
    }
}

impl Drop for TuiAltScreen {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop(TuiStopOptions {
                preserve_screen: true,
            });
        }
    }
}
