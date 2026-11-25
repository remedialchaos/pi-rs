//! Viewport layout, clipping, painting and hit testing.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::components::scroll_view::Scrollbar;
use crate::components::stack::{alignment_offset, allocate_stack_sizes, visible_stack_entries};
use crate::layout_node::{
    LayoutNode, LayoutViewport, ScrollLayoutState, StackAlign, StackAxis, StackBasis,
    get_layout_node,
};
use crate::terminal_image::{
    crop_kitty_image_line, get_kitty_image_metadata, get_max_kitty_image_rows, is_image_line,
};
use crate::tui::{
    CURSOR_MARKER, ComponentRef, SEGMENT_RESET, composite_tui_line, strip_prompt_markers,
};
use crate::utils::{
    extract_ansi_code, get_active_background_ansi, get_grapheme_cell_range, slice_by_column,
    visible_width,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: usize,
    pub y: isize,
    pub width: usize,
    pub height: usize,
}

impl LayoutRect {
    pub(crate) fn right(self) -> usize {
        self.x.saturating_add(self.width)
    }
    pub(crate) fn bottom(self) -> isize {
        self.y.saturating_add_unsigned(self.height)
    }
    fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Self {
            x,
            y,
            width: self.right().min(other.right()).saturating_sub(x),
            height: self.bottom().min(other.bottom()).saturating_sub(y).max(0) as usize,
        }
    }
    pub fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.right() && y as isize >= self.y && (y as isize) < self.bottom()
    }
}

type Lines = Rc<[String]>;

pub struct LayoutBox {
    pub component: ComponentRef,
    pub rect: LayoutRect,
    pub clip: LayoutRect,
    pub children: Vec<LayoutBox>,
    pub lines: Option<Lines>,
    pub line_offset: usize,
    pub scroll_view: Option<Rc<dyn ScrollLayoutState>>,
    pub scroll_content_lines: Option<Lines>,
    pub layer: usize,
}

impl LayoutBox {
    fn new(component: ComponentRef, rect: LayoutRect, clip: LayoutRect) -> Self {
        Self {
            component,
            rect,
            clip: clip.intersection(rect),
            children: Vec::new(),
            lines: None,
            line_offset: 0,
            scroll_view: None,
            scroll_content_lines: None,
            layer: 0,
        }
    }

    fn translate_and_clip(&mut self, delta: isize, parent: LayoutRect) {
        self.rect.y = self.rect.y.saturating_add(delta);
        self.clip = parent.intersection(self.rect);
        for child in &mut self.children {
            child.translate_and_clip(delta, self.clip);
        }
    }
}

#[derive(Default)]
pub struct LayoutRenderCache(HashMap<(ComponentRef, usize), Lines>);

impl LayoutRenderCache {
    pub(crate) fn insert(&mut self, component: ComponentRef, width: usize, lines: Rc<[String]>) {
        self.0.insert((component, width), lines);
    }
}

pub struct LayoutFrame {
    pub root: LayoutBox,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
    pub primary_scroll_view: Option<Rc<dyn ScrollLayoutState>>,
    pub render_cache: LayoutRenderCache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: usize,
    pub track_top: isize,
    pub track_height: usize,
    pub thumb_top: isize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

struct Context<'a> {
    viewport: LayoutViewport,
    cache: LayoutRenderCache,
    reusable: Option<&'a LayoutRenderCache>,
    request_render: Rc<dyn Fn()>,
    primary: Option<Rc<dyn ScrollLayoutState>>,
}

impl Context<'_> {
    fn render(&mut self, component: &ComponentRef, width: usize) -> Lines {
        let key = (component.clone(), width.max(1));
        if let Some(lines) = self.cache.0.get(&key) {
            return lines.clone();
        }
        let lines = self
            .reusable
            .and_then(|cache| cache.0.get(&key))
            .cloned()
            .unwrap_or_else(|| component.borrow().render(key.1).into());
        self.cache.0.insert(key, lines.clone());
        lines
    }

    fn layout(
        &mut self,
        component: ComponentRef,
        x: usize,
        y: isize,
        width: usize,
        height: Option<usize>,
        clip: LayoutRect,
    ) -> LayoutBox {
        let width = width.max(1);
        let rect = |height| LayoutRect {
            x,
            y,
            width,
            height,
        };
        match get_layout_node(&component) {
            None => {
                let lines = self.render(&component, width);
                let height = height.unwrap_or(lines.len());
                let mut node = LayoutBox::new(component, rect(height), clip);
                if lines.len() > height && height != 0 {
                    node.line_offset = lines
                        .iter()
                        .position(|line| line.contains(CURSOR_MARKER))
                        .map_or(0, |i| (i + 1).saturating_sub(height));
                }
                node.lines = Some(lines);
                node
            }
            Some(LayoutNode::Scroll(node)) => {
                let before = node.state.scroll_top() as isize;
                let content_width = node.state.content_width(width);
                let mut child = self.layout(
                    node.component.clone(),
                    x,
                    y.saturating_sub(before),
                    content_width,
                    None,
                    clip,
                );
                let extent = height.unwrap_or(child.rect.height);
                node.state.set_request_render(self.request_render.clone());
                node.state.update_layout(child.rect.height, extent);
                if node.state.primary() || self.primary.is_none() {
                    self.primary = Some(node.state.clone());
                }
                let mut viewport = LayoutBox::new(component, rect(extent), clip);
                child.translate_and_clip(before - node.state.scroll_top() as isize, viewport.clip);
                viewport.scroll_content_lines = Some(self.render(&node.component, content_width));
                viewport.children.push(child);
                viewport.scroll_view = Some(node.state);
                viewport
            }
            Some(LayoutNode::Stack(node)) => {
                let entries: Vec<_> = visible_stack_entries(&node.entries, self.viewport).collect();
                let horizontal = node.axis == StackAxis::Horizontal;
                let sizes = allocate_stack_sizes(
                    &entries,
                    entries.iter().map(|entry| match entry.basis {
                        StackBasis::Cells(size) => size,
                        StackBasis::Auto if horizontal => self
                            .render(&entry.component, width)
                            .iter()
                            .map(|line| visible_width(line))
                            .max()
                            .unwrap_or(0),
                        StackBasis::Auto => self.render(&entry.component, width).len(),
                    }),
                    if horizontal { Some(width) } else { height },
                    node.gap,
                );
                let mut result;
                if horizontal {
                    let intrinsic: Vec<_> = entries
                        .iter()
                        .zip(&sizes)
                        .map(|(e, &w)| self.render(&e.component, w).len())
                        .collect();
                    let height =
                        height.unwrap_or_else(|| intrinsic.iter().copied().max().unwrap_or(0));
                    result = LayoutBox::new(component, rect(height), clip);
                    let mut x = x;
                    for ((entry, width), natural_height) in
                        entries.into_iter().zip(sizes).zip(intrinsic)
                    {
                        let child_height = if node.align == StackAlign::Stretch {
                            height
                        } else {
                            height.min(natural_height)
                        };
                        let y = y.saturating_add_unsigned(alignment_offset(
                            node.align,
                            height - child_height,
                        ));
                        let child = if width == 0 {
                            LayoutBox::new(
                                entry.component.clone(),
                                LayoutRect {
                                    x,
                                    y,
                                    width,
                                    height: child_height,
                                },
                                result.clip,
                            )
                        } else {
                            self.layout(
                                entry.component.clone(),
                                x,
                                y,
                                width,
                                Some(child_height),
                                result.clip,
                            )
                        };
                        result.children.push(child);
                        x = x.saturating_add(width).saturating_add(node.gap);
                    }
                } else {
                    let height = height.unwrap_or_else(|| {
                        sizes.iter().copied().fold(
                            sizes.len().saturating_sub(1).saturating_mul(node.gap),
                            usize::saturating_add,
                        )
                    });
                    result = LayoutBox::new(component, rect(height), clip);
                    let mut y = y;
                    for (entry, size) in entries.into_iter().zip(sizes) {
                        result.children.push(self.layout(
                            entry.component.clone(),
                            x,
                            y,
                            width,
                            Some(size),
                            result.clip,
                        ));
                        y = y.saturating_add_unsigned(size.saturating_add(node.gap));
                    }
                }
                result
            }
        }
    }
}

pub fn get_scrollbar_geometry(
    node: &LayoutBox,
    include_hidden_auto: bool,
) -> Option<ScrollbarGeometry> {
    let view = node.scroll_view.as_ref()?;
    let rect = node.rect;
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let content = node.children.first()?.rect.height;
    let track = rect.height;
    let can_reveal = include_hidden_auto && view.scrollbar() == Scrollbar::Auto && content > track;
    if !(view.is_scrollbar_visible() || can_reveal) {
        return None;
    }
    let thumb = ((track as f64 * track as f64 / content as f64).round() as usize)
        .clamp(track.min(2), track);
    let max_top = content.saturating_sub(track);
    let top = if max_top == 0 {
        0
    } else {
        (view.scroll_top() as f64 / max_top as f64 * (track - thumb) as f64).round() as usize
    };
    let col = rect.right() - 1;
    (col >= node.clip.x && col < node.clip.right()).then_some(ScrollbarGeometry {
        column: col,
        track_top: rect.y,
        track_height: track,
        thumb_top: rect.y.saturating_add_unsigned(top),
        thumb_height: thumb,
        max_scroll_top: max_top,
    })
}

fn replace_scrollbar_cell(
    line: &str,
    column: usize,
    width: usize,
    replacement: &str,
    preserve_bg: bool,
) -> String {
    if is_image_line(line) {
        return line.to_owned();
    }
    let (start, end) = get_grapheme_cell_range(line, column)
        .map_or((column, column + 1), |range| (range.start, range.end));
    let mut result = slice_by_column(line, 0, start, true);
    result.extend(std::iter::repeat_n(
        ' ',
        start.saturating_sub(visible_width(&result)),
    ));
    result.push_str(SEGMENT_RESET);
    if preserve_bg {
        let target = slice_by_column(line, start, end - start, true);
        let mut index = 0;
        while let Some(sequence) = extract_ansi_code(&target, index) {
            index += sequence.len();
        }
        result.push_str(&get_active_background_ansi(&target[..index]));
    }
    result.extend(std::iter::repeat_n(' ', column - start));
    result.push_str(replacement);
    result.extend(std::iter::repeat_n(' ', end - column - 1));
    result.push_str(&slice_by_column(line, end, width.saturating_sub(end), true));
    result
}

fn paint(node: &LayoutBox, screen: &mut [String], width: usize) {
    let full_width = node.rect.x == 0 && node.rect.width >= width;
    let rows = node.clip.y.max(0) as usize..(node.clip.bottom().max(0) as usize).min(screen.len());
    if let Some(lines) = &node.lines {
        for row in rows.clone() {
            let source = node.line_offset + (row as isize - node.rect.y) as usize;
            let Some(line) = lines.get(source) else { break };
            let line = strip_prompt_markers(line);
            let line = if let Some(image) = get_kitty_image_metadata(line) {
                let height = image.rows.min(rows.end - row);
                crop_kitty_image_line(line, 0, height)
            } else {
                std::borrow::Cow::Borrowed(line)
            };
            if full_width && (is_image_line(&line) || screen[row].is_empty()) {
                screen[row] = line.into_owned();
            } else {
                let clipped = node.clip.right().saturating_sub(node.rect.x);
                if clipped != 0 {
                    screen[row] =
                        composite_tui_line(&screen[row], &line, node.rect.x, clipped, width);
                }
            }
        }
    }
    for child in &node.children {
        paint(child, screen, width);
    }
    if let (Some(view), Some(lines)) = (&node.scroll_view, &node.scroll_content_lines) {
        let top = view.scroll_top();
        if full_width && node.rect.y >= 0 && rows.contains(&(node.rect.y as usize)) && top != 0 {
            let start = top.saturating_sub(get_max_kitty_image_rows());
            for (index, line) in lines.iter().enumerate().take(top).skip(start).rev() {
                let Some(image) = get_kitty_image_metadata(line) else {
                    continue;
                };
                let hidden = top - index;
                if hidden < image.rows {
                    let y = node.rect.y as usize;
                    screen[y] = crop_kitty_image_line(
                        line,
                        hidden,
                        (rows.end - y).min(image.rows - hidden),
                    )
                    .into_owned();
                }
                break;
            }
        }
        if let Some(geometry) = get_scrollbar_geometry(node, false) {
            let thumb = view.style_scrollbar(true);
            let track = view.style_scrollbar(false);
            let thumb_rows = geometry.thumb_top
                ..geometry
                    .thumb_top
                    .saturating_add_unsigned(geometry.thumb_height);
            let preserve = view.scrollbar() != Scrollbar::Always;
            for row in rows {
                let cell = if thumb_rows.contains(&(row as isize)) {
                    &thumb
                } else {
                    &track
                };
                screen[row] =
                    replace_scrollbar_cell(&screen[row], geometry.column, width, cell, preserve);
            }
        }
    }
}

pub fn render_layout_frame(
    root: ComponentRef,
    width: usize,
    height: usize,
    request_render: Rc<dyn Fn()>,
    reusable_output: Option<&LayoutRenderCache>,
) -> LayoutFrame {
    let (width, height) = (width.max(1), height.max(1));
    let mut context = Context {
        viewport: LayoutViewport { width, height },
        cache: LayoutRenderCache::default(),
        reusable: reusable_output,
        request_render,
        primary: None,
    };
    let root = context.layout(
        root,
        0,
        0,
        width,
        Some(height),
        LayoutRect {
            x: 0,
            y: 0,
            width,
            height,
        },
    );
    let mut lines = vec![String::new(); height];
    paint(&root, &mut lines, width);
    LayoutFrame {
        root,
        width,
        height,
        lines,
        primary_scroll_view: context.primary,
        render_cache: context.cache,
    }
}

pub fn get_layout_boxes_at(frame: &LayoutFrame, x: usize, y: usize) -> Vec<&LayoutBox> {
    fn visit<'a>(
        node: &'a LayoutBox,
        x: usize,
        y: usize,
        depth: usize,
        out: &mut Vec<(usize, &'a LayoutBox)>,
    ) {
        if !node.clip.contains(x, y) {
            return;
        }
        out.push((depth, node));
        for child in &node.children {
            visit(child, x, y, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    visit(&frame.root, x, y, 0, &mut out);
    out.sort_by_key(|&(depth, node)| std::cmp::Reverse((node.layer, depth)));
    out.into_iter().map(|(_, node)| node).collect()
}

pub fn get_scroll_view_box<'a>(
    frame: &'a LayoutFrame,
    view: &Rc<dyn ScrollLayoutState>,
) -> Option<&'a LayoutBox> {
    fn visit<'a>(node: &'a LayoutBox, view: &Rc<dyn ScrollLayoutState>) -> Option<&'a LayoutBox> {
        if node
            .scroll_view
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, view))
        {
            return Some(node);
        }
        node.children.iter().find_map(|child| visit(child, view))
    }
    visit(&frame.root, view)
}

pub fn get_scroll_views_at(
    frame: &LayoutFrame,
    x: usize,
    y: usize,
) -> Vec<&Rc<dyn ScrollLayoutState>> {
    get_layout_boxes_at(frame, x, y)
        .into_iter()
        .filter_map(|node| node.scroll_view.as_ref())
        .collect()
}

pub(crate) fn expire_scrollbars(node: &LayoutBox, now: Instant) {
    if let Some(view) = &node.scroll_view {
        view.expire_scrollbar(now);
    }
    for child in &node.children {
        expire_scrollbars(child, now);
    }
}
