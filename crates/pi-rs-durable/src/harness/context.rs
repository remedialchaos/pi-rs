use super::Conversation;
use crate::{
    ContextEdit, EntryId, EntryQuery, EntryRecord, ScanOrder,
    session::{Error, Result},
};
use pi_rs_chord::context::Context;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct ContextView {
    pub head: Option<EntryRecord>,
    pub entries: Vec<EntryRecord>,
    pub contributions: Vec<Vec<Value>>,
    pub messages: Vec<Value>,
}
impl Conversation {
    pub async fn context(&self, at: Option<EntryId>, ctx: &Context) -> Result<ContextView> {
        self.session
            .commit(
                async |tx| {
                    let tail = match at {
                        Some(id) => {
                            let q = EntryQuery {
                                conversation_id: self.id,
                                min_entry_id: Some(id),
                                max_entry_id: Some(id),
                                order: None,
                            };
                            if tx.scan_entries(&q, 1, None).await?.items.is_empty() {
                                return Err(Error::invalid(format!(
                                    "Entry {id} is not visible from conversation {}",
                                    self.id
                                )));
                            }
                            id
                        }
                        None => match tx.latest_entry_id(self.id).await? {
                            Some(id) => id,
                            None => return Ok(ContextView::default()),
                        },
                    };
                    let query = EntryQuery {
                        conversation_id: self.id,
                        min_entry_id: None,
                        max_entry_id: Some(tail),
                        order: Some(ScanOrder::Descending),
                    };
                    let mut cursor = None;
                    let mut entries = Vec::new();
                    let mut start = None;
                    loop {
                        let page = tx.scan_entries(&query, 256, cursor.as_ref()).await?;
                        for entry in page.items {
                            if start.is_some_and(|id| entry.id < id) {
                                break;
                            }
                            if start.is_none() {
                                start = entry.head;
                            }
                            entries.push(entry);
                        }
                        cursor = page.next;
                        if cursor.is_none()
                            || start.is_some_and(|id| entries.last().is_some_and(|e| e.id <= id))
                        {
                            break;
                        }
                    }
                    entries.reverse();
                    Ok(derive_context(entries))
                },
                ctx,
            )
            .await
    }
}

pub fn derive_context(entries: Vec<EntryRecord>) -> ContextView {
    let head = entries.iter().rev().find(|e| e.head.is_some()).cloned();
    let start = head.as_ref().and_then(|e| e.head).unwrap_or(0);
    let mut edits = HashMap::new();
    let mut active: Vec<_> = head.iter().cloned().collect();
    for entry in entries.into_iter().filter(|e| e.id >= start) {
        for edit in entry.edits.iter().flatten() {
            let id = match edit {
                ContextEdit::Omit { target } | ContextEdit::Replace { target, .. } => *target,
            };
            edits.insert(id, edit.clone());
        }
        if entry.head.is_none() {
            active.push(entry);
        }
    }
    let system = |m: &Value| m["role"] == "system";
    let mut baseline = None;
    for entry in &active {
        if head.as_ref().is_some_and(|h| entry.id <= h.id) {
            continue;
        }
        if entry.kind == "pi.system" {
            baseline = entry.model.iter().flatten().any(system).then_some(entry.id);
            break;
        }
        if head.is_none() && (entry.model.iter().flatten().any(system) || entry.edits.iter().flatten().any(|e|matches!(e,ContextEdit::Replace{messages,..} if messages.iter().any(system)))){break}
    }
    let mut contributions = Vec::new();
    let mut first_system = None;
    for entry in &active {
        let contribution = match edits.get(&entry.id) {
            Some(ContextEdit::Omit { .. }) => Vec::new(),
            Some(ContextEdit::Replace { messages, .. }) => messages.clone(),
            None => entry.model.clone().unwrap_or_default(),
        }
        .into_iter()
        .filter(|m| {
            m["role"] != "assistant"
                || !matches!(
                    m["stopReason"].as_str(),
                    Some("aborted" | "error" | "deferred")
                )
        })
        .collect::<Vec<_>>();
        if first_system.is_none() && contribution.iter().any(system) {
            first_system = Some(entry.id);
        }
        contributions.push(contribution);
    }
    let mut messages =
        order_tool_results(&contributions.iter().flatten().cloned().collect::<Vec<_>>());
    if let Some(index) = messages.iter().position(system)
        && index > 0
        && ((first_system == baseline && baseline.is_some_and(|id| !edits.contains_key(&id)))
            || messages[..index].iter().all(|m| m["role"] == "user"))
    {
        messages[..=index].rotate_right(1);
    }
    ContextView {
        head,
        entries: active,
        contributions,
        messages,
    }
}

pub fn order_tool_results(messages: &[Value]) -> Vec<Value> {
    let mut ordered = Vec::new();
    for (index, m) in messages.iter().enumerate() {
        if m["role"] == "toolResult" {
            continue;
        }
        ordered.push(m.clone());
        if m["role"] != "assistant" {
            continue;
        }
        let mut results = HashMap::new();
        for candidate in messages[index + 1..]
            .iter()
            .take_while(|v| v["role"] != "assistant")
            .filter(|v| v["role"] == "toolResult")
        {
            if let Some(id) = candidate["toolCallId"].as_str() {
                results.entry(id).or_insert(candidate);
            }
        }
        for call in m["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["type"] == "toolCall")
        {
            ordered.push(call["id"].as_str().and_then(|id|results.get(id)).map(|v|(*v).clone()).unwrap_or_else(||json!({
                "role":"toolResult","toolCallId":call["id"],"toolName":call["name"],
                "content":[{"type":"text","text":"Tool result unavailable: history ends before this call completed."}],
                "isError":true,"details":{"reason":"missing_result"},"timestamp":m["timestamp"]
            })));
        }
    }
    ordered
}
