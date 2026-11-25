//! Pure pane layout tree and arrangement validation (`apps/p2/plugins/views/layout.ts`).

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CHAT: &str = "transcript";
const MIN_SHARE: f64 = 0.1;
const RECENT: u64 = 60_000;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_id(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{n}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Row,
    Column,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutNode {
    Area {
        id: String,
        views: Vec<String>,
        active: String,
    },
    Split {
        id: String,
        dir: SplitDir,
        children: Vec<LayoutNode>,
        sizes: Vec<f64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub tree: LayoutNode,
    pub hidden: Vec<String>,
    pub focus: HashMap<String, u64>,
    pub phone: String,
    pub arranged: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

fn area(views: Vec<String>, active: Option<String>) -> LayoutNode {
    let active = active.unwrap_or_else(|| views.first().cloned().unwrap_or_else(|| CHAT.into()));
    LayoutNode::Area {
        id: new_id("a"),
        views,
        active,
    }
}

fn split(dir: SplitDir, children: Vec<LayoutNode>, sizes: Vec<f64>) -> LayoutNode {
    LayoutNode::Split {
        id: new_id("s"),
        dir,
        children,
        sizes,
    }
}

pub fn initial_layout() -> Layout {
    Layout {
        tree: area(vec![CHAT.into()], None),
        hidden: Vec::new(),
        focus: HashMap::new(),
        phone: CHAT.into(),
        arranged: 0,
    }
}

pub fn views_in(node: &LayoutNode) -> Vec<String> {
    match node {
        LayoutNode::Area { views, .. } => views.clone(),
        LayoutNode::Split { children, .. } => children.iter().flat_map(views_in).collect(),
    }
}

pub fn areas_of(node: &LayoutNode) -> Vec<(String, Vec<String>, String)> {
    match node {
        LayoutNode::Area { id, views, active } => vec![(id.clone(), views.clone(), active.clone())],
        LayoutNode::Split { children, .. } => children.iter().flat_map(areas_of).collect(),
    }
}

fn map_area(
    node: LayoutNode,
    target_id: &str,
    f: &impl Fn(String, Vec<String>, String) -> LayoutNode,
) -> LayoutNode {
    match node {
        LayoutNode::Area { id, views, active } => {
            if id == target_id {
                f(id, views, active)
            } else {
                LayoutNode::Area { id, views, active }
            }
        }
        LayoutNode::Split {
            id,
            dir,
            children,
            sizes,
        } => LayoutNode::Split {
            id,
            dir,
            children: children
                .into_iter()
                .map(|c| map_area(c, target_id, f))
                .collect(),
            sizes,
        },
    }
}

fn normalize(node: LayoutNode) -> LayoutNode {
    let LayoutNode::Split {
        id,
        dir,
        children,
        sizes,
    } = node
    else {
        return node;
    };
    let mut next_children = Vec::new();
    let mut next_sizes = Vec::new();
    for (idx, child) in children.into_iter().enumerate() {
        let inner = normalize(child);
        let share = sizes.get(idx).copied().unwrap_or(0.0);
        if let LayoutNode::Split {
            dir: inner_dir,
            children: grand,
            sizes: inner_sizes,
            ..
        } = &inner
            && *inner_dir == dir
        {
            let total: f64 = inner_sizes.iter().sum();
            let total = if total == 0.0 { 1.0 } else { total };
            for (at, g) in grand.iter().enumerate() {
                next_children.push(g.clone());
                next_sizes.push((share * inner_sizes.get(at).copied().unwrap_or(0.0)) / total);
            }
        } else {
            next_children.push(inner);
            next_sizes.push(share);
        }
    }
    if next_children.len() == 1 {
        return next_children.remove(0);
    }
    let total: f64 = next_sizes.iter().sum();
    let len = next_sizes.len().max(1) as f64;
    let normalized_sizes = if total > 0.0 {
        next_sizes.into_iter().map(|s| s / total).collect()
    } else {
        vec![1.0 / len; next_sizes.len()]
    };
    LayoutNode::Split {
        id,
        dir,
        children: next_children,
        sizes: normalized_sizes,
    }
}

pub fn without(tree: LayoutNode, view_id: &str) -> Option<LayoutNode> {
    match tree {
        LayoutNode::Area { id, views, active } => {
            if !views.iter().any(|v| v == view_id) {
                return Some(LayoutNode::Area { id, views, active });
            }
            let idx = views.iter().position(|v| v == view_id).unwrap_or(0);
            let remaining: Vec<String> = views.into_iter().filter(|v| v != view_id).collect();
            if remaining.is_empty() {
                return None;
            }
            let next_active = if active == view_id {
                remaining
                    .get(idx.min(remaining.len() - 1))
                    .or_else(|| remaining.first())
                    .cloned()
                    .unwrap_or_else(|| CHAT.into())
            } else {
                active
            };
            Some(LayoutNode::Area {
                id,
                views: remaining,
                active: next_active,
            })
        }
        LayoutNode::Split {
            id,
            dir,
            children,
            sizes,
        } => {
            let mut kept = Vec::new();
            let mut kept_sizes = Vec::new();
            for (idx, child) in children.into_iter().enumerate() {
                if let Some(next) = without(child, view_id) {
                    kept.push(next);
                    kept_sizes.push(sizes.get(idx).copied().unwrap_or(0.0));
                }
            }
            if kept.is_empty() {
                None
            } else {
                Some(normalize(LayoutNode::Split {
                    id,
                    dir,
                    children: kept,
                    sizes: kept_sizes,
                }))
            }
        }
    }
}

pub fn place(tree: LayoutNode, view_id: &str) -> LayoutNode {
    let target = areas_of(&tree)
        .into_iter()
        .rfind(|(_, views, _)| !views.iter().any(|v| v == CHAT))
        .map(|(id, _, _)| id);
    if let Some(target_id) = target {
        let vid = view_id.to_string();
        return map_area(tree, &target_id, &|id, mut views, _| {
            views.push(vid.clone());
            LayoutNode::Area {
                id,
                views,
                active: vid.clone(),
            }
        });
    }
    normalize(split(
        SplitDir::Row,
        vec![tree, area(vec![view_id.into()], None)],
        vec![0.5, 0.5],
    ))
}

pub fn activate(tree: LayoutNode, view_id: &str) -> LayoutNode {
    let target = areas_of(&tree)
        .into_iter()
        .find(|(_, views, _)| views.iter().any(|v| v == view_id))
        .map(|(id, _, _)| id);
    let Some(target_id) = target else {
        return tree;
    };
    let vid = view_id.to_string();
    map_area(tree, &target_id, &|id, views, _| LayoutNode::Area {
        id,
        views,
        active: vid.clone(),
    })
}

pub fn resize(tree: LayoutNode, split_id: &str, index: usize, share: f64) -> LayoutNode {
    match tree {
        LayoutNode::Area { .. } => tree,
        LayoutNode::Split {
            id,
            dir,
            children,
            mut sizes,
        } => {
            if id == split_id && index + 1 < sizes.len() {
                let before: f64 = sizes[..index].iter().sum();
                let pair = sizes[index] + sizes[index + 1];
                let left = (share - before).clamp(MIN_SHARE * pair, pair - MIN_SHARE * pair);
                sizes[index] = left;
                sizes[index + 1] = pair - left;
                LayoutNode::Split {
                    id,
                    dir,
                    children,
                    sizes,
                }
            } else {
                LayoutNode::Split {
                    id,
                    dir,
                    children: children
                        .into_iter()
                        .map(|c| resize(c, split_id, index, share))
                        .collect(),
                    sizes,
                }
            }
        }
    }
}

pub fn reconcile(layout: Layout, views: &[(String, u64)], now: u64) -> Layout {
    let ids: HashSet<&str> = views.iter().map(|(id, _)| id.as_str()).collect();
    let mut tree = Some(layout.tree);
    if let Some(current) = &tree {
        for id in views_in(current) {
            if id != CHAT && !ids.contains(id.as_str()) {
                tree = tree.and_then(|t| without(t, &id));
            }
        }
    }
    let mut tree = tree.unwrap_or_else(|| area(vec![CHAT.into()], None));
    if !views_in(&tree).iter().any(|v| v == CHAT) {
        tree = normalize(split(
            SplitDir::Row,
            vec![area(vec![CHAT.into()], None), tree],
            vec![0.5, 0.5],
        ));
    }
    let mut hidden: Vec<String> = layout
        .hidden
        .into_iter()
        .filter(|id| ids.contains(id.as_str()))
        .collect();
    let mut phone = layout.phone;
    let mut focus = HashMap::new();
    for (id, view_focus) in views {
        focus.insert(id.clone(), *view_focus);
        let seen = layout.focus.get(id).copied();
        if seen.is_some_and(|s| *view_focus <= s) {
            continue;
        }
        hidden.retain(|h| h != id);
        tree = if views_in(&tree).iter().any(|v| v == id) {
            activate(tree, id)
        } else {
            place(tree, id)
        };
        if seen.is_some() || now.saturating_sub(*view_focus) < RECENT {
            phone = id.clone();
        }
    }
    if phone != CHAT && (!ids.contains(phone.as_str()) || hidden.contains(&phone)) {
        phone = CHAT.into();
    }
    Layout {
        tree,
        hidden,
        focus,
        phone,
        arranged: layout.arranged,
    }
}

/// Validates an agent's arrangement (`view_arrange`). Returns `None` if valid, or `Some(reason)`.
pub fn arrangement_error(value: &Value, known: &HashSet<String>) -> Option<String> {
    fn visit(
        node: &Value,
        depth: usize,
        known: &HashSet<String>,
        seen: &mut HashSet<String>,
    ) -> Option<String> {
        if depth > 8 {
            return Some("nested too deep".into());
        }
        if let Some(id) = node.as_str() {
            if !known.contains(id) {
                return Some(format!("no view \"{id}\""));
            }
            if !seen.insert(id.to_string()) {
                return Some(format!("\"{id}\" appears twice"));
            }
            return None;
        }
        let Some(obj) = node.as_object() else {
            return Some("a node is a view ID, { tabs }, { row } or { column }".into());
        };
        if let Some(tabs_val) = obj.get("tabs")
            && let Some(tabs) = tabs_val.as_array()
        {
            if tabs.is_empty() {
                return Some("empty tabs".into());
            }
            for id in tabs {
                if let Some(s) = id.as_str() {
                    if let Some(err) = visit(&Value::String(s.to_string()), depth + 1, known, seen)
                    {
                        return Some(err);
                    }
                } else {
                    return Some("tabs hold view IDs".into());
                }
            }
            if let Some(active) = obj.get("active")
                && !tabs.iter().any(|t| t == active)
            {
                return Some("active is not one of its tabs".into());
            }
            return None;
        }
        let children = obj
            .get("row")
            .and_then(Value::as_array)
            .or_else(|| obj.get("column").and_then(Value::as_array));
        let Some(children) = children else {
            return Some(
                "a node is a view ID, { tabs }, { row } or { column } with children".into(),
            );
        };
        if children.is_empty() {
            return Some(
                "a node is a view ID, { tabs }, { row } or { column } with children".into(),
            );
        }
        if let Some(sizes_val) = obj.get("sizes") {
            let Some(sizes) = sizes_val.as_array() else {
                return Some("sizes: one positive number per child".into());
            };
            if sizes.len() != children.len()
                || !sizes
                    .iter()
                    .all(|s| s.as_f64().is_some_and(|n| n > 0.0 && n.is_finite()))
            {
                return Some("sizes: one positive number per child".into());
            }
        }
        for child in children {
            if let Some(err) = visit(child, depth + 1, known, seen) {
                return Some(err);
            }
        }
        None
    }

    let mut seen = HashSet::new();
    visit(value, 0, known, &mut seen)
}

pub fn arranged_tree(value: &Value, known: &HashSet<String>) -> Option<LayoutNode> {
    fn build(node: &Value, depth: usize, known: &HashSet<String>) -> Option<LayoutNode> {
        if depth > 8 {
            return None;
        }
        if let Some(id) = node.as_str() {
            return known.contains(id).then(|| area(vec![id.to_string()], None));
        }
        let obj = node.as_object()?;
        if let Some(tabs) = obj.get("tabs").and_then(Value::as_array) {
            let views: Vec<String> = tabs
                .iter()
                .filter_map(|t| t.as_str())
                .filter(|id| known.contains(*id))
                .map(str::to_string)
                .collect();
            if views.is_empty() {
                return None;
            }
            let active = obj
                .get("active")
                .and_then(Value::as_str)
                .filter(|a| views.iter().any(|v| v == *a))
                .map(str::to_string)
                .unwrap_or_else(|| views[0].clone());
            return Some(area(views, Some(active)));
        }
        let (dir, list) = if let Some(row) = obj.get("row").and_then(Value::as_array) {
            (SplitDir::Row, row)
        } else {
            let col = obj.get("column").and_then(Value::as_array)?;
            (SplitDir::Column, col)
        };
        let sizes = obj
            .get("sizes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut children = Vec::new();
        let mut shares = Vec::new();
        for (idx, child) in list.iter().enumerate() {
            if let Some(built) = build(child, depth + 1, known) {
                children.push(built);
                let share = sizes
                    .get(idx)
                    .and_then(Value::as_f64)
                    .filter(|&n| n > 0.0)
                    .unwrap_or(1.0);
                shares.push(share);
            }
        }
        if children.is_empty() {
            None
        } else {
            Some(normalize(split(dir, children, shares)))
        }
    }

    let tree = build(value, 0, known)?;
    if views_in(&tree).iter().any(|v| v == CHAT) {
        Some(tree)
    } else {
        Some(normalize(split(
            SplitDir::Row,
            vec![area(vec![CHAT.into()], None), tree],
            vec![0.4, 0.6],
        )))
    }
}

fn named_views(value: &Value, depth: usize) -> Vec<String> {
    if depth > 8 {
        return Vec::new();
    }
    if let Some(s) = value.as_str() {
        return vec![s.to_string()];
    }
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let children = obj
        .get("tabs")
        .or_else(|| obj.get("row"))
        .or_else(|| obj.get("column"))
        .and_then(Value::as_array);
    children
        .map(|arr| arr.iter().flat_map(|c| named_views(c, depth + 1)).collect())
        .unwrap_or_default()
}

pub fn adopt(layout: Layout, arrangement_layout: &Value, stamp: u64, views: &[String]) -> Layout {
    let mut known: HashSet<String> = views.iter().cloned().collect();
    known.insert(CHAT.into());
    let tree = arranged_tree(arrangement_layout, &known);
    let named: Vec<String> = named_views(arrangement_layout, 0)
        .into_iter()
        .filter(|id| id != CHAT)
        .collect();
    let Some(tree) = tree else {
        return Layout {
            arranged: stamp,
            ..layout
        };
    };
    if !named.is_empty() && !named.iter().any(|id| known.contains(id)) {
        return Layout {
            arranged: stamp,
            ..layout
        };
    }
    let placed: HashSet<String> = views_in(&tree).into_iter().collect();
    let phone = if placed.contains(&layout.phone) {
        layout.phone
    } else {
        CHAT.into()
    };
    Layout {
        tree,
        hidden: views
            .iter()
            .filter(|id| !placed.contains(*id))
            .cloned()
            .collect(),
        focus: layout.focus,
        phone,
        arranged: stamp,
    }
}
