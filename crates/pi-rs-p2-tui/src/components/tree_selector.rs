use crate::components::selectors::SelectorContext;
use crate::render_context::line_text;
use pi_rs_tui::{Component, Focusable};

#[derive(Debug, Clone)]
pub struct TreeSelectorNode {
    pub entry: u32,
    pub parent: Option<u32>,
    pub conversation: u32,
    pub role: TreeRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
struct FlatNode {
    node: TreeSelectorNode,
    prefix: String,
    active: bool,
}

const VISIBLE_ROWS: usize = 16;

pub struct TreeList {
    nodes: Vec<FlatNode>,
    selected: usize,
    ctx: SelectorContext,
}

impl TreeList {
    pub fn new(
        nodes: Vec<TreeSelectorNode>,
        ctx: SelectorContext,
        initial_selected: Option<u32>,
    ) -> Self {
        let ids: std::collections::HashSet<_> = nodes.iter().map(|n| n.entry).collect();
        let mut children: std::collections::HashMap<Option<u32>, Vec<&TreeSelectorNode>> =
            std::collections::HashMap::new();
        for node in &nodes {
            children
                .entry(node.parent.filter(|p| ids.contains(p)))
                .or_default()
                .push(node);
        }
        let parents: std::collections::HashMap<_, _> =
            nodes.iter().map(|n| (n.entry, n.parent)).collect();
        let mut active = std::collections::HashSet::new();
        let mut current = initial_selected;
        while let Some(id) = current {
            if !active.insert(id) {
                break;
            }
            current = parents.get(&id).copied().flatten();
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|n| !active.contains(&n.entry));
        }
        let mut flat = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<_> = children
            .get(&None)
            .into_iter()
            .flatten()
            .rev()
            .map(|n| (*n, String::new(), None, children[&None].len() > 1))
            .collect();
        while let Some((node, mut gutter, connector, virtual_root)) = stack.pop() {
            if !seen.insert(node.entry) {
                continue;
            }
            flat.push(FlatNode {
                node: node.clone(),
                prefix: format!(
                    "{gutter}{}",
                    match connector {
                        Some(true) => "└─ ",
                        Some(false) => "├─ ",
                        None => "",
                    }
                ),
                active: active.contains(&node.entry),
            });
            if let Some(last) = connector {
                gutter.push_str(if last { "   " } else { "│  " });
            }
            if let Some(kids) = children.get(&Some(node.entry)) {
                let branches = kids.len() > 1;
                if !branches && (connector.is_some() || virtual_root) {
                    gutter.push_str("   ");
                }
                for (i, kid) in kids.iter().enumerate().rev() {
                    stack.push((
                        *kid,
                        gutter.clone(),
                        branches.then_some(i + 1 == kids.len()),
                        false,
                    ));
                }
            }
        }
        let selected = initial_selected
            .and_then(|id| flat.iter().position(|n| n.node.entry == id))
            .unwrap_or_else(|| flat.len().saturating_sub(1));
        Self {
            nodes: flat,
            selected,
            ctx,
        }
    }
}

impl Component for TreeList {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        if self.nodes.is_empty() {
            return vec![theme.fg("muted", "  No messages yet")];
        }
        let mut lines = Vec::new();

        for (i, node) in self
            .nodes
            .iter()
            .enumerate()
            .skip(
                self.selected
                    .saturating_sub(VISIBLE_ROWS / 2)
                    .min(self.nodes.len().saturating_sub(VISIBLE_ROWS)),
            )
            .take(VISIBLE_ROWS)
        {
            let prefix = if i == self.selected {
                theme.fg("accent", "› ")
            } else {
                "  ".into()
            };
            let role_marker = match node.node.role {
                TreeRole::User => theme.fg("accent", "user: "),
                TreeRole::Assistant => theme.fg("muted", "assistant: "),
            };
            let text = line_text(&node.node.text);
            let text = if i == self.selected {
                theme.bold(&text)
            } else {
                text
            };
            let marker = if node.active {
                theme.fg("accent", "• ")
            } else {
                String::new()
            };
            let line = pi_rs_tui::truncate_to_width(
                &format!(
                    "{prefix}{}{marker}{role_marker}{text}",
                    theme.fg("dim", &node.prefix)
                ),
                width,
                true,
            );
            lines.push(if i == self.selected {
                theme.bg("selectedBg", &line)
            } else {
                line
            });
        }

        lines.push(theme.fg(
            "muted",
            &format!("  ({}/{})", self.selected + 1, self.nodes.len()),
        ));
        lines
    }

    fn invalidate(&mut self) {}
}

pub struct LabelInput {
    ctx: SelectorContext,
    label: String,
    on_save: Box<dyn Fn(String) + Send + Sync>,
    on_cancel: Box<dyn Fn() + Send + Sync>,
    focused: bool,
}

impl LabelInput {
    pub fn new(
        ctx: SelectorContext,
        initial: String,
        on_save: impl Fn(String) + Send + Sync + 'static,
        on_cancel: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            ctx,
            label: initial,
            on_save: Box::new(on_save),
            on_cancel: Box::new(on_cancel),
            focused: false,
        }
    }
}

impl Component for LabelInput {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        vec![theme.fg("accent", &format!("Label: {}", self.label))]
    }

    fn handle_input(&mut self, data: &str) {
        let _keys = &self.ctx.keys;
        if data == "\n" {
            (self.on_save)(self.label.clone());
        } else if data == "\x1b" {
            (self.on_cancel)();
        } else if data == "\x7f" || data == "\x08" {
            self.label.pop();
        } else if data.chars().count() == 1 && !data.chars().any(char::is_control) {
            self.label.push_str(data);
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for LabelInput {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub struct TreeSelectorComponent {
    ctx: SelectorContext,
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    on_select: Box<dyn Fn(u32) + Send + Sync>,
    on_rename: Box<dyn Fn(String) + Send + Sync>,
    on_cancel: Box<dyn Fn() + Send + Sync>,
    focused: bool,
}

impl TreeSelectorComponent {
    pub fn new(
        ctx: SelectorContext,
        nodes: Vec<TreeSelectorNode>,
        on_select: impl Fn(u32) + Send + Sync + 'static,
        on_rename: impl Fn(String) + Send + Sync + 'static,
        on_cancel: impl Fn() + Send + Sync + 'static,
        initial_selected: Option<u32>,
    ) -> Self {
        let tree_list = TreeList::new(nodes, ctx.clone(), initial_selected);
        Self {
            ctx,
            tree_list,
            label_input: None,
            on_select: Box::new(on_select),
            on_rename: Box::new(on_rename),
            on_cancel: Box::new(on_cancel),
            focused: false,
        }
    }
}

impl Component for TreeSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.label_input
            .as_ref()
            .map_or_else(|| self.tree_list.render(width), |input| input.render(width))
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(input) = &mut self.label_input {
            if self.ctx.keys.matches(data, "tui.select.confirm") {
                (self.on_rename)(input.label.clone());
                self.label_input = None;
            } else if self.ctx.keys.matches(data, "tui.select.cancel") {
                self.label_input = None;
            } else {
                input.handle_input(data);
            }
            return;
        }
        if self.ctx.keys.matches(data, "app.tree.editLabel") {
            self.label_input = Some(LabelInput::new(
                self.ctx.clone(),
                String::new(),
                |_| {},
                || {},
            ));
            return;
        }
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            self.tree_list.selected = self
                .tree_list
                .selected
                .checked_sub(1)
                .unwrap_or_else(|| self.tree_list.nodes.len().saturating_sub(1));
        } else if keys.matches(data, "tui.select.down") {
            self.tree_list.selected =
                (self.tree_list.selected + 1) % self.tree_list.nodes.len().max(1);
        } else if keys.matches(data, "tui.select.pageUp")
            || keys.matches(data, "tui.editor.cursorLeft")
        {
            self.tree_list.selected = self.tree_list.selected.saturating_sub(VISIBLE_ROWS);
        } else if keys.matches(data, "tui.select.pageDown")
            || keys.matches(data, "tui.editor.cursorRight")
        {
            self.tree_list.selected = (self.tree_list.selected + VISIBLE_ROWS)
                .min(self.tree_list.nodes.len().saturating_sub(1));
        } else if keys.matches(data, "tui.select.confirm") {
            if let Some(row) = self.tree_list.nodes.get(self.tree_list.selected) {
                (self.on_select)(row.node.entry);
            }
        } else if keys.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for TreeSelectorComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
