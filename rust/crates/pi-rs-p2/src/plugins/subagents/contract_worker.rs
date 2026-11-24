//! Worker-side contracts, Durable documents, and tool/model resolution for `subagents`.

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use indexmap::IndexMap;
use pi_rs_ai::{ThinkingLevel, registry::Models};
use pi_rs_chord::{context::Context, json::JsonValue};
use pi_rs_durable::{
    ConversationId, DocDefinition, DocToken, EntryId, ROOT_CONVERSATION_ID, TaskId,
    documents::{DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    errors::BoxError,
    harness::{Agent, ToolRegistration, configure},
    session::Transaction,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::runtime::{location::Location, session_worker::ModelRef};

use super::{
    contract::model_ref_of,
    model_list::{ListedModel, close_models},
};

pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const CODEMODE_TOOL: &str = "codemode";
pub const TEAM_TOOL: &str = "team";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolScope {
    Any,
    Main,
    Subagent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Subagent,
    OneShot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub conversation_id: ConversationId,
    pub model: String,
    pub thinking_level: String,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unplaced_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationId>,
    #[serde(default)]
    pub reported: Vec<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OneShot {
    pub conversation_id: ConversationId,
    pub label: String,
    pub model: String,
    pub thinking_level: String,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentState {
    #[serde(default)]
    pub agents: IndexMap<String, Subagent>,
    #[serde(default)]
    pub reporters: IndexMap<String, TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_shots: Option<IndexMap<String, OneShot>>,
}

/// Durable document `p2.subagents` (v2, conversation-scoped on conversation 1).
pub fn subagents_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.subagents".into(),
        version: 2,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("agents".into(), json!({})),
                ("reporters".into(), json!({})),
            ]))
        })),
        migrate: Some(Arc::new(|mut value, from_version| {
            if from_version != 1 {
                return Err(format!("Unknown p2.subagents version {from_version}").into());
            }
            if let Some(agents) = value.get_mut("agents").and_then(Value::as_object_mut) {
                for agent in agents.values_mut() {
                    if let Some(obj) = agent.as_object_mut()
                        && let Some(cwd) = obj.remove("cwd")
                    {
                        obj.insert("unplacedCwd".into(), cwd);
                    }
                }
            }
            Ok(value)
        })),
        checkpoint_when: Arc::new(|_, _, stats| stats.deltas_since_base >= 64),
    })
}

/// Durable document `p2.inherited-location` (v1, conversation-scoped).
pub fn inherited_location_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.inherited-location".into(),
        version: 1,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| Ok(Map::new()))),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

#[derive(Clone, Debug, Default)]
pub struct AgentChoice {
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub tools: Option<Vec<String>>,
}

pub struct ToolChoice {
    pub chosen: Vec<String>,
    pub tools: Vec<ToolRegistration>,
    pub loaded: Vec<ToolRegistration>,
}

pub struct ResolvedChoice {
    pub model: ModelRef,
    pub thinking_level: ThinkingLevel,
    pub tools: ToolChoice,
}

pub fn called_by_model(tool: &ToolRegistration) -> bool {
    tool.callers
        .as_ref()
        .is_none_or(|callers| callers.iter().any(|c| c == "model"))
}

fn scope_of(tool: &ToolRegistration, access: &IndexMap<String, ToolScope>) -> ToolScope {
    access
        .get(&tool.declaration.name)
        .copied()
        .unwrap_or(ToolScope::Any)
}

fn every_tool(caller: &Agent) -> Vec<ToolRegistration> {
    let mut map: IndexMap<String, ToolRegistration> = IndexMap::new();
    for tool in caller
        .tools
        .iter()
        .chain(caller.deferred.iter())
        .chain(caller.callable.iter())
    {
        map.entry(tool.declaration.name.clone())
            .or_insert_with(|| tool.clone());
    }
    map.into_values().collect()
}

pub fn choosable_tools(
    caller: &Agent,
    access: &IndexMap<String, ToolScope>,
) -> Vec<ToolRegistration> {
    every_tool(caller)
        .into_iter()
        .filter(|tool| called_by_model(tool) && scope_of(tool, access) == ToolScope::Any)
        .collect()
}

pub fn still_given(
    caller: &Agent,
    access: &IndexMap<String, ToolScope>,
    current: Option<&Subagent>,
) -> Option<Vec<String>> {
    let current = current?;
    let choosable: HashSet<String> = choosable_tools(caller, access)
        .into_iter()
        .map(|t| t.declaration.name)
        .collect();
    Some(
        current
            .tools
            .iter()
            .filter(|name| choosable.contains(*name))
            .cloned()
            .collect(),
    )
}

fn tool_named<'a>(tools: &'a [ToolRegistration], name: &str) -> Option<&'a ToolRegistration> {
    if let Some(exact) = tools.iter().find(|t| t.declaration.name == name) {
        return Some(exact);
    }
    let lowered = name.to_lowercase();
    let mut matches = tools
        .iter()
        .filter(|t| t.declaration.name.to_lowercase() == lowered);
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

pub fn resolve_tools(
    caller: &Agent,
    access: &IndexMap<String, ToolScope>,
    names: Option<&[String]>,
    load: &[String],
    kind: AgentKind,
) -> std::result::Result<ToolChoice, String> {
    let all = every_tool(caller);
    let choosable = choosable_tools(caller, access);
    let mut unknown = Vec::new();
    let mut named = Vec::new();
    if let Some(names) = names {
        for name in names {
            match tool_named(&choosable, name) {
                Some(tool) => named.push(tool.clone()),
                None => unknown.push(name.clone()),
            }
        }
    }
    if !unknown.is_empty() {
        let available = choosable
            .iter()
            .map(|t| t.declaration.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Unknown tools: {}. Choose from your own tools: {available}.",
            unknown.join(", ")
        ));
    }
    let chosen = match names {
        None => choosable.clone(),
        Some(_) => {
            let mut seen = BTreeSet::new();
            let mut deduped = Vec::new();
            for tool in named {
                if seen.insert(tool.declaration.name.clone()) {
                    deduped.push(tool);
                }
            }
            deduped
        }
    };
    let to_load: HashSet<String> = load
        .iter()
        .filter_map(|name| tool_named(&choosable, name).map(|t| t.declaration.name.clone()))
        .collect();
    let has_codemode = chosen.iter().any(|t| t.declaration.name == CODEMODE_TOOL);
    let scripts: Vec<ToolRegistration> = if has_codemode {
        all.iter()
            .filter(|t| !called_by_model(t) && scope_of(t, access) != ToolScope::Main)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let own: Vec<ToolRegistration> = if kind == AgentKind::Subagent {
        all.iter()
            .filter(|t| scope_of(t, access) == ToolScope::Subagent)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let chosen_names = chosen.iter().map(|t| t.declaration.name.clone()).collect();
    let mut loaded = own.clone();
    for tool in &chosen {
        if tool.deferred && to_load.contains(&tool.declaration.name) {
            loaded.push(tool.clone());
        }
    }
    let mut tools = chosen;
    tools.extend(scripts);
    tools.extend(own);
    Ok(ToolChoice {
        chosen: chosen_names,
        tools,
        loaded,
    })
}

pub fn parse_thinking_level(text: &str) -> Option<ThinkingLevel> {
    match text {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

pub fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

pub async fn resolve_choice(
    models: &Models,
    own: &Agent,
    access: &IndexMap<String, ToolScope>,
    choice: &AgentChoice,
    current: Option<&Subagent>,
    kind: AgentKind,
    _context: &Context,
) -> std::result::Result<ResolvedChoice, String> {
    let text = choice
        .model
        .as_deref()
        .or_else(|| current.map(|c| c.model.as_str()));
    let parsed_ref: Option<ModelRef> = match text {
        None => own
            .state
            .get("model")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        Some(t) => model_ref_of(t).map(|(provider, model_id)| ModelRef {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        }),
    };
    let model = parsed_ref
        .as_ref()
        .and_then(|r| models.get_model(&r.provider, &r.model_id));
    let (Some(model_ref), Some(model)) = (parsed_ref, model) else {
        let available: Vec<ListedModel> = models
            .get_available(None, &Default::default())
            .await
            .into_iter()
            .flatten()
            .map(|m| ListedModel::from(&m))
            .collect();
        let close = text
            .map(|t| close_models(&available, t))
            .unwrap_or_default();
        let mut parts = vec![format!(
            "Unknown model {}.",
            choice.model.as_deref().unwrap_or("(none)")
        )];
        if !close.is_empty() {
            parts.push(format!("Close matches: {}.", close.join(", ")));
        }
        parts.push(
            "Use the subagent tool's models action (model: a provider or parts of a model id) to list them."
                .into(),
        );
        return Err(parts.join(" "));
    };
    if let Some(level) = choice.thinking_level.as_deref()
        && !THINKING_LEVELS.contains(&level)
    {
        return Err(format!("Invalid thinkingLevel {level}."));
    }
    let requested_str = choice
        .thinking_level
        .as_deref()
        .or_else(|| current.map(|c| c.thinking_level.as_str()))
        .or(own.state.get("thinkingLevel").and_then(Value::as_str))
        .unwrap_or("off");
    let requested = parse_thinking_level(requested_str).unwrap_or(ThinkingLevel::Off);
    let given = still_given(own, access, current);
    let selected_names = choice.tools.as_deref().or(given.as_deref());
    let load = choice.tools.as_deref().unwrap_or(&[]);
    let tools = resolve_tools(own, access, selected_names, load, kind)?;
    Ok(ResolvedChoice {
        model: model_ref,
        thinking_level: model.clamp_thinking_level(requested),
        tools,
    })
}

/// Gives the subagent working in `conversation_id` its own `location` in `tx`.
pub async fn place_subagent(
    tx: &mut Transaction<'_>,
    conversation_id: ConversationId,
    location: &Location,
) -> std::result::Result<bool, BoxError> {
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let raw = tx.doc(&doc, &addr, None).await?;
    let mut state: SubagentState = serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
    let Some((_name, agent)) = state
        .agents
        .iter_mut()
        .find(|(_, each)| each.conversation_id == conversation_id)
    else {
        return Ok(false);
    };
    agent.location = Some(location.clone());
    raw.set(&[], JsonValue::from_serde(serde_json::to_value(&state)?))?;
    let patch = Map::from_iter([("cwd".into(), json!(location.cwd))]);
    configure(tx, conversation_id, &patch).await?;
    Ok(true)
}

pub fn model_ref_string(model: &ModelRef) -> String {
    format!("{}/{}", model.provider, model.model_id)
}
