//! Shared contracts and types for the `subagents` plugin.

use std::{cmp::Ordering, collections::BTreeSet};

use pi_rs_chord::{Result, contracts::Contract};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{api::v1::p2_contracts::lookup, runtime::location::Location};

pub const SUBAGENT_TASK_ENTRY: &str = "p2.subagent-task";
pub const AGENT_MESSAGE_ENTRY: &str = "p2.agent-message";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildFailure {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Child {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    pub conversation_id: u64,
    pub model: String,
    pub thinking_level: String,
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub last_entry_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ChildFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageFrom {
    pub name: String,
    pub conversation_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMessageIntent {
    Answered,
    Failed,
    Question,
    Note,
}

impl AgentMessageIntent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Question => "question",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub from: AgentMessageFrom,
    pub intent: AgentMessageIntent,
    pub text: String,
}

/// Parses `provider/modelId` (the model ID may hold additional slashes) into `(provider, model_id)`.
pub fn model_ref_of(text: &str) -> Option<(&str, &str)> {
    let slash = text.find('/')?;
    if slash == 0 {
        return None;
    }
    Some((&text[..slash], &text[slash + 1..]))
}

/// Returns the part of `provider/modelId` (or a model ID) after the last slash.
pub fn short_model_name(text: &str) -> &str {
    text.rsplit_once('/').map_or(text, |(_, tail)| tail)
}

/// Orders subagents by most recently active first (`lastEntryId`, then `conversationId`).
pub fn by_recent_activity(a: &Child, b: &Child) -> Ordering {
    b.last_entry_id
        .cmp(&a.last_entry_id)
        .then_with(|| b.conversation_id.cmp(&a.conversation_id))
}

/// Returns a child's own location, or the session's location if the child follows the session.
pub fn conversation_location(
    session_cwd: Option<&str>,
    session_env: Option<&str>,
    child: Option<&Child>,
) -> Option<Location> {
    if let Some(loc) = child.and_then(|c| c.location.as_ref()) {
        return Some(loc.clone());
    }
    session_cwd.map(|cwd| Location {
        cwd: cwd.to_owned(),
        env: session_env.map(str::to_owned),
    })
}

/// Lists unique non-empty remote environment names used by the session and its subagents.
pub fn remote_environments(session_env: Option<&str>, children: &[Child]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for env in std::iter::once(session_env)
        .chain(
            children
                .iter()
                .map(|c| c.location.as_ref().and_then(|l| l.env.as_deref())),
        )
        .flatten()
    {
        if !env.is_empty() && seen.insert(env.to_owned()) {
            out.push(env.to_owned());
        }
    }
    out
}

/// Formats `<sender .../>` XML-escaped attribute block for a subagent sender.
pub fn subagent_sender_text(name: &str) -> String {
    format!(
        "<sender kind=\"subagent\" name=\"{}\"/>",
        escape_attribute(name)
    )
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strips v3-only fields (`updatedAt`, `failure`) from a serialized `Child` for v2 readers.
pub fn strip_child_v3_fields(mut child: Value) -> Value {
    if let Some(obj) = child.as_object_mut() {
        obj.remove("updatedAt");
        obj.remove("failure");
    }
    child
}

/// Strips v3-only fields from a `{ session -> Child[] }` map for v2 `Subagents.reader.children`.
pub fn strip_children_map_v3(value: Value) -> Value {
    let Some(map) = value.as_object() else {
        return value;
    };
    let mut out = Map::with_capacity(map.len());
    for (session, list) in map {
        let stripped = match list.as_array() {
            Some(items) => Value::Array(items.iter().cloned().map(strip_child_v3_fields).collect()),
            None => list.clone(),
        };
        out.insert(session.clone(), stripped);
    }
    Value::Object(out)
}

pub fn child_store_contract() -> Result<Contract> {
    lookup("p2.child-store")
}

pub fn subagents_contract() -> Result<Contract> {
    lookup("p2.subagents")
}

pub fn subagent_control_contract() -> Result<Contract> {
    lookup("p2.subagent-control")
}
