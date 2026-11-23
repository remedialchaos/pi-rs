use super::ContextView;
use crate::{ContextEdit, EntryDraft, session::Result};
use indexmap::IndexMap;
use pi_rs_ai::{
    transcript::{get_current_tools, get_tool_state_changes},
    types::{Message, Tool, ToolReference},
};
use serde_json::{Value, json};

pub fn replay_sections(messages: &[Value]) -> IndexMap<String, String> {
    let mut sections = IndexMap::new();
    for message in messages.iter().filter(|m| m["role"] == "system") {
        for (key, value) in message["sections"].as_object().into_iter().flatten() {
            if let Some(text) = value.as_str() {
                sections.insert(key.clone(), text.to_owned());
            } else if value.is_null() {
                sections.shift_remove(key);
            }
        }
    }
    sections
}

pub fn plan_system_entries(
    view: &ContextView,
    desired: &IndexMap<String, String>,
    tools: &[Tool],
    timestamp: u64,
) -> Result<Vec<EntryDraft>> {
    let system_entries = || view.entries.iter().filter(|e| e.kind == "pi.system");
    let baseline = view
        .head
        .as_ref()
        .is_some_and(|head| !system_entries().any(|e| e.id > head.id));
    let shown = replay_sections(&view.messages);
    let mut patches = Vec::new();
    if baseline {
        patches.push(Some(json!(desired)));
    } else if shown
        .keys()
        .filter(|k| desired.contains_key(*k))
        .chain(desired.keys().filter(|k| !shown.contains_key(*k)))
        .ne(desired.keys())
    {
        let remove = shown.keys().map(|k| (k.clone(), Value::Null)).collect();
        patches.extend([Some(Value::Object(remove)), Some(json!(desired))]);
    } else {
        let mut patch = serde_json::Map::new();
        for (key, value) in &shown {
            if desired.get(key) != Some(value) {
                patch.insert(key.clone(), json!(desired.get(key)));
            }
        }
        for (key, value) in desired {
            if !shown.contains_key(key) {
                patch.insert(key.clone(), json!(value));
            }
        }
        if !patch.is_empty() {
            patches.push(Some(Value::Object(patch)));
        }
    }
    let messages: Vec<Message> = view
        .messages
        .iter()
        .filter(|m| !baseline && m["role"] == "system")
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    let offered: Vec<_> = get_current_tools(&messages).into_iter().cloned().collect();
    let mut changes = get_tool_state_changes(&offered, tools);
    let retained = offered
        .iter()
        .filter(|t| !changes.tools_removed.iter().any(|r| r.name == t.name));
    if retained
        .chain(&changes.tools_added)
        .map(|t| &t.name)
        .ne(tools.iter().map(|t| &t.name))
    {
        changes.tools_removed = offered
            .into_iter()
            .map(|t| ToolReference { name: t.name })
            .collect();
        changes.tools_added = tools.to_vec();
    }
    if patches.is_empty() && (!changes.tools_added.is_empty() || !changes.tools_removed.is_empty())
    {
        patches.push(None);
    }
    let len = patches.len();
    Ok(patches
        .into_iter()
        .enumerate()
        .map(|(i, patch)| {
            let mut message = json!({"role":"system","content":"","timestamp":timestamp});
            if let Some(sections) = patch {
                message["sections"] = sections;
            }
            if i + 1 == len {
                if !changes.tools_removed.is_empty() {
                    message["toolsRemoved"] = json!(changes.tools_removed);
                }
                if !changes.tools_added.is_empty() {
                    message["toolsAdded"] = json!(changes.tools_added);
                }
            }
            let edits = baseline
                .then(|| {
                    system_entries()
                        .map(|entry| ContextEdit::Omit { target: entry.id })
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());
            EntryDraft {
                kind: "pi.system".into(),
                model: Some(vec![message]),
                head: None,
                data: None,
                edits,
            }
        })
        .collect())
}
