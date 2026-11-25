use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use pi_rs_tui::{Component, Focusable, get_keybindings};
use serde_json::Value;

use super::{InputDialog, Picker, PickerItem};
use crate::{core::session_manager::SessionEntry, modes::interactive::theme::Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeFilter {
    #[default]
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl TreeFilter {
    const ALL: [Self; 5] = [
        Self::Default,
        Self::NoTools,
        Self::UserOnly,
        Self::LabeledOnly,
        Self::All,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }
    pub fn from_setting(setting: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == setting)
            .unwrap_or_default()
    }
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn one_line(text: &str) -> String {
    text.trim()
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub fn user_message_items(entries: &[SessionEntry]) -> Vec<PickerItem<String>> {
    user_message_rows(entries.iter())
}

pub fn user_message_rows<'a>(
    entries: impl IntoIterator<Item = &'a SessionEntry>,
) -> Vec<PickerItem<String>> {
    let mut rows: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { id, message, .. } if message["role"] == "user" => Some(
                PickerItem::new(one_line(&content_text(&message["content"])), id.clone()),
            ),
            _ => None,
        })
        .collect();
    let len = rows.len();
    for (index, row) in rows.iter_mut().enumerate() {
        row.label
            .push_str(&format!(" · Message {} of {len}", index + 1));
    }
    rows
}

#[derive(Clone, Copy)]
enum Kind {
    User,
    Assistant,
    Tool,
    Message,
    Bookkeeping,
    Usage,
}

struct Node {
    id: String,
    parent: Option<usize>,
    children: Vec<usize>,
    kind: Kind,
    visible_assistant: bool,
    display: String,
    copy: Option<String>,
    label: Option<String>,
    label_time: Option<String>,
    active: bool,
}

struct Tree {
    nodes: Vec<Node>,
    order: Vec<usize>,
    active: Option<usize>,
}

impl Tree {
    fn new(entries: &[SessionEntry], active_id: Option<&str>) -> Self {
        let mut tool_calls = HashMap::new();
        for entry in entries {
            if let SessionEntry::Message { message, .. } = entry
                && message["role"] == "assistant"
            {
                for block in message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "toolCall")
                {
                    if let Some(id) = block["id"].as_str() {
                        tool_calls.insert(id, (&block["name"], &block["arguments"]));
                    }
                }
            }
        }
        let mut nodes: Vec<_> = entries
            .iter()
            .map(|entry| Node::new(entry, &tool_calls))
            .collect();
        let by_id: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id(), index))
            .collect();
        let active = active_id.and_then(|id| by_id.get(id).copied());
        let mut roots = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if let SessionEntry::Label {
                target_id,
                label,
                timestamp,
                ..
            } = entry
                && let Some(&target) = by_id.get(target_id.as_str())
            {
                nodes[target].label.clone_from(label);
                nodes[target].label_time = label.as_ref().map(|_| timestamp.clone());
            }
            match entry
                .parent_id()
                .and_then(|id| by_id.get(id).copied())
                .filter(|&parent| parent != index)
            {
                Some(parent) => {
                    nodes[parent].children.push(index);
                    nodes[index].parent = Some(parent);
                }
                None => roots.push(index),
            }
        }
        let mut current = active;
        while let Some(index) = current {
            if std::mem::replace(&mut nodes[index].active, true) {
                break;
            }
            current = nodes[index].parent;
        }
        roots.sort_by_key(|&index| !nodes[index].active);
        let active_nodes: Vec<_> = nodes.iter().map(|node| node.active).collect();
        for node in &mut nodes {
            node.children.sort_by_key(|&index| !active_nodes[index]);
        }
        let mut order = Vec::with_capacity(nodes.len());
        let mut seen = vec![false; nodes.len()];
        let mut stack = Vec::new();
        for root in roots.into_iter().chain(0..nodes.len()) {
            if seen[root] {
                continue;
            }
            nodes[root].parent = None;
            stack.push(root);
            while let Some(index) = stack.pop() {
                if std::mem::replace(&mut seen[index], true) {
                    continue;
                }
                order.push(index);
                nodes[index].children.retain(|&child| !seen[child]);
                stack.extend(nodes[index].children.iter().rev().copied());
            }
        }
        Self {
            nodes,
            order,
            active,
        }
    }

    fn nearest(&self, mut index: usize, visible: &[usize]) -> Option<usize> {
        loop {
            if visible.contains(&index) {
                return Some(index);
            }
            index = self.nodes[index].parent?;
        }
    }

    fn visible(&self, mode: TreeFilter, query: &str, folded: &HashSet<usize>) -> VisibleTree {
        let query = query.to_lowercase();
        let mut children = vec![Vec::new(); self.nodes.len()];
        let mut parent = vec![None; self.nodes.len()];
        let mut roots = Vec::new();
        let mut order = Vec::new();
        let mut nearest = vec![None; self.nodes.len()];
        let mut hidden = vec![false; self.nodes.len()];
        for &index in &self.order {
            let node = &self.nodes[index];
            let parent_index = node.parent;
            if parent_index.is_some_and(|parent| hidden[parent] || folded.contains(&parent)) {
                hidden[index] = true;
                continue;
            }
            let nearest_parent = parent_index.and_then(|parent| nearest[parent]);
            nearest[index] = nearest_parent;
            if !node.matches(mode, &query, self.active == Some(index)) {
                continue;
            }
            nearest[index] = Some(index);
            parent[index] = nearest_parent;
            order.push(index);
            match nearest_parent {
                Some(parent) => children[parent].push(index),
                None => roots.push(index),
            }
        }
        VisibleTree {
            children,
            parent,
            roots,
            order,
        }
    }
}

struct VisibleTree {
    children: Vec<Vec<usize>>,
    parent: Vec<Option<usize>>,
    roots: Vec<usize>,
    order: Vec<usize>,
}

impl VisibleTree {
    fn foldable(&self, index: usize) -> bool {
        !self.children[index].is_empty()
            && self.parent[index].is_none_or(|parent| self.children[parent].len() > 1)
    }

    fn labels(&self, tree: &Tree, folded: &HashSet<usize>, timestamps: bool) -> Vec<String> {
        let mut labels = vec![String::new(); tree.nodes.len()];
        let multiple_roots = self.roots.len() > 1;
        let mut stack: Vec<_> = self
            .roots
            .iter()
            .rev()
            .map(|&root| {
                (
                    root,
                    0usize,
                    multiple_roots,
                    None,
                    Vec::<(usize, bool)>::new(),
                )
            })
            .collect();
        while let Some((index, indent, just_branched, connector, gutters)) = stack.pop() {
            let node = &tree.nodes[index];
            let is_folded = folded.contains(&index);
            let mut prefix = String::new();
            for col in 0..indent {
                if let Some((_, continued)) = gutters.iter().find(|&&(pos, _)| pos == col) {
                    prefix.push_str(if *continued { "│  " } else { "   " });
                } else if let Some(last) = connector.filter(|_| col + 1 == indent) {
                    prefix.push(if last { '└' } else { '├' });
                    prefix.push(if is_folded {
                        '⊞'
                    } else if self.foldable(index) {
                        '⊟'
                    } else {
                        '─'
                    });
                    prefix.push(' ');
                } else {
                    prefix.push_str("   ");
                }
            }
            if connector.is_none() && is_folded {
                prefix.push_str("⊞ ");
            }
            if node.active {
                prefix.push_str("• ");
            }
            if let Some(label) = &node.label {
                prefix.push_str(&format!("[{}] ", one_line(label)));
                if timestamps && let Some(time) = &node.label_time {
                    prefix.push_str(&format!("{time} "));
                }
            }
            prefix.push_str(&node.display);
            labels[index] = prefix;
            let children = &self.children[index];
            let branches = children.len() > 1;
            let increment = branches || just_branched;
            let mut gutters = gutters;
            if let Some(last) = connector {
                gutters.push((indent.saturating_sub(1), !last));
            }
            for (pos, &child) in children.iter().enumerate().rev() {
                stack.push((
                    child,
                    indent + usize::from(increment),
                    branches,
                    branches.then_some(pos + 1 == children.len()),
                    gutters.clone(),
                ));
            }
        }
        labels
    }
}

impl Node {
    fn new(entry: &SessionEntry, tools: &HashMap<&str, (&Value, &Value)>) -> Self {
        let mut visible_assistant = true;
        let (kind, display, copy) = match entry {
            SessionEntry::Message { message, .. } => {
                let role = message["role"].as_str().unwrap_or_default();
                let content = content_text(&message["content"]);
                match role {
                    "user" => (
                        Kind::User,
                        format!("user: {}", one_line(&content)),
                        Some(content),
                    ),
                    "assistant" => {
                        let stop = message["stopReason"].as_str();
                        let error = message["errorMessage"].as_str();
                        visible_assistant = !content.trim().is_empty()
                            || stop.is_some_and(|stop| !matches!(stop, "stop" | "toolUse"));
                        let text = if !content.trim().is_empty() {
                            one_line(&content)
                        } else if stop == Some("aborted") {
                            "(aborted)".into()
                        } else {
                            error.map_or_else(|| "(no content)".into(), one_line)
                        };
                        let copy = if content.trim().is_empty() {
                            error.map(str::to_owned)
                        } else {
                            Some(content)
                        };
                        (Kind::Assistant, format!("assistant: {text}"), copy)
                    }
                    "toolResult" | "tool_result" => {
                        let fallback = message["toolName"].as_str().unwrap_or("tool");
                        let label = message["toolCallId"]
                            .as_str()
                            .and_then(|id| tools.get(id))
                            .map_or_else(
                                || format!("[{fallback}]"),
                                |(name, arguments)| {
                                    tool_label(name.as_str().unwrap_or(fallback), arguments)
                                },
                            );
                        (Kind::Tool, label, Some(content))
                    }
                    "bashExecution" => {
                        let command = message["command"].as_str().unwrap_or_default();
                        (
                            Kind::Message,
                            format!("[bash]: {}", one_line(command)),
                            Some(command.to_owned()),
                        )
                    }
                    unknown => (Kind::Message, format!("[{unknown}]"), Some(content)),
                }
            }
            SessionEntry::CustomMessage {
                custom_type,
                content,
                ..
            } => {
                let text = content_text(content);
                (
                    Kind::Message,
                    format!("[{custom_type}]: {}", one_line(&text)),
                    Some(text),
                )
            }
            SessionEntry::Compaction {
                tokens_before,
                summary,
                ..
            } => (
                Kind::Message,
                format!(
                    "[compaction: {}k tokens]",
                    tokens_before.saturating_add(500) / 1000
                ),
                Some(summary.clone()),
            ),
            SessionEntry::BranchSummary { summary, .. } => (
                Kind::Message,
                format!("[branch summary]: {}", one_line(summary)),
                Some(summary.clone()),
            ),
            SessionEntry::ModelChange { model_id, .. } => {
                (Kind::Bookkeeping, format!("[model: {model_id}]"), None)
            }
            SessionEntry::ThinkingLevelChange { thinking_level, .. } => (
                Kind::Bookkeeping,
                format!("[thinking: {thinking_level}]"),
                None,
            ),
            SessionEntry::Custom { custom_type, .. } => {
                (Kind::Bookkeeping, format!("[custom: {custom_type}]"), None)
            }
            SessionEntry::ContextEdit {
                target_id,
                replacement,
                ..
            } => (
                Kind::Bookkeeping,
                format!(
                    "[context {}: {target_id}]",
                    if replacement.is_null() {
                        "omit"
                    } else {
                        "replace"
                    }
                ),
                None,
            ),
            SessionEntry::Label { label, .. } => (
                Kind::Bookkeeping,
                format!("[label: {}]", label.as_deref().unwrap_or("(cleared)")),
                None,
            ),
            SessionEntry::SessionInfoEntry { name, .. } => (
                Kind::Bookkeeping,
                format!("[title: {}]", name.as_deref().unwrap_or("empty")),
                None,
            ),
            SessionEntry::Usage { .. } => (Kind::Usage, String::new(), None),
        };
        Self {
            id: entry.id().to_owned(),
            parent: None,
            children: Vec::new(),
            kind,
            visible_assistant,
            display,
            copy: copy.filter(|text| !text.trim().is_empty()),
            label: None,
            label_time: None,
            active: false,
        }
    }

    fn matches(&self, mode: TreeFilter, query: &str, is_leaf: bool) -> bool {
        if matches!(self.kind, Kind::Usage) || (!is_leaf && !self.visible_assistant) {
            return false;
        }
        let allowed = match mode {
            TreeFilter::Default => !matches!(self.kind, Kind::Bookkeeping),
            TreeFilter::NoTools => !matches!(self.kind, Kind::Bookkeeping | Kind::Tool),
            TreeFilter::UserOnly => matches!(self.kind, Kind::User),
            TreeFilter::LabeledOnly => self.label.is_some(),
            TreeFilter::All => true,
        };
        if !allowed || query.trim().is_empty() {
            return allowed;
        }
        let display = self.display.to_lowercase();
        let label = self.label.as_deref().map(str::to_lowercase);
        query.split_whitespace().all(|token| {
            display.contains(token) || label.as_deref().is_some_and(|label| label.contains(token))
        })
    }
}

fn tool_label(name: &str, args: &Value) -> String {
    let path = args["path"].as_str().or_else(|| args["file_path"].as_str());
    let pattern = args["pattern"].as_str().unwrap_or_default();
    match name {
        "read" | "write" | "edit" => format!("[{name}: {}]", path.unwrap_or_default()),
        "bash" => format!(
            "[bash: {}]",
            one_line(args["command"].as_str().unwrap_or_default())
                .chars()
                .take(50)
                .collect::<String>()
        ),
        "grep" => format!("[grep: /{pattern}/ in {}]", path.unwrap_or(".")),
        "find" => format!("[find: {pattern} in {}]", path.unwrap_or(".")),
        "ls" => format!("[ls: {}]", path.unwrap_or(".")),
        _ => format!(
            "[{name}: {}]",
            args.to_string().chars().take(40).collect::<String>()
        ),
    }
}

pub fn tree_items(entries: &[SessionEntry], active_id: Option<&str>) -> Vec<PickerItem<String>> {
    let tree = Tree::new(entries, active_id);
    let folded = HashSet::new();
    let visible = tree.visible(TreeFilter::Default, "", &folded);
    let mut labels = visible.labels(&tree, &folded, false);
    visible
        .order
        .into_iter()
        .map(|index| {
            PickerItem::new(
                std::mem::take(&mut labels[index]),
                tree.nodes[index].id.clone(),
            )
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum TreeAction {
    Select(String),
    Label(String, Option<String>),
    Copy(String),
    Cancel,
}

type LabelResult = Rc<RefCell<Option<Option<String>>>>;

pub struct TreePicker {
    picker: Picker,
    theme: Theme,
    tree: Tree,
    visible: VisibleTree,
    filter: TreeFilter,
    folded: HashSet<usize>,
    timestamps: bool,
    label: Option<(usize, InputDialog)>,
    label_result: LabelResult,
    action: Box<dyn FnMut(TreeAction)>,
}

impl TreePicker {
    pub fn new(
        entries: &[SessionEntry],
        active_id: Option<&str>,
        filter: TreeFilter,
        theme: Theme,
        action: impl FnMut(TreeAction) + 'static,
    ) -> Self {
        let tree = Tree::new(entries, active_id);
        let folded = HashSet::new();
        let visible = tree.visible(filter, "", &folded);
        let mut dialog = Self {
            picker: Picker::new("Session Tree".into(), Vec::new(), theme.clone(), |_| {})
                .hint("↑↓ move · ←→ page · Alt+↑↓ branch · Ctrl+L label · Ctrl+C copy · Esc back"),
            theme,
            tree,
            visible,
            filter,
            folded,
            timestamps: false,
            label: None,
            label_result: Rc::new(RefCell::new(None)),
            action: Box::new(action),
        };
        dialog.refresh(dialog.tree.active);
        dialog
    }

    pub fn max_visible(mut self, count: usize) -> Self {
        self.picker = self.picker.max_visible(count);
        self
    }

    fn refresh(&mut self, keep: Option<usize>) {
        self.visible = self
            .tree
            .visible(self.filter, self.picker.query(), &self.folded);
        let labels = self
            .visible
            .labels(&self.tree, &self.folded, self.timestamps);
        let keep = keep
            .and_then(|index| self.tree.nearest(index, &self.visible.order))
            .or_else(|| self.visible.order.last().copied());
        let order = self.visible.order.clone();
        self.picker
            .replace_rows(labels, Box::new(move |_| order.clone()), keep);
        self.picker.description = format!(
            "Filter: {} · Alt+1–5 filter · Tab cycle",
            self.filter.as_str()
        );
    }

    fn navigate_branch(&mut self, mut index: usize, up: bool) {
        if up && self.visible.foldable(index) && self.folded.insert(index) {
            self.refresh(Some(index));
            return;
        }
        if !up && self.folded.remove(&index) {
            self.refresh(Some(index));
            return;
        }
        if up {
            let initial = index;
            while let Some(parent) = self.visible.parent[index] {
                if index != initial && self.visible.children[parent].len() > 1 {
                    break;
                }
                index = parent;
            }
        } else {
            while let Some(&child) = self.visible.children[index].first() {
                let branched = self.visible.children[index].len() > 1;
                index = child;
                if branched {
                    break;
                }
            }
        }
        self.picker.select_index(index);
    }
}

impl Component for TreePicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.label.as_ref().map_or_else(
            || self.picker.render(width),
            |(_, input)| input.render(width),
        )
    }

    fn handle_input(&mut self, data: &str) {
        if let Some((index, input)) = &mut self.label {
            input.handle_input(data);
            let result = self.label_result.borrow_mut().take();
            if let Some(result) = result {
                let index = *index;
                self.label = None;
                if let Some(value) = result {
                    let value = value.trim();
                    let label = (!value.is_empty()).then(|| value.to_owned());
                    let node = &mut self.tree.nodes[index];
                    node.label.clone_from(&label);
                    node.label_time = label.as_ref().map(|_| chrono::Local::now().to_rfc3339());
                    (self.action)(TreeAction::Label(node.id.clone(), label));
                    self.refresh(Some(index));
                }
            }
            return;
        }
        let keys = get_keybindings();
        let is = |name: &str| keys.matches(data, name);
        let selected = self.picker.selected_index();
        let direct = ["default", "noTools", "userOnly", "labeledOnly", "all"]
            .into_iter()
            .zip(TreeFilter::ALL)
            .find_map(|(key, mode)| is(&format!("app.tree.filter.{key}")).then_some(mode));
        if let Some(mode) = direct {
            self.filter = if self.filter == mode {
                TreeFilter::Default
            } else {
                mode
            };
            self.folded.clear();
            self.refresh(selected);
        } else if is("app.tree.filter.cycleForward") || is("app.tree.filter.cycleBackward") {
            let index = TreeFilter::ALL
                .iter()
                .position(|&mode| mode == self.filter)
                .unwrap_or_default();
            self.filter = TreeFilter::ALL[(index
                + if is("app.tree.filter.cycleForward") {
                    1
                } else {
                    4
                })
                % 5];
            self.folded.clear();
            self.refresh(selected);
        } else if is("app.tree.foldOrUp") || is("app.tree.unfoldOrDown") {
            if let Some(index) = selected {
                self.navigate_branch(index, is("app.tree.foldOrUp"));
            }
        } else if is("tui.editor.cursorLeft") || is("tui.editor.cursorRight") {
            let position = if is("tui.editor.cursorLeft") {
                self.picker.selected.saturating_sub(self.picker.visible)
            } else {
                (self.picker.selected + self.picker.visible)
                    .min(self.picker.order.len().saturating_sub(1))
            };
            self.picker.set_position(position);
        } else if is("tui.select.confirm") || data == "\n" {
            if let Some(index) = selected {
                (self.action)(TreeAction::Select(self.tree.nodes[index].id.clone()));
            }
        } else if is("app.message.copy") {
            if let Some(text) = selected.and_then(|index| self.tree.nodes[index].copy.clone()) {
                (self.action)(TreeAction::Copy(text));
            }
        } else if is("app.tree.editLabel") {
            if let Some(index) = selected {
                let result = Rc::clone(&self.label_result);
                let mut input = InputDialog::new(
                    "Label (empty to remove)".into(),
                    None,
                    self.theme.clone(),
                    move |value| *result.borrow_mut() = Some(value),
                )
                .initial_value(self.tree.nodes[index].label.clone().unwrap_or_default());
                input.set_focused(self.picker.is_focused());
                self.label = Some((index, input));
            }
        } else if is("app.tree.toggleLabelTimestamp") {
            self.timestamps = !self.timestamps;
            self.refresh(selected);
        } else if is("tui.select.cancel") {
            if self.picker.query().is_empty() {
                (self.action)(TreeAction::Cancel);
            } else {
                self.picker.search.set_value(String::new());
                self.folded.clear();
                self.refresh(selected);
            }
        } else {
            let previous = self.picker.query().to_owned();
            self.picker.handle_input(data);
            if previous != self.picker.query() {
                self.folded.clear();
                self.refresh(selected);
            }
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
    fn captures_viewport_input(&self) -> bool {
        self.label.is_none()
    }
}

impl Focusable for TreePicker {
    fn is_focused(&self) -> bool {
        self.picker.is_focused()
    }
    fn set_focused(&mut self, focused: bool) {
        self.picker.set_focused(focused);
        if let Some((_, input)) = &mut self.label {
            input.set_focused(focused);
        }
    }
}
