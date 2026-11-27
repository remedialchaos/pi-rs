use super::AGENT_DOC;
use crate::{
    ConversationId, JsonObject,
    documents::resolve_address,
    session::{Error, Result, Transaction},
};
use pi_rs_chord::delta::Segment;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const DEFAULT_RESTART_LIMIT: u64 = 5;
pub static DEFAULT_RETRY_POLICY: LazyLock<Value> = LazyLock::new(
    || json!({"enabled":true,"maxRetries":3,"baseDelayMs":2000,"maxAgentDelayMs":60000}),
);
pub static DEFAULT_COMPACTION_POLICY: LazyLock<Value> = LazyLock::new(
    || json!({"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000,"backgroundTokens":32768}),
);
pub static DEFAULT_PROGRESS_POLICY: LazyLock<Value> = LazyLock::new(
    || json!({"partialIntervalMs":100,"outputIntervalMs":100,"persistWithinMs":1000}),
);

pub fn resolve_settings(host: &Value) -> Value {
    let mut settings = json!({
        "stream": {}, "retry": *DEFAULT_RETRY_POLICY, "compaction": *DEFAULT_COMPACTION_POLICY,
        "progress": *DEFAULT_PROGRESS_POLICY, "toolExecution":"parallel",
        "steeringMode":"one-at-a-time", "followUpMode":"one-at-a-time",
        "contextRetentionMs":600000, "documentRetentionMs":600000,
        "afterFailedRun":"keep","restartLimit":DEFAULT_RESTART_LIMIT
    });
    for key in [
        "extensions",
        "toolExecution",
        "steeringMode",
        "followUpMode",
        "contextRetentionMs",
        "afterFailedRun",
    ] {
        if !host[key].is_null() {
            settings[key] = host[key].clone();
        }
    }
    for key in ["stream", "retry", "compaction", "progress"] {
        for (field, value) in host[key].as_object().into_iter().flatten() {
            if key == "progress"
                && (value.is_null()
                    || field == "persistWithinMs" && !value.as_f64().is_some_and(|n| n >= 0.0))
            {
                continue;
            }
            settings[key][field] = value.clone();
        }
    }
    if host["documentRetentionMs"]
        .as_f64()
        .is_some_and(|n| n >= 0.0)
    {
        settings["documentRetentionMs"] = host["documentRetentionMs"].clone();
    }
    if let Some(limit) = host["restartLimit"].as_f64().filter(|n| *n >= 1.0) {
        settings["restartLimit"] = json!(limit.floor() as u64);
    }
    settings
}

fn names(items: &Value) -> Result<Value> {
    items
        .as_array()
        .ok_or_else(|| Error::invalid("Expected a registration list"))?
        .iter()
        .map(|item| {
            item["name"]
                .as_str()
                .map(Value::from)
                .ok_or_else(|| Error::invalid("Expected a named registration"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

fn safe_integer(value: &Value, min: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|n| n.fract() == 0.0 && n >= min && n <= 9_007_199_254_740_991.0)
}

fn policy(value: &Value) -> Result<JsonObject> {
    let mut result = JsonObject::new();
    for key in [
        "enabled",
        "reserveTokens",
        "keepRecentTokens",
        "backgroundTokens",
    ] {
        if let Some(v) = value.get(key) {
            if key != "enabled" && !safe_integer(v, 0.0) {
                return Err(Error::invalid(format!(
                    "compaction.{key} must be an integer 0 or more, got {v}"
                )));
            }
            result.insert(key.into(), v.clone());
        }
    }
    Ok(result)
}

/// Apply the supplied agent fields to the rewindable document within the caller's transaction.
pub async fn configure(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    change: &JsonObject,
) -> Result<()> {
    let address = resolve_address(&AGENT_DOC, Some(conversation), None)?;
    let doc = tx.doc(&AGENT_DOC, &address, None).await?;
    for key in [
        "model",
        "thinkingLevel",
        "extensions",
        "tools",
        "modelTools",
        "instructions",
        "cwd",
        "maxTurns",
        "compaction",
    ] {
        let Some(value) = change.get(key) else {
            continue;
        };
        let path = [Segment::Key(key.into())];
        if value.is_null() {
            doc.delete(&path)?;
            continue;
        }
        let stored = match key {
            "maxTurns" if !safe_integer(value, 1.0) => {
                return Err(Error::invalid(format!(
                    "maxTurns must be an integer 1 or more, got {value}"
                )));
            }
            "extensions" | "tools" | "modelTools" if value.is_array() => names(value)?,
            "extensions" | "tools" | "modelTools" => {
                let mut selection = JsonObject::new();
                for field in ["add", "remove"] {
                    if (field != "add" || key != "tools")
                        && let Some(list) = value.get(field)
                    {
                        selection.insert(field.into(), names(list)?);
                    }
                }
                Value::Object(selection)
            }
            "compaction" => {
                let mut settings = policy(value)?;
                if let Some(overrides) = value.get("modelOverrides").and_then(Value::as_object) {
                    let mut models = JsonObject::new();
                    for (model, config) in overrides {
                        models.insert(model.clone(), Value::Object(policy(config)?));
                    }
                    settings.insert("modelOverrides".into(), Value::Object(models));
                }
                Value::Object(settings)
            }
            _ => value.clone(),
        };
        doc.set(&path, stored.into())?;
    }
    Ok(())
}

use super::{Extension, PromptSection, RegistrySnapshot, ToolRegistration, Wrap};
use indexmap::{IndexMap, IndexSet};
use std::sync::Arc;

pub struct Agent {
    pub state: Value,
    pub extensions: Vec<Arc<Extension>>,
    pub tools: Vec<ToolRegistration>,
    pub callable: Vec<ToolRegistration>,
    pub deferred: Vec<ToolRegistration>,
    pub sections: Vec<PromptSection>,
}

fn ordered_names(value: &Value) -> IndexSet<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

pub fn resolve_agent(
    state: Value,
    snapshot: &RegistrySnapshot,
    defaults: Option<&[String]>,
    mut report: impl FnMut(String),
) -> Agent {
    let selection = &state["extensions"];
    let mut selected = IndexSet::new();
    if selection.is_array() {
        selected = ordered_names(selection);
    } else {
        match defaults {
            Some(names) => selected.extend(names.iter().map(String::as_str)),
            None => selected.extend(snapshot.installed().map(|e| e.name.as_str())),
        }
        selected.extend(ordered_names(&selection["add"]));
        let removed = ordered_names(&selection["remove"]);
        selected.retain(|n| !removed.contains(n));
    }
    let extensions: Vec<_> = selected
        .iter()
        .filter_map(|n| snapshot.extension(n).cloned())
        .collect();
    let mut composed = IndexMap::new();
    let mut sections = IndexMap::new();
    for e in &extensions {
        for t in &e.tools {
            composed.insert(t.declaration.name.clone(), t.clone());
        }
        for s in &e.sections {
            sections.insert(s.key.clone(), s.clone());
        }
    }
    for e in &extensions {
        for wrap in &e.wraps {
            match wrap {
                Wrap::Tool { name, wrap } => {
                    if let Some(tool) = composed.get(name) {
                        let wrapped = super::parks::wrap_composite(tool.clone(), wrap.as_ref());
                        if wrapped.declaration.name == *name {
                            composed.insert(name.clone(), wrapped);
                        } else {
                            report(format!(
                                "Wrapper renamed {name} to {}",
                                wrapped.declaration.name
                            ));
                            composed.shift_remove(name);
                        }
                    }
                }
                Wrap::Section { key, wrap } => {
                    if let Some(s) = sections.get(key) {
                        let wrapped = wrap(s.clone());
                        if wrapped.key == *key {
                            sections.insert(key.clone(), wrapped);
                        } else {
                            report(format!("Wrapper renamed {key} to {}", wrapped.key));
                            sections.shift_remove(key);
                        }
                    }
                }
            }
        }
    }
    let filter = &state["tools"];
    let enabled: Vec<_> = if filter.is_array() {
        ordered_names(filter)
            .iter()
            .filter_map(|n| composed.get(*n).cloned())
            .collect()
    } else {
        let removed = ordered_names(&filter["remove"]);
        composed
            .into_values()
            .filter(|t| !removed.contains(t.declaration.name.as_str()))
            .collect()
    };
    let callable = enabled
        .iter()
        .filter(|t| {
            t.callers
                .as_ref()
                .is_none_or(|c| c.iter().any(|n| n == "tools"))
        })
        .cloned()
        .collect();
    let model: IndexMap<_, _> = enabled
        .into_iter()
        .filter(|t| {
            t.callers
                .as_ref()
                .is_none_or(|c| c.iter().any(|n| n == "model"))
        })
        .map(|t| (t.declaration.name.clone(), t))
        .collect();
    let offered = &state["modelTools"];
    let names = if offered.is_array() {
        ordered_names(offered)
    } else {
        let remove = ordered_names(&offered["remove"]);
        let mut names: IndexSet<_> = model
            .values()
            .filter(|t| !t.deferred)
            .map(|t| t.declaration.name.as_str())
            .collect();
        names.extend(
            ordered_names(&offered["add"])
                .into_iter()
                .filter(|n| model.get(*n).is_some_and(|t| t.deferred)),
        );
        names.retain(|n| !remove.contains(n));
        names
    };
    let tools = names
        .iter()
        .filter_map(|n| model.get(*n).cloned())
        .collect();
    let deferred = model
        .values()
        .filter(|t| t.deferred && !names.contains(t.declaration.name.as_str()))
        .cloned()
        .collect();
    if let Some(text) = state["instructions"].as_str() {
        let owned = text.to_owned();
        sections.insert(
            "instructions".into(),
            super::section(
                "instructions",
                Arc::new(move |_, _| {
                    let text = owned.clone();
                    Box::pin(async move { Ok(Some(text)) })
                }),
                None,
            ),
        );
    }
    Agent {
        state,
        extensions,
        tools,
        callable,
        deferred,
        sections: sections.into_values().collect(),
    }
}

/// The extension whose code a tool of `agent` runs: the last selected one that provides or
/// wraps it.
pub fn tool_extension<'a>(agent: &'a Agent, name: &str) -> Option<&'a str> {
    agent.extensions.iter().rev().find_map(|extension| {
        let provides = extension.tools.iter().any(|t| t.declaration.name == name);
        let wraps = extension
            .wraps
            .iter()
            .any(|w| matches!(w, Wrap::Tool { name: target, .. } if target == name));
        (provides || wraps).then_some(extension.name.as_str())
    })
}

pub async fn add_tools(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    added: &[String],
) -> Result<()> {
    let address = resolve_address(&AGENT_DOC, Some(conversation), None)?;
    let doc = tx.doc(&AGENT_DOC, &address, None).await?;
    let state = serde_json::to_value(doc.value()?.to_json())?;
    for field in ["tools", "modelTools"] {
        let mut value = state[field].clone();
        if let Some(list) = value.as_array_mut() {
            for name in added {
                if !list.iter().any(|v| v == name) {
                    list.push(json!(name));
                }
            }
        } else {
            if let Some(remove) = value.get_mut("remove").and_then(Value::as_array_mut) {
                remove.retain(|v| !added.iter().any(|name| v == name));
            }
            if field == "modelTools" {
                let mut list = value["add"].as_array().cloned().unwrap_or_default();
                for name in added {
                    if !list.iter().any(|v| v == name) {
                        list.push(json!(name));
                    }
                }
                value["add"] = json!(list);
            }
        }
        if value != state[field] {
            doc.set(&[Segment::Key(field.into())], value.into())?;
        }
    }
    Ok(())
}

impl super::Invocation {
    /// Resolve the agent for the conversation that owns this invocation.
    pub async fn agent(&self, ctx: &pi_rs_chord::context::Context) -> Result<Agent> {
        self.harness()
            .handle(self.record.conversation_id)
            .agent(ctx)
            .await
    }
}
