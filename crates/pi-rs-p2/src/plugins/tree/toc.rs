//! Pure table-of-contents builder for `/tree` (`apps/p2/plugins/tree/toc.ts`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::contract::{Tree, TreeNode, TreeRole};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tip {
    pub conversation: u64,
    pub label: Option<String>,
    pub main: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TurnMessages {
    Exchange {
        user: TreeNode,
        answer: Option<TreeNode>,
    },
    Answer {
        answer: TreeNode,
    },
}

impl TurnMessages {
    pub fn last(&self) -> &TreeNode {
        match self {
            Self::Exchange { user, answer } => answer.as_ref().unwrap_or(user),
            Self::Answer { answer } => answer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub messages: TurnMessages,
    pub current: bool,
    pub head: bool,
    pub tips: Vec<Tip>,
    pub branches: Vec<Chain>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chain {
    pub start: u64,
    pub conversation: u64,
    pub turns: Vec<Turn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Toc {
    pub turns: Vec<Turn>,
    pub detached: Vec<Chain>,
}

fn current_path(tree: &Tree) -> HashSet<u64> {
    let parents: HashMap<u64, Option<u64>> =
        tree.nodes.iter().map(|n| (n.entry, n.parent)).collect();
    let mut path = HashSet::new();
    let mut at = tree.current;
    while let Some(id) = at {
        if !path.insert(id) {
            break;
        }
        at = parents.get(&id).copied().flatten();
    }
    path
}

fn tips_of(tree: &Tree) -> HashMap<u64, Vec<Tip>> {
    let mut last = HashMap::new();
    for node in &tree.nodes {
        last.insert(node.conversation, node.entry);
    }
    let mut tips: HashMap<u64, Vec<Tip>> = HashMap::new();
    for branch in &tree.branches {
        let main = branch.conversation == tree.main;
        let at = last
            .get(&branch.conversation)
            .copied()
            .or(if main { tree.current } else { None });
        let Some(at) = at else {
            continue;
        };
        let tip = Tip {
            conversation: branch.conversation,
            label: branch.label.clone(),
            main,
        };
        let list = tips.entry(at).or_default();
        if main {
            list.insert(0, tip);
        } else {
            list.push(tip);
        }
    }
    tips
}

fn without_steps(tree: &Tree, tips: &HashMap<u64, Vec<Tip>>) -> Vec<TreeNode> {
    let mut children: HashMap<u64, Vec<&TreeNode>> = HashMap::new();
    for node in &tree.nodes {
        if let Some(parent) = node.parent {
            children.entry(parent).or_default().push(node);
        }
    }
    let mut hidden: HashMap<u64, Option<u64>> = HashMap::new();
    for node in &tree.nodes {
        let next = children.get(&node.entry);
        if node.role == TreeRole::Assistant
            && Some(node.entry) != tree.current
            && !tips.contains_key(&node.entry)
            && let Some(list) = next
            && list.len() == 1
            && list[0].role == TreeRole::Assistant
        {
            hidden.insert(node.entry, node.parent);
        }
    }
    let shown_parent = |mut at: Option<u64>| -> Option<u64> {
        while let Some(p) = at {
            if let Some(&next_p) = hidden.get(&p) {
                at = next_p;
            } else {
                break;
            }
        }
        at
    };
    tree.nodes
        .iter()
        .filter(|n| !hidden.contains_key(&n.entry))
        .map(|n| TreeNode {
            parent: shown_parent(n.parent),
            ..n.clone()
        })
        .collect()
}

fn turn_of(node: &TreeNode, next: &[&TreeNode], tips: &HashMap<u64, Vec<Tip>>) -> TurnMessages {
    if node.role == TreeRole::Assistant {
        return TurnMessages::Answer {
            answer: node.clone(),
        };
    }
    let only = (next.len() == 1).then(|| next[0]);
    let answer = only
        .filter(|n| n.role == TreeRole::Assistant && !tips.contains_key(&node.entry))
        .cloned();
    TurnMessages::Exchange {
        user: node.clone(),
        answer,
    }
}

pub fn table_of_contents(tree: &Tree) -> Toc {
    let tips = tips_of(tree);
    let path = current_path(tree);
    let shown = without_steps(tree, &tips);
    let mut children: HashMap<Option<u64>, Vec<&TreeNode>> = HashMap::new();
    for node in &shown {
        children.entry(node.parent).or_default().push(node);
    }

    fn turns_from(
        start: &TreeNode,
        children: &HashMap<Option<u64>, Vec<&TreeNode>>,
        tips: &HashMap<u64, Vec<Tip>>,
        path: &HashSet<u64>,
        current_entry: Option<u64>,
    ) -> Vec<Turn> {
        let mut turns = Vec::new();
        let mut cur: Option<&TreeNode> = Some(start);
        while let Some(node) = cur {
            let after_node = children
                .get(&Some(node.entry))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let messages = turn_of(node, after_node, tips);
            let last = messages.last();
            let last_entry = last.entry;
            let last_conv = last.conversation;
            let next = children
                .get(&Some(last_entry))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let current = path.contains(&last_entry);
            let follow: Option<&TreeNode> = if current {
                next.iter().copied().find(|each| path.contains(&each.entry))
            } else {
                next.iter()
                    .copied()
                    .find(|each| each.conversation == last_conv)
                    .or_else(|| next.first().copied())
            };
            let branches = next
                .iter()
                .copied()
                .filter(|each| follow.is_none_or(|f| f.entry != each.entry))
                .map(|each| Chain {
                    start: each.entry,
                    conversation: each.conversation,
                    turns: turns_from(each, children, tips, path, current_entry),
                })
                .collect();
            turns.push(Turn {
                messages,
                current,
                head: Some(last_entry) == current_entry,
                tips: tips.get(&last_entry).cloned().unwrap_or_default(),
                branches,
            });
            cur = follow;
        }
        turns
    }

    let roots = children.get(&None).map(Vec::as_slice).unwrap_or(&[]);
    let root = roots
        .iter()
        .copied()
        .find(|n| path.contains(&n.entry))
        .or_else(|| roots.first().copied());

    let turns = root
        .map(|r| turns_from(r, &children, &tips, &path, tree.current))
        .unwrap_or_default();
    let detached = roots
        .iter()
        .copied()
        .filter(|n| root.is_none_or(|r| r.entry != n.entry))
        .map(|n| Chain {
            start: n.entry,
            conversation: n.conversation,
            turns: turns_from(n, &children, &tips, &path, tree.current),
        })
        .collect();

    Toc { turns, detached }
}
