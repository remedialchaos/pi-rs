use std::rc::Rc;

use crate::layout_node::{
    LayoutNode, LayoutViewport, StackAlign, StackAxis, StackBasis, StackLayoutEntry,
    StackLayoutNode,
};
use crate::tui::{Component, ComponentRef, Container, composite_tui_line};
use crate::utils::visible_width;

pub struct StackEntryOptions {
    pub basis: StackBasis,
    pub grow: f64,
    pub shrink: f64,
    pub min_size: Option<usize>,
    pub max_size: Option<usize>,
    pub visible: Option<Rc<dyn Fn(LayoutViewport) -> bool>>,
}

impl Default for StackEntryOptions {
    fn default() -> Self {
        Self {
            basis: StackBasis::Auto,
            grow: 0.,
            shrink: 1.,
            min_size: None,
            max_size: None,
            visible: None,
        }
    }
}

#[derive(Default)]
pub struct StackOptions {
    pub gap: usize,
    pub align: StackAlign,
}

pub struct Stack<const HORIZONTAL: bool> {
    children: Container,
    entries: Vec<StackLayoutEntry>,
    options: StackOptions,
}

impl<const HORIZONTAL: bool> Default for Stack<HORIZONTAL> {
    fn default() -> Self {
        Self::new([], StackOptions::default())
    }
}

impl<const HORIZONTAL: bool> Stack<HORIZONTAL> {
    pub fn new(
        children: impl IntoIterator<Item = (ComponentRef, StackEntryOptions)>,
        options: StackOptions,
    ) -> Self {
        let mut stack = Self {
            children: Container::default(),
            entries: Vec::new(),
            options,
        };
        for (component, entry) in children {
            stack.add_child(component, entry);
        }
        stack
    }

    pub fn add_child(&mut self, component: ComponentRef, entry: StackEntryOptions) {
        self.children.add_child(component.clone());
        self.entries.push(StackLayoutEntry {
            component,
            basis: entry.basis,
            grow: if entry.grow.is_finite() {
                entry.grow.floor().max(0.)
            } else {
                0.
            },
            shrink: if entry.shrink.is_finite() {
                entry.shrink.floor().max(0.)
            } else {
                1.
            },
            min_size: entry.min_size,
            max_size: entry.max_size,
            visible: entry.visible,
        });
    }

    pub fn remove_child(&mut self, child: &ComponentRef) {
        self.children.remove_child(child);
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.component == child)
        {
            self.entries.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.entries.clear();
    }
}

impl<const HORIZONTAL: bool> Component for Stack<HORIZONTAL> {
    fn children(&self) -> &[ComponentRef] {
        self.children.children()
    }
    fn invalidate(&mut self) {
        self.children.invalidate();
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(StackLayoutNode {
            axis: if HORIZONTAL {
                StackAxis::Horizontal
            } else {
                StackAxis::Vertical
            },
            entries: self.entries.clone(),
            gap: self.options.gap,
            align: self.options.align,
        }))
    }

    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let entries: Vec<_> = visible_stack_entries(
            &self.entries,
            LayoutViewport {
                width,
                height: usize::MAX,
            },
        )
        .collect();
        let gap = self.options.gap;
        let mut rendered: Vec<_> = entries
            .iter()
            .map(|entry| entry.component.borrow().render(width))
            .collect();
        if !HORIZONTAL {
            let mut lines = Vec::new();
            let sizes = allocate_stack_sizes(&entries, rendered.iter().map(Vec::len), None, gap);
            for (i, (mut child, size)) in rendered.into_iter().zip(sizes).enumerate() {
                if i != 0 {
                    lines.resize_with(lines.len() + gap, String::new);
                }
                child.resize_with(size, String::new);
                lines.extend(child);
            }
            return lines;
        }
        let sizes = allocate_stack_sizes(
            &entries,
            rendered.iter().map(|lines| {
                lines
                    .iter()
                    .map(|line| visible_width(line))
                    .max()
                    .unwrap_or(0)
            }),
            Some(width),
            gap,
        );
        for ((entry, child), size) in entries.iter().zip(&mut rendered).zip(sizes.iter().copied()) {
            match size {
                0 => child.clear(),
                size if size != width => *child = entry.component.borrow().render(size),
                _ => {}
            }
        }
        let height = rendered.iter().map(Vec::len).max().unwrap_or(0);
        let mut lines = vec![String::new(); height];
        let mut x = 0;
        for (child, size) in rendered.into_iter().zip(sizes) {
            let offset = alignment_offset(self.options.align, height - child.len());
            for (i, line) in child.into_iter().enumerate() {
                lines[i + offset] = composite_tui_line(&lines[i + offset], &line, x, size, width);
            }
            x = x.saturating_add(size).saturating_add(gap);
        }
        lines
    }
}

pub fn visible_stack_entries(
    entries: &[StackLayoutEntry],
    viewport: LayoutViewport,
) -> impl Iterator<Item = &StackLayoutEntry> {
    entries.iter().filter(move |entry| {
        entry
            .visible
            .as_ref()
            .is_none_or(|visible| visible(viewport))
    })
}

pub(crate) fn alignment_offset(align: StackAlign, free: usize) -> usize {
    match align {
        StackAlign::Center => free / 2,
        StackAlign::End => free,
        _ => 0,
    }
}

pub fn allocate_stack_sizes(
    entries: &[&StackLayoutEntry],
    intrinsic: impl IntoIterator<Item = usize>,
    available: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let mut sizes: Vec<_> = entries
        .iter()
        .zip(intrinsic.into_iter().chain(std::iter::repeat(0)))
        .map(|(entry, intrinsic)| {
            let min = entry.min_size.unwrap_or(0);
            let size = match entry.basis {
                StackBasis::Auto => intrinsic,
                StackBasis::Cells(size) => size,
            };
            size.clamp(min, entry.max_size.unwrap_or(usize::MAX).max(min))
        })
        .collect();
    let Some(available) = available else {
        return sizes;
    };
    let content = available.saturating_sub(entries.len().saturating_sub(1).saturating_mul(gap));
    let total = sizes.iter().copied().fold(0_usize, usize::saturating_add);
    let growing = total < content;
    let mut remaining = total.abs_diff(content);
    let trade = |entry: &StackLayoutEntry, size: usize| {
        if growing {
            (
                entry.grow,
                entry.max_size.unwrap_or(usize::MAX).saturating_sub(size),
            )
        } else {
            (
                entry.shrink * size.max(1) as f64,
                size.saturating_sub(entry.min_size.unwrap_or(0)),
            )
        }
    };
    while remaining > 0 {
        let weight_sum: f64 = entries
            .iter()
            .zip(&sizes)
            .map(|(entry, &size)| {
                let (weight, capacity) = trade(entry, size);
                if capacity > 0 { weight.max(0.) } else { 0. }
            })
            .sum();
        if weight_sum == 0. {
            break;
        }
        let previous = remaining;
        for (entry, size) in entries.iter().zip(&mut sizes) {
            if remaining == 0 {
                break;
            }
            let (weight, capacity) = trade(entry, *size);
            if weight <= 0. || capacity == 0 {
                continue;
            }
            let delta = ((remaining as f64 * weight / weight_sum) as usize)
                .max(1)
                .min(remaining)
                .min(capacity);
            if growing {
                *size += delta;
            } else {
                *size -= delta;
            }
            remaining -= delta;
        }
        if previous == remaining {
            break;
        }
    }
    sizes
}
