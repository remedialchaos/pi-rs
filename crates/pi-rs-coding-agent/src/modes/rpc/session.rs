use std::{path::Path, sync::Arc};

use pi_rs_agent::AgentMessage;
use pi_rs_ai::{AssistantContent, StopReason};
use serde_json::{Value, json};

use crate::core::{
    agent_session::{AgentSession, ForkPosition, SessionManager as SessionStore},
    agent_session_services::DiskSession,
    export_html::{ExportOptions, SessionSnapshot, export_session_html},
    session_manager::{SessionEntry, SessionManager, SessionTreeNode},
};

use super::RpcSessionState;

pub(super) fn state(session: &AgentSession) -> RpcSessionState {
    let settings = session.settings();
    let manager = session.session_manager();
    RpcSessionState {
        model: Some(session.model()),
        thinking_level: session.thinking_level(),
        is_streaming: session.is_streaming(),
        is_compacting: session.is_compacting(),
        steering_mode: settings.steering_mode(),
        follow_up_mode: settings.follow_up_mode(),
        session_file: session.session_file(),
        session_id: session.session_id(),
        session_name: manager
            .disk()
            .and_then(|disk| disk.lock().get_session_name().map(str::to_owned)),
        auto_compaction_enabled: settings.compaction_enabled(),
        message_count: session.messages().len(),
        pending_message_count: session.peek_steering().len() + session.peek_follow_up().len(),
    }
}

pub(super) fn stats(session: &AgentSession) -> Value {
    let stats = session.stats();
    let mut data = json!({
        "sessionId": stats.session_id,
        "userMessages": stats.user_messages,
        "assistantMessages": stats.assistant_messages,
        "toolCalls": stats.tool_calls,
        "toolResults": stats.tool_results,
        "totalMessages": stats.total_messages,
        "tokens": {
            "input": stats.tokens.input,
            "output": stats.tokens.output,
            "cacheRead": stats.tokens.cache_read,
            "cacheWrite": stats.tokens.cache_write,
            "total": stats.tokens.total,
        },
        "cost": stats.cost,
    });
    if let Some(file) = stats.session_file {
        data["sessionFile"] = json!(file);
    }
    if let Some(usage) = stats.context_usage.filter(|usage| usage.context_window > 0) {
        data["contextUsage"] = json!({
            "tokens": usage.tokens,
            "contextWindow": usage.context_window,
            "percent": usage.percent,
        });
    }
    data
}

pub(in crate::modes) fn set_name(
    session: &AgentSession,
    name: &str,
) -> Result<Option<String>, String> {
    let name = js_trim(name);
    if name.is_empty() {
        return Err("Session name cannot be empty".into());
    }
    let manager = session.session_manager();
    let disk = manager
        .disk()
        .ok_or("Session manager does not support session names")?;
    let mut manager = disk.lock();
    manager.append_session_info(name)?;
    Ok(manager.get_session_name().map(str::to_owned))
}

pub(super) fn entries(session: &AgentSession, since: Option<&str>) -> Result<Value, String> {
    let session_manager = session.session_manager();
    if let Some(disk) = session_manager.disk() {
        let disk = disk.lock();
        let entries = disk.get_entries();
        let start = after(since, entries.iter().map(SessionEntry::id))?;
        return Ok(json!({ "entries": &entries[start..], "leafId": disk.get_leaf_id() }));
    }
    let entries = session_manager.entries();
    let start = after(since, entries.iter().map(|entry| entry.id.as_str()))?;
    Ok(
        json!({ "entries": entries[start..].iter().map(|entry| &entry.data).collect::<Vec<_>>(), "leafId": entries.last().map(|entry| &entry.id) }),
    )
}

fn after<'a>(since: Option<&str>, mut ids: impl Iterator<Item = &'a str>) -> Result<usize, String> {
    match since {
        None => Ok(0),
        Some(since) => ids
            .position(|id| id == since)
            .map(|index| index + 1)
            .ok_or_else(|| format!("Entry not found: {since}")),
    }
}

pub(super) fn tree(session: &AgentSession) -> Value {
    fn node_to_json(node: SessionTreeNode) -> Value {
        let mut data = json!({ "entry": node.entry, "children": node.children.into_iter().map(node_to_json).collect::<Vec<_>>() });
        if let Some(label) = node.label {
            data["label"] = json!(label);
        }
        if let Some(timestamp) = node.label_timestamp {
            data["labelTimestamp"] = json!(timestamp);
        }
        data
    }
    let manager = session.session_manager();
    if let Some(disk) = manager.disk() {
        let disk = disk.lock();
        let nodes = disk
            .get_tree()
            .into_iter()
            .map(node_to_json)
            .collect::<Vec<_>>();
        return json!({ "tree": nodes, "leafId": disk.get_leaf_id() });
    }
    let entries = manager.entries();
    let leaf = entries.last().map(|entry| &entry.id);
    let mut tree = Vec::new();
    for entry in entries.iter().rev() {
        tree = vec![json!({ "entry": entry.data, "children": tree })];
    }
    json!({ "tree": tree, "leafId": leaf })
}

pub(in crate::modes) fn leaf_id(session: &AgentSession) -> Option<String> {
    let manager = session.session_manager();
    match manager.disk() {
        Some(disk) => disk.lock().get_leaf_id().map(str::to_owned),
        None => manager.entries().pop().map(|entry| entry.id),
    }
}

fn user_text(message: &Value) -> Option<String> {
    if message.get("role")?.as_str()? != "user" {
        return None;
    }
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let mut text = String::new();
    for block in content.as_array()? {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(block) = block.get("text").and_then(Value::as_str)
        {
            text.push_str(block);
        }
    }
    Some(text)
}

pub(super) fn fork_messages(session: &AgentSession) -> Vec<Value> {
    let manager = session.session_manager();
    let pair = |id: &str, message: &Value| {
        let text = user_text(message)?;
        (!text.is_empty()).then(|| json!({ "entryId": id, "text": text }))
    };
    if let Some(disk) = manager.disk() {
        return disk
            .lock()
            .get_entries()
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message { id, message, .. } => pair(id, message),
                _ => None,
            })
            .collect();
    }
    manager
        .entries()
        .iter()
        .filter_map(|entry| pair(&entry.id, &entry.data))
        .collect()
}

pub(in crate::modes) fn last_assistant_text(session: &AgentSession) -> Option<String> {
    let messages = session.messages();
    let assistant = messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) if assistant.stop_reason == StopReason::Stop || assistant.content.iter().any(|block| matches!(block, AssistantContent::Text(text) if !js_trim(&text.text).is_empty())) => Some(assistant),
        _ => None,
    })?;
    let text: String = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    let trimmed = js_trim(&text);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

pub(in crate::modes) fn new_session(
    session: &AgentSession,
    parent: Option<&str>,
) -> Result<Arc<dyn SessionStore>, String> {
    let current = session.session_manager();
    let cwd = current.cwd();
    let mut manager = match current.disk() {
        Some(disk) if disk.lock().is_persisted() => {
            SessionManager::create_in(&cwd, disk.lock().get_session_dir(), None)?
        }
        _ => SessionManager::in_memory_at(&cwd),
    };
    if let Some(parent) = parent.filter(|parent| !parent.is_empty()) {
        manager.new_session(None, Some(parent))?;
    }
    Ok(Arc::new(DiskSession::new(manager)))
}

pub(in crate::modes) fn switch_session(
    session: &AgentSession,
    path: &str,
) -> Result<Arc<dyn SessionStore>, String> {
    let manager = SessionManager::open(path)?;
    let cwd = manager.get_cwd();
    if !cwd.is_empty()
        && !Path::new(cwd).exists()
        && let Some(file) = manager.get_session_file()
    {
        return Err(format!(
            "Stored session working directory does not exist: {cwd}\nSession file: {}\nCurrent working directory: {}",
            file.display(),
            session.cwd()
        ));
    }
    Ok(Arc::new(DiskSession::new(manager)))
}

pub(in crate::modes) type ForkResult = (Arc<dyn SessionStore>, Option<String>);

pub(in crate::modes) fn fork_session(
    session: &AgentSession,
    entry_id: &str,
    position: ForkPosition,
) -> Result<ForkResult, String> {
    const INVALID: &str = "Invalid entry ID for forking";
    let manager = session.session_manager();
    let Some(disk) = manager.disk() else {
        let text = match position {
            ForkPosition::At => None,
            ForkPosition::Before => Some(
                manager
                    .entries()
                    .iter()
                    .find(|entry| entry.id == entry_id)
                    .and_then(|entry| user_text(&entry.data))
                    .ok_or(INVALID)?,
            ),
        };
        manager
            .fork(entry_id, position)
            .map_err(|error| error.to_string())?;
        return Ok((manager, text));
    };
    let disk = disk.lock();
    let entry = disk.get_entry(entry_id).ok_or(INVALID)?;
    let (target, text) = match position {
        ForkPosition::At => (Some(entry_id), None),
        ForkPosition::Before => match entry {
            SessionEntry::Message { message, .. } => {
                (entry.parent_id(), Some(user_text(message).ok_or(INVALID)?))
            }
            _ => return Err(INVALID.into()),
        },
    };
    let mut forked = if let Some(path) = disk.get_session_file() {
        if let Some(target) = target {
            if !path.exists() {
                return Err("This session has not been saved yet. Send a message before cloning or forking it.".into());
            }
            let mut forked =
                SessionManager::open_with(path, Some(disk.get_session_dir().to_owned()), None)?;
            if forked.fork(target)?.is_none() {
                return Err("Failed to create forked session".into());
            }
            forked
        } else {
            let mut forked =
                SessionManager::create_in(disk.get_cwd(), disk.get_session_dir(), None)?;
            forked.new_session(None, path.to_str())?;
            forked
        }
    } else if target.is_some() {
        // In-memory forks keep the tree and navigate to the selected parent or leaf.
        SessionManager::in_memory_with_entries(
            disk.get_cwd(),
            Some(disk.get_session_id()),
            disk.get_entries().to_vec(),
        )?
    } else {
        SessionManager::in_memory_at(disk.get_cwd())
    };
    if !forked.is_persisted()
        && let Some(target) = target
    {
        forked.fork(target)?;
    }
    Ok((Arc::new(DiskSession::new(forked)), text))
}

pub(super) fn export_html(session: &AgentSession, output: Option<String>) -> Result<Value, String> {
    let manager = session.session_manager();
    let disk = manager
        .disk()
        .ok_or("Cannot export in-memory session to HTML")?;
    let prompt = session.system_prompt();
    let (path, snapshot) = {
        let disk = disk.lock();
        let file = disk
            .get_session_file()
            .ok_or("Cannot export in-memory session to HTML")?;
        if !file.exists() {
            return Err("Nothing to export yet - start a conversation first".into());
        }
        let path = output.filter(|path| !path.is_empty()).unwrap_or_else(|| {
            format!(
                "pi-session-{}.html",
                file.file_stem().unwrap_or_default().to_string_lossy()
            )
        });
        let snapshot = SessionSnapshot {
            header: json!(disk.get_header()),
            entries: disk
                .get_entries()
                .iter()
                .map(|entry| json!(entry))
                .collect(),
            leaf_id: disk.get_leaf_id().map(str::to_owned),
            system_prompt: Some(prompt),
            ..SessionSnapshot::default()
        };
        (path, snapshot)
    };
    let html = export_session_html(&snapshot, &ExportOptions::default())?;
    let resolved = crate::core::skills::resolve_path_default(Path::new(&path));
    std::fs::write(resolved, html).map_err(|error| error.to_string())?;
    Ok(json!({ "path": path }))
}
