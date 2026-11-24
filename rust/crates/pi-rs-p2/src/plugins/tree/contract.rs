//! `/tree` contracts and wire types (`p2.tree`).

use std::sync::Arc;

use pi_rs_chord::{Result, contracts::Contract};
use serde::{Deserialize, Serialize};

use crate::api::v1::p2_contracts::lookup;

/// How much of a message a node previews, and the longest branch label.
pub const PREVIEW_LENGTH: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub entry: u64,
    pub parent: Option<u64>,
    pub role: TreeRole,
    pub text: String,
    pub conversation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeBranch {
    pub conversation: u64,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tree {
    pub nodes: Vec<TreeNode>,
    pub branches: Vec<TreeBranch>,
    pub main: u64,
    pub current: Option<u64>,
}

pub fn session_tree_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.tree")?))
}
