//! Branch command target helpers (`apps/p2/plugins/tree/branch-target.ts`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchRef {
    pub session: String,
    pub conversation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandTarget {
    #[serde(rename = "type")]
    pub target_type: String,
    pub id: String,
    pub kind: String,
    pub label: String,
}

pub fn branch_target(branch: &BranchRef, label: &str) -> CommandTarget {
    CommandTarget {
        target_type: "branch".into(),
        id: json!([branch.session, branch.conversation]).to_string(),
        kind: "Branch".into(),
        label: label.to_string(),
    }
}

pub fn branch_of(text: Option<&str>) -> Option<BranchRef> {
    let text = text?;
    let val: Value = serde_json::from_str(text).ok()?;
    let arr = val.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let session = arr[0].as_str()?.to_string();
    let conversation = arr[1].as_u64().filter(|&n| n >= 1)?;
    Some(BranchRef {
        session,
        conversation,
    })
}
