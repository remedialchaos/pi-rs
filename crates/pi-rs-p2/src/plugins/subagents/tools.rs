//! Durable tasks (`Anchor`, `Reporter`), `subagent` and `team` tools, and location/archive helpers.

use std::{
    collections::{BTreeMap, HashSet},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool, registry::Models};
use pi_rs_chord::{context::Context, json::JsonValue, sync::SyncFuture};
use pi_rs_durable::{
    AbortSubmissionResult, ConversationId, ConversationOwnership, EntryQuery, ROOT_CONVERSATION_ID,
    SubmissionKind, SubmissionRecord, SubmissionStatus, TaskOutcome, TaskOwnership, TaskState,
    documents::resolve_address,
    errors::BoxError,
    harness::{
        AGENT_DOC, Conversation, Extension, Harness, InputEntry, Invocation, LIVE_DOC, Submission,
        SubmissionBody, SubmissionDraft, ToolRegistration, configure,
    },
    session::{Error as DurableError, Session, TaskOptions},
    tasks::{PhaseHandler, Task, TaskDefinition, define_task},
    tools::ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::runtime::{
    location::{Location, LocationInput, location_text},
    main_conversation::main_conversation,
};

use super::{
    contract::{
        AGENT_MESSAGE_ENTRY, AgentMessage, AgentMessageFrom, AgentMessageIntent,
        SUBAGENT_TASK_ENTRY, subagent_sender_text,
    },
    contract_worker::{
        AgentChoice, AgentKind, CODEMODE_TOOL, Subagent, SubagentState, TEAM_TOOL, ToolChoice,
        ToolScope, inherited_location_doc, resolve_choice, resolve_tools, still_given,
        subagents_doc, thinking_level_str,
    },
    model_list::{ListedModel, list_models},
};

const SUBAGENT_EXTENSION: &str = "subagent-tools";
const UNDELIVERED: &str =
    "its previous run failed before this message was delivered; send it again";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReporterInput {
    pub name: String,
    pub conversation_id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationId>,
    pub message: String,
    pub follow_up: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportPayload {
    intent: AgentMessageIntent,
    text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
enum ReporterCheckpoint {
    Deliver,
    Report {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<ReportPayload>,
    },
}

/// Durable task `p2.subagent-anchor`: owns a subagent conversation as a background task.
pub fn anchor_task() -> Arc<Task<Invocation>> {
    let done: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move {
            inv.commit(ctx, async |_, _| {
                Ok(TaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: Value::Null,
                    },
                })
            })
            .await?;
            Ok(())
        })
    });
    let abort: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move {
            inv.commit(ctx, async |_, _| {
                Ok(TaskState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                })
            })
            .await?;
            Ok(())
        })
    });
    Arc::new(define_task(TaskDefinition {
        name: "p2.subagent-anchor".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({ "phase": "done" }))),
        phases: [("done".into(), done)].into_iter().collect(),
        abort,
        migrate: None,
        hooks: (),
    }))
}

/// Durable task `p2.subagent-reporter`: delivers a message to a subagent and reports its answer back.
pub fn reporter_task() -> Arc<Task<Invocation>> {
    let deliver: PhaseHandler<Invocation> = Arc::new(|record, inv, ctx| {
        Box::pin(async move {
            let input: ReporterInput = serde_json::from_value(record.input.clone())?;
            let harness = inv.harness();
            let subagent = harness
                .conversation(input.conversation_id, ctx)
                .await?
                .ok_or_else(|| format!("Subagent {} has no conversation", input.name))?;
            let submission = subagent
                .submit(
                    SubmissionDraft {
                        request_id: Some(format!("subagent:{}", record.id)),
                        notify: None,
                        body: SubmissionBody::Input {
                            content: json!(input.message),
                            entry: Some(InputEntry {
                                kind: SUBAGENT_TASK_ENTRY.into(),
                                data: Some(json!({})),
                            }),
                            when_busy: Some(
                                if input.follow_up { "followUp" } else { "steer" }.into(),
                            ),
                            when_idle: None,
                            on_abort: None,
                        },
                    },
                    ctx,
                )
                .await?;
            let settled = settled_or_withdrawn(
                &submission,
                &subagent,
                || {
                    let ctx = ctx.clone();
                    Box::pin(async move {
                        let _ = ctx
                            .await_with(tokio::time::sleep(Duration::from_secs(1)))
                            .await;
                    })
                },
                ctx,
            )
            .await?;

            inv.commit(ctx, async move |tx, _| {
                let mut report = None;
                match settled {
                    None => {
                        report = Some(ReportPayload {
                            intent: AgentMessageIntent::Failed,
                            text: UNDELIVERED.into(),
                        });
                    }
                    Some(rec) if rec.status == SubmissionStatus::Unanswered => {
                        if rec.reason.as_deref() != Some("aborted") {
                            report = Some(ReportPayload {
                                intent: AgentMessageIntent::Failed,
                                text: rec.reason.clone().unwrap_or_else(|| "failed".into()),
                            });
                        }
                    }
                    Some(rec)
                        if matches!(rec.kind, SubmissionKind::Input | SubmissionKind::Continue) =>
                    {
                        if let Some(answer_id) = rec.answer {
                            let doc = subagents_doc();
                            let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
                            let raw = tx.doc(&doc, &addr, None).await?;
                            let mut state: SubagentState =
                                serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                            if let Some(agent) = state.agents.get_mut(&input.name)
                                && !agent.reported.contains(&answer_id)
                            {
                                agent.reported.push(answer_id);
                                tx.doc(&doc, &addr, None).await?.set(
                                    &[],
                                    JsonValue::from_serde(serde_json::to_value(&state)?),
                                )?;
                                let answer_entry = tx.entry(answer_id).await?;
                                let text = answer_entry
                                    .as_ref()
                                    .and_then(|e| e.model.as_deref())
                                    .and_then(|m| m.first())
                                    .map(extract_assistant_text)
                                    .unwrap_or_default();
                                report = Some(ReportPayload {
                                    intent: AgentMessageIntent::Answered,
                                    text,
                                });
                            }
                        }
                    }
                    Some(_) => {}
                }
                let checkpoint = serde_json::to_value(ReporterCheckpoint::Report { report })?;
                Ok(TaskState::Running {
                    checkpoint,
                    wake: None,
                })
            })
            .await?;
            Ok(())
        })
    });

    let report: PhaseHandler<Invocation> = Arc::new(|record, inv, ctx| {
        Box::pin(async move {
            let input: ReporterInput = serde_json::from_value(record.input.clone())?;
            let checkpoint: ReporterCheckpoint = match &record.state {
                TaskState::Running { checkpoint, .. } => {
                    serde_json::from_value(checkpoint.clone())?
                }
                _ => ReporterCheckpoint::Report { report: None },
            };
            if let ReporterCheckpoint::Report {
                report: Some(payload),
            } = checkpoint
            {
                let harness = inv.harness();
                let target_id = match input.parent {
                    Some(p) => p,
                    None => main_conversation(&harness.session, ctx).await?,
                };
                if let Some(target) = harness.conversation(target_id, ctx).await? {
                    let msg = AgentMessage {
                        from: AgentMessageFrom {
                            name: input.name.clone(),
                            conversation_id: input.conversation_id,
                        },
                        intent: payload.intent,
                        text: payload.text,
                    };
                    let (content, entry) =
                        agent_message_input(&msg, &format!("subagent send to {}", input.name))?;
                    target
                        .submit(
                            SubmissionDraft {
                                request_id: Some(format!("subagent-report:{}", record.id)),
                                notify: None,
                                body: SubmissionBody::Input {
                                    content,
                                    entry: Some(entry),
                                    when_busy: Some("followUp".into()),
                                    when_idle: None,
                                    on_abort: Some("keep".into()),
                                },
                            },
                            ctx,
                        )
                        .await?;
                }
            }
            inv.commit(ctx, async |_, _| {
                Ok(TaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: Value::Null,
                    },
                })
            })
            .await?;
            Ok(())
        })
    });

    let abort: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move {
            inv.commit(ctx, async |_, _| {
                Ok(TaskState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                })
            })
            .await?;
            Ok(())
        })
    });

    Arc::new(define_task(TaskDefinition {
        name: "p2.subagent-reporter".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({ "phase": "deliver" }))),
        phases: [("deliver".into(), deliver), ("report".into(), report)]
            .into_iter()
            .collect(),
        abort,
        migrate: None,
        hooks: (),
    }))
}

/// Waits for a submission to settle, withdrawing it if the subagent goes idle while the submission remains queued.
pub async fn settled_or_withdrawn(
    submission: &Submission,
    subagent: &Conversation,
    pause: impl Fn() -> Pin<Box<dyn Future<Output = ()> + Send>>,
    context: &Context,
) -> std::result::Result<Option<Arc<SubmissionRecord>>, BoxError> {
    loop {
        let settled_fut = submission.wait(context);
        let idle_fut = subagent.wait_for_idle(context);
        let went_idle = tokio::select! {
            res = settled_fut => return Ok(Some(res?)),
            res = idle_fut => { res?; true }
        };
        if !went_idle {
            unreachable!();
        }
        if submission.status(context).await?.status != SubmissionStatus::Queued {
            return Ok(Some(submission.wait(context).await?));
        }
        pause().await;
        if submission.status(context).await?.status != SubmissionStatus::Queued {
            return Ok(Some(submission.wait(context).await?));
        }
        if submission.abort(context).await? == AbortSubmissionResult::Aborted {
            return Ok(None);
        }
    }
}

pub fn extract_assistant_text(message: &Value) -> String {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = part.get("text").and_then(Value::as_str)
        {
            out.push_str(text);
        }
    }
    out
}

pub fn agent_message_input(
    message: &AgentMessage,
    reply_with: &str,
) -> std::result::Result<(Value, InputEntry), BoxError> {
    let label = match message.intent {
        AgentMessageIntent::Answered => "answered, no reply needed".to_owned(),
        AgentMessageIntent::Failed => "failed".to_owned(),
        AgentMessageIntent::Question => format!("asks; answer with {reply_with}"),
        AgentMessageIntent::Note => "notes, no reply needed".to_owned(),
    };
    let content = json!([
        { "type": "text", "text": subagent_sender_text(&message.from.name) },
        { "type": "text", "text": format!("[{label}] {}", message.text) },
    ]);
    let entry = InputEntry {
        kind: AGENT_MESSAGE_ENTRY.into(),
        data: Some(serde_json::to_value(message)?),
    };
    Ok((content, entry))
}

async fn latest_answer(
    session: &Session,
    conversation_id: ConversationId,
    context: &Context,
) -> std::result::Result<Option<String>, BoxError> {
    let mut cursor = None;
    let mut scanned = 0usize;
    while scanned < 2000 {
        let page = session
            .scan_entries(
                &EntryQuery {
                    conversation_id,
                    ..Default::default()
                },
                200,
                cursor.as_ref(),
                context,
            )
            .await?;
        for entry in page.items {
            scanned += 1;
            if entry.kind == "assistant"
                && let Some(msg) = entry.model.as_deref().and_then(|m| m.first())
                && msg.get("stopReason").and_then(Value::as_str) == Some("stop")
            {
                let text = extract_assistant_text(msg);
                if !text.trim().is_empty() {
                    return Ok(Some(text));
                }
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, Default)]
pub struct PlacedSubagents {
    pub followers: Vec<ConversationId>,
    pub own: Vec<Location>,
}

/// Places v1 unplaced cwds on `spawned_on`, then returns follower conversations and own locations.
pub async fn place_subagents(
    harness: &Harness,
    spawned_on: impl Fn(ConversationId, &str) -> Option<String>,
    context: &Context,
) -> std::result::Result<PlacedSubagents, BoxError> {
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let mut state: SubagentState = match harness.session.snapshot(&doc, &addr, context).await? {
        Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
        None => SubagentState::default(),
    };

    if state.agents.values().any(|a| a.unplaced_cwd.is_some()) {
        harness
            .commit(
                async |tx| {
                    let raw = tx.doc(&doc, &addr, None).await?;
                    let mut current: SubagentState =
                        serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                    for agent in current.agents.values_mut() {
                        if let Some(cwd) = agent.unplaced_cwd.take() {
                            let env = spawned_on(agent.conversation_id, &cwd);
                            agent.location = Some(Location { env, cwd });
                        }
                    }
                    raw.set(&[], JsonValue::from_serde(serde_json::to_value(&current)?))?;
                    Ok(())
                },
                context,
            )
            .await?;
        state = match harness.session.snapshot(&doc, &addr, context).await? {
            Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
            None => SubagentState::default(),
        };
    }

    let mut followers = Vec::new();
    let mut own = Vec::new();
    for agent in state.agents.values() {
        match &agent.location {
            None => followers.push(agent.conversation_id),
            Some(loc) => own.push(loc.clone()),
        }
    }
    if let Some(one_shots) = &state.one_shots {
        for shot in one_shots.values() {
            followers.push(shot.conversation_id);
        }
    }
    Ok(PlacedSubagents { followers, own })
}

/// Resolves where v1 unplaced cwds were spawned using recorded `subagentLocations` or `session_env`.
pub fn spawn_environments(
    recorded: Option<&Value>,
    session_env: Option<String>,
) -> impl Fn(ConversationId, &str) -> Option<String> {
    let known: BTreeMap<String, Location> = recorded
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    move |conversation_id, cwd| {
        if let Some(loc) = known.get(&conversation_id.to_string())
            && loc.cwd == cwd
        {
            return loc.env.clone();
        }
        session_env.clone()
    }
}

/// Returns a conversation's own location, or `None` if it follows the session.
pub async fn own_location(
    session: &Session,
    conversation_id: ConversationId,
    context: &Context,
) -> std::result::Result<Option<Location>, BoxError> {
    if conversation_id == ROOT_CONVERSATION_ID {
        return Ok(None);
    }
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    if let Some(raw) = session.snapshot(&doc, &addr, context).await? {
        let state: SubagentState = serde_json::from_value(Value::Object((*raw).clone()))?;
        if let Some(loc) = state
            .agents
            .values()
            .find(|a| a.conversation_id == conversation_id)
            .and_then(|a| a.location.clone())
        {
            return Ok(Some(loc));
        }
    }
    let inherited = inherited_location_doc();
    let inh_addr = resolve_address(&inherited, Some(conversation_id), None)?;
    if let Some(raw) = session.snapshot(&inherited, &inh_addr, context).await? {
        let loc: Option<Location> = raw
            .get("location")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        return Ok(loc);
    }
    Ok(None)
}

pub type DirectoryResolver = Arc<
    dyn Fn(LocationInput, Location, Context) -> SyncFuture<std::result::Result<Location, String>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct SubagentLocator {
    pub locate: Arc<dyn Fn(ConversationId, Context) -> SyncFuture<Location> + Send + Sync>,
    pub sandbox: Arc<dyn Fn(Location) -> SyncFuture<Option<String>> + Send + Sync>,
    pub directory: DirectoryResolver,
}

pub async fn place_target(
    locator: &SubagentLocator,
    caller: &Location,
    env: Option<Option<&str>>,
    cwd: Option<&str>,
    context: &Context,
) -> std::result::Result<Location, String> {
    let resolved_env: Option<String> = match env {
        None => caller.env.clone(),
        Some(e) => e.map(str::to_owned),
    };
    let sandbox = (locator.sandbox)(caller.clone())
        .await
        .map_err(|e| e.to_string())?;
    if let Some(sb) = sandbox
        && resolved_env.as_deref() != Some(sb.as_str())
    {
        return Err(format!(
            "You work in sandbox {sb}; your subagents can only work inside it."
        ));
    }
    let input = LocationInput {
        env: resolved_env.clone(),
        path: cwd.unwrap_or("").to_owned(),
    };
    match (locator.directory)(input, caller.clone(), context.clone())
        .await
        .map_err(|e| e.to_string())?
    {
        Ok(loc) => Ok(loc),
        Err(err) => {
            let prefix = if resolved_env == caller.env {
                String::new()
            } else {
                format!("{}:", resolved_env.as_deref().unwrap_or("local"))
            };
            let path = cwd.unwrap_or("~");
            let where_str = format!("{prefix}{path}");
            if err == "not an existing directory" {
                Err(format!("cwd {where_str} is not an existing directory."))
            } else {
                Err(format!("cwd {where_str}: {err}"))
            }
        }
    }
}

/// Archives (aborting work first) or restores a subagent.
pub async fn set_archived(
    harness: &Harness,
    parent: ConversationId,
    conversation_id: ConversationId,
    archived: bool,
    context: &Context,
) -> std::result::Result<(), BoxError> {
    if archived && let Some(conv) = harness.conversation(conversation_id, context).await? {
        conv.abort(false, context).await?;
    }
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(parent), None)?;
    harness
        .commit(
            async |tx| {
                let raw = tx.doc(&doc, &addr, None).await?;
                let mut state: SubagentState =
                    serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                if let Some(agent) = state
                    .agents
                    .values_mut()
                    .find(|a| a.conversation_id == conversation_id)
                {
                    if archived {
                        agent.archived = Some(true);
                    } else {
                        agent.archived = None;
                    }
                    raw.set(&[], JsonValue::from_serde(serde_json::to_value(&state)?))?;
                }
                Ok(())
            },
            context,
        )
        .await?;
    Ok(())
}

/// Adds newly-installed script-only tools to every subagent that has `codemode`.
pub async fn enable_script_tools(
    harness: &Harness,
    names: &[String],
    context: &Context,
) -> std::result::Result<(), BoxError> {
    let doc = subagents_doc();
    let addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let Some(raw) = harness.session.snapshot(&doc, &addr, context).await? else {
        return Ok(());
    };
    let state: SubagentState = serde_json::from_value(Value::Object((*raw).clone()))?;
    let mut lacking = Vec::new();
    for agent in state.agents.values() {
        if !agent.tools.iter().any(|t| t == CODEMODE_TOOL) {
            continue;
        }
        let agent_addr = resolve_address(&AGENT_DOC, Some(agent.conversation_id), None)?;
        if let Some(agent_raw) = harness
            .session
            .snapshot(&AGENT_DOC, &agent_addr, context)
            .await?
            && let Some(enabled) = agent_raw.get("tools").and_then(Value::as_array)
        {
            let existing: HashSet<&str> = enabled.iter().filter_map(Value::as_str).collect();
            if names.iter().any(|name| !existing.contains(name.as_str())) {
                lacking.push(agent.conversation_id);
            }
        }
    }
    if lacking.is_empty() {
        return Ok(());
    }
    harness
        .commit(
            async |tx| {
                for conversation_id in lacking {
                    let agent_addr = resolve_address(&AGENT_DOC, Some(conversation_id), None)?;
                    let raw = tx.doc(&AGENT_DOC, &agent_addr, None).await?;
                    let mut doc_map: Map<String, Value> =
                        serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                    if let Some(arr) = doc_map.get("tools").and_then(Value::as_array) {
                        let mut seen = HashSet::new();
                        let mut merged = Vec::new();
                        for v in arr {
                            if let Some(s) = v.as_str()
                                && seen.insert(s.to_owned())
                            {
                                merged.push(json!(s));
                            }
                        }
                        for name in names {
                            if seen.insert(name.clone()) {
                                merged.push(json!(name));
                            }
                        }
                        doc_map.insert("tools".into(), Value::Array(merged));
                        raw.set(&[], JsonValue::from_serde(Value::Object(doc_map)))?;
                    }
                }
                Ok(())
            },
            context,
        )
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentToolArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    follow_up: Option<bool>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking_level: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Option<Option<String>>,
    #[serde(default)]
    follow_session: Option<bool>,
}

fn subagent_reply(
    action: &str,
    name: Option<&str>,
    text: impl Into<String>,
    extra: Map<String, Value>,
    is_error: bool,
) -> ToolResult {
    let mut details = Map::new();
    details.insert("action".into(), json!(action));
    if let Some(n) = name {
        details.insert("name".into(), json!(n));
    }
    for (k, v) in extra {
        details.insert(k, v);
    }
    ToolResult {
        output: Some(vec![InputContent::Text(text.into().into())]),
        details: Some(Value::Object(details)),
        is_error: if is_error { Some(true) } else { None },
        ..Default::default()
    }
}

fn tool_choice_patch(choice: &ToolChoice) -> Map<String, Value> {
    let mut patch = Map::new();
    let tools: Vec<Value> = choice
        .tools
        .iter()
        .map(|t| json!(t.declaration.name))
        .collect();
    let loaded: Vec<Value> = choice
        .loaded
        .iter()
        .map(|t| json!(t.declaration.name))
        .collect();
    patch.insert("tools".into(), Value::Array(tools));
    patch.insert("modelTools".into(), json!({ "add": loaded }));
    patch
}

async fn is_conversation_busy(
    session: &Session,
    id: ConversationId,
    context: &Context,
) -> std::result::Result<bool, BoxError> {
    let addr = resolve_address(&LIVE_DOC, Some(id), None)?;
    Ok(session
        .snapshot(&LIVE_DOC, &addr, context)
        .await?
        .is_some_and(|live| live.get("run").is_some_and(|v| !v.is_null())))
}

/// Builds the `subagent-tools` extension (`subagent` tool + `Anchor` & `Reporter` tasks).
pub fn create_subagent_extension(
    models: Arc<dyn Fn() -> std::result::Result<Models, String> + Send + Sync>,
    locator: SubagentLocator,
    access: Arc<dyn Fn() -> IndexMap<String, ToolScope> + Send + Sync>,
) -> Extension {
    let parameters = json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["spawn", "send", "configure", "stop", "archive", "restore", "answer", "status", "models"]
            },
            "name": { "type": "string" },
            "message": { "type": "string" },
            "followUp": { "type": "boolean" },
            "model": { "type": "string" },
            "thinkingLevel": { "type": "string" },
            "tools": { "type": "array", "items": { "type": "string" } },
            "cwd": { "type": "string" },
            "env": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "followSession": { "type": "boolean" }
        },
        "required": ["action"],
        "additionalProperties": true
    });

    let description = [
        "Manage persistent subagents that work in the background while you keep talking to the user.",
        "Each subagent is its own conversation with its own memory, working in your cwd unless spawned with its own.",
        "Actions:",
        "- spawn (name, message; optional model, thinkingLevel, tools, env, cwd): start a subagent and send its first message.",
        "  Without env and cwd it works where you work and follows the session when it moves. With them it gets its own",
        "  location, which stays when the session moves: cwd is an existing directory its tools run in and the user's workspace",
        "  view shows, e.g. a git worktree you created first (relative to your cwd on your environment, to the home on another);",
        "  env is an environment name from location_envs, or null for this device (default: yours).",
        "  model is 'provider/modelId' (default: yours); thinkingLevel is off|minimal|low|medium|high|xhigh|max",
        "  (clamped to the model); tools names tools of yours it gets (default: all you can give; [] for none). Tools that",
        "  work only in your conversation (subagent, rename_session, …) cannot be given. With codemode its scripts call",
        "  its tools and the tools behind code mode (MCP tools, terminal, one-shot agents). With location_move it can",
        "  move itself.",
        "- send (name, message; followUp: true queues after the current answer instead of steering).",
        "- configure (name; optional model, thinkingLevel, tools, env, cwd, followSession): replace its model/thinking/tool",
        "  selection for later work; env/cwd give it its own location, followSession: true makes it follow the session",
        "  again. A location changes only while it is idle.",
        "- stop (name): abort its current work; it stays usable.",
        "- archive (name): stop it and hide it from the user's sidebar and from status; restore (name) or send brings it back.",
        "- answer (name): return its latest completed answer, e.g. when a report did not reach you or you need it again.",
        "- status (optional name): list subagents and whether they are working; archived ones only by name.",
        "- models (optional model: a provider or parts of a model id, e.g. 'anthropic' or 'sonnet 4'): list models that have",
        "  credentials configured, or only those matching model.",
        "Answers are reported back to you as messages when they arrive; do not wait or poll for them.",
        "Subagents can also message you while they work: a <sender kind=\"subagent\"> message marked [asks; …] needs your",
        "answer (send to that subagent); a note needs none.",
    ]
    .join("\n");

    let subagent_tool = ToolRegistration {
        declaration: Tool {
            name: "subagent".into(),
            description,
            parameters,
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: false,
        callers: Some(vec!["model".into()]),
        deferred: false,
        execute: Arc::new(move |raw_args, inv, ctx| {
            let models = models.clone();
            let locator = locator.clone();
            let access = access.clone();
            Box::pin(async move {
                let has_env = raw_args.get("env").is_some();
                let args: SubagentToolArgs = serde_json::from_value(raw_args)
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                let action = args.action.as_str();
                let name_opt = args.name.as_deref();
                let harness = inv.harness();
                let doc = subagents_doc();
                let doc_addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;

                let registry: SubagentState =
                    match harness.session.snapshot(&doc, &doc_addr, ctx).await? {
                        Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
                        None => SubagentState::default(),
                    };

                if action == "models" {
                    let models = models().map_err(DurableError::invalid)?;
                    let available: Vec<ListedModel> = models
                        .get_available(None, &Default::default())
                        .await
                        .into_iter()
                        .flatten()
                        .map(|m| ListedModel::from(&m))
                        .collect();
                    return Ok(subagent_reply(
                        action,
                        name_opt,
                        list_models(&available, args.model.as_deref()),
                        Map::new(),
                        false,
                    )
                    .into());
                }

                if action == "status" {
                    let all: Vec<(&String, &Subagent)> = registry.agents.iter().collect();
                    let listed: Vec<(&String, &Subagent)> = match name_opt {
                        None => all
                            .iter()
                            .copied()
                            .filter(|(_, a)| !a.archived.unwrap_or(false))
                            .collect(),
                        Some(n) => all.iter().copied().filter(|(k, _)| *k == n).collect(),
                    };
                    let mut lines = Vec::new();
                    for (each, agent) in &listed {
                        let busy =
                            is_conversation_busy(&harness.session, agent.conversation_id, ctx)
                                .await?;
                        let tools_str = if agent.tools.is_empty() {
                            "none".to_owned()
                        } else {
                            agent.tools.join(", ")
                        };
                        let loc_str = agent
                            .location
                            .as_ref()
                            .map(|l| format!(" · in {}", location_text(l)))
                            .unwrap_or_default();
                        let archived_str = if agent.archived.unwrap_or(false) {
                            " · archived"
                        } else {
                            ""
                        };
                        lines.push(format!(
                            "{each}: {}{archived_str} · {} · thinking {} · tools {tools_str}{loc_str}",
                            if busy { "working" } else { "idle" },
                            agent.model,
                            agent.thinking_level
                        ));
                    }
                    let archived = if name_opt.is_none() {
                        all.len().saturating_sub(listed.len())
                    } else {
                        0
                    };
                    if archived > 0 {
                        lines.push(format!(
                            "{archived} archived (status with a name shows one)."
                        ));
                    }
                    let text = if lines.is_empty() {
                        "No subagents.".into()
                    } else {
                        lines.join("\n")
                    };
                    return Ok(subagent_reply(action, name_opt, text, Map::new(), false).into());
                }

                let Some(name) = name_opt.filter(|n| !n.trim().is_empty()) else {
                    return Ok(subagent_reply(
                        action,
                        name_opt,
                        format!("{action} needs a name."),
                        Map::new(),
                        true,
                    )
                    .into());
                };
                let agent = registry.agents.get(name);
                if action != "spawn" && agent.is_none() {
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        format!("No subagent named {name}."),
                        Map::new(),
                        true,
                    )
                    .into());
                }

                if action == "stop"
                    && let Some(agent) = agent
                {
                    if let Some(conv) = harness.conversation(agent.conversation_id, ctx).await? {
                        conv.abort(false, ctx).await?;
                    }
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        format!("Stopped {name}."),
                        Map::from_iter([("conversationId".into(), json!(agent.conversation_id))]),
                        false,
                    )
                    .into());
                }

                if (action == "archive" || action == "restore")
                    && let Some(agent) = agent
                {
                    let archiving = action == "archive";
                    set_archived(
                        harness,
                        ROOT_CONVERSATION_ID,
                        agent.conversation_id,
                        archiving,
                        ctx,
                    )
                    .await?;
                    let text = if archiving {
                        format!("Stopped and archived {name}.")
                    } else {
                        format!("Restored {name}.")
                    };
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        text,
                        Map::from_iter([("conversationId".into(), json!(agent.conversation_id))]),
                        false,
                    )
                    .into());
                }

                if action == "answer"
                    && let Some(agent) = agent
                {
                    let answer =
                        latest_answer(&harness.session, agent.conversation_id, ctx).await?;
                    let busy =
                        is_conversation_busy(&harness.session, agent.conversation_id, ctx).await?;
                    let state = if busy {
                        format!("{name} is still working.")
                    } else {
                        String::new()
                    };
                    let text = match answer {
                        None => {
                            if state.is_empty() {
                                format!("{name} has not answered yet.")
                            } else {
                                format!("{name} has not answered yet. {state}")
                            }
                        }
                        Some(ans) => {
                            if state.is_empty() {
                                ans
                            } else {
                                format!("{state} Its latest answer:\n\n{ans}")
                            }
                        }
                    };
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        text,
                        Map::from_iter([("conversationId".into(), json!(agent.conversation_id))]),
                        false,
                    )
                    .into());
                }

                if action != "configure"
                    && args.message.as_deref().is_none_or(|m| m.trim().is_empty())
                {
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        format!("{action} needs a message."),
                        Map::new(),
                        true,
                    )
                    .into());
                }

                let placing = args.cwd.is_some() || has_env;
                if action == "configure"
                    && args.model.is_none()
                    && args.thinking_level.is_none()
                    && args.tools.is_none()
                    && !placing
                    && !args.follow_session.unwrap_or(false)
                {
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        "configure needs model, thinkingLevel, tools, env, cwd or followSession.",
                        Map::new(),
                        true,
                    )
                    .into());
                }
                if placing && action != "spawn" && action != "configure" {
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        "Only spawn and configure take a location.",
                        Map::new(),
                        true,
                    )
                    .into());
                }
                if args.follow_session.unwrap_or(false) && (action != "configure" || placing) {
                    return Ok(subagent_reply(
                        action,
                        Some(name),
                        "followSession is for configure, without env or cwd.",
                        Map::new(),
                        true,
                    )
                    .into());
                }

                let caller_conv = harness
                    .conversation(inv.record.conversation_id, ctx)
                    .await?
                    .ok_or_else(|| DurableError::invalid("Caller conversation missing"))?;
                let own_agent = caller_conv.agent(ctx).await?;
                let tool_access = access();

                let mut resolved_model: Option<(String, String)> = None;
                let mut resolved_thinking: Option<String> = None;
                let mut resolved_tools: Option<ToolChoice> = None;
                // None = unchanged; Some(None) = follow session; Some(Some(loc)) = own location
                let mut target_location: Option<Option<Location>> = None;
                let mut narrowed: Option<ToolChoice> = None;

                if action == "spawn" || action == "configure" {
                    if action == "spawn"
                        && let Some(existing) = agent
                    {
                        let suffix = if existing.archived.unwrap_or(false) {
                            " (archived)"
                        } else {
                            ""
                        };
                        return Ok(subagent_reply(
                            action,
                            Some(name),
                            format!("{name} already exists{suffix}; use send."),
                            Map::new(),
                            true,
                        )
                        .into());
                    }
                    let models = models().map_err(DurableError::invalid)?;
                    let choice_input = AgentChoice {
                        model: args.model.clone(),
                        thinking_level: args.thinking_level.clone(),
                        tools: args.tools.clone(),
                    };
                    let existing = if action == "spawn" { None } else { agent };
                    let choice = match resolve_choice(
                        &models,
                        &own_agent,
                        &tool_access,
                        &choice_input,
                        existing,
                        AgentKind::Subagent,
                        ctx,
                    )
                    .await
                    {
                        Ok(c) => c,
                        Err(msg) => {
                            return Ok(
                                subagent_reply(action, Some(name), msg, Map::new(), true).into()
                            );
                        }
                    };
                    resolved_model =
                        Some((choice.model.provider.clone(), choice.model.model_id.clone()));
                    resolved_thinking = Some(thinking_level_str(choice.thinking_level).into());
                    resolved_tools = Some(choice.tools);

                    if let Some(existing) = agent
                        && (placing || args.follow_session.unwrap_or(false))
                        && is_conversation_busy(&harness.session, existing.conversation_id, ctx)
                            .await?
                    {
                        return Ok(subagent_reply(
                            action,
                            Some(name),
                            format!(
                                "{name} is working: stop it or wait before changing its location."
                            ),
                            Map::new(),
                            true,
                        )
                        .into());
                    }
                    if args.follow_session.unwrap_or(false) {
                        target_location = Some(None);
                    } else if placing {
                        let caller_loc =
                            (locator.locate)(inv.record.conversation_id, ctx.clone()).await?;
                        let kept = match (agent.and_then(|a| a.location.as_ref()), has_env) {
                            (Some(loc), false) => (locator.sandbox)(loc.clone()).await?,
                            _ => None,
                        };
                        let env_arg: Option<Option<&str>> = if let Some(ref sb) = kept {
                            Some(Some(sb.as_str()))
                        } else if has_env {
                            Some(args.env.as_ref().and_then(|e| e.as_deref()))
                        } else {
                            None
                        };
                        match place_target(&locator, &caller_loc, env_arg, args.cwd.as_deref(), ctx)
                            .await
                        {
                            Ok(loc) => target_location = Some(Some(loc)),
                            Err(msg) => {
                                return Ok(subagent_reply(
                                    action,
                                    Some(name),
                                    msg,
                                    Map::new(),
                                    true,
                                )
                                .into());
                            }
                        }
                    }

                    if let Some(existing_loc) = agent.and_then(|a| a.location.as_ref())
                        && let Some(sandbox) = (locator.sandbox)(existing_loc.clone()).await?
                        && let Some(ref next_opt) = target_location
                    {
                        let next_env = match next_opt {
                            Some(loc) => loc.env.clone(),
                            None => {
                                (locator.locate)(ROOT_CONVERSATION_ID, ctx.clone())
                                    .await?
                                    .env
                            }
                        };
                        if next_env.as_deref() != Some(sandbox.as_str()) {
                            return Ok(subagent_reply(
                                action,
                                Some(name),
                                format!("{name} works in sandbox {sandbox} and cannot leave it."),
                                Map::new(),
                                true,
                            )
                            .into());
                        }
                    }
                } else if let Some(existing) = agent
                    && let Some(kept) = still_given(&own_agent, &tool_access, Some(existing))
                    && kept.len() != existing.tools.len()
                    && let Ok(res) = resolve_tools(
                        &own_agent,
                        &tool_access,
                        Some(&kept),
                        &[],
                        AgentKind::Subagent,
                    )
                {
                    narrowed = Some(res);
                }

                let session_loc = if matches!(target_location, Some(None)) {
                    Some((locator.locate)(ROOT_CONVERSATION_ID, ctx.clone()).await?)
                } else {
                    None
                };

                let action_owned = action.to_owned();
                let name_owned = name.to_owned();
                let message_owned = args.message.clone().unwrap_or_default();
                let follow_up = action == "send" && args.follow_up.unwrap_or(false);
                let caller_conv_id = inv.record.conversation_id;
                let caller_task_id = inv.record.id;

                let result_text = harness
                    .commit(
                        async move |tx| {
                            let mut state: SubagentState = {
                                let raw = tx.doc(&doc, &doc_addr, None).await?;
                                serde_json::from_value(raw.value()?.to_json().to_serde()?)?
                            };

                            if action_owned == "configure" {
                                let Some(target) = state.agents.get_mut(&name_owned) else {
                                    return Ok(format!("No subagent named {name_owned}."));
                                };
                                let mut patch = Map::new();
                                if let Some((ref prov, ref mid)) = resolved_model {
                                    patch.insert(
                                        "model".into(),
                                        json!({ "provider": prov, "modelId": mid }),
                                    );
                                    target.model = format!("{prov}/{mid}");
                                }
                                if let Some(ref tl) = resolved_thinking {
                                    patch.insert("thinkingLevel".into(), json!(tl));
                                    target.thinking_level = tl.clone();
                                }
                                if let Some(ref tc) = resolved_tools {
                                    for (k, v) in tool_choice_patch(tc) {
                                        patch.insert(k, v);
                                    }
                                    target.tools = tc.chosen.clone();
                                }
                                match &target_location {
                                    Some(Some(loc)) => {
                                        patch.insert("cwd".into(), json!(loc.cwd));
                                        target.location = Some(loc.clone());
                                    }
                                    Some(None) => {
                                        if let Some(ref sl) = session_loc {
                                            patch.insert("cwd".into(), json!(sl.cwd));
                                        }
                                        target.location = None;
                                    }
                                    None => {}
                                }
                                let conv_id = target.conversation_id;
                                configure(tx, conv_id, &patch).await?;
                                tx.doc(&doc, &doc_addr, None)
                                    .await?
                                    .set(&[], JsonValue::from_serde(serde_json::to_value(&state)?))?;
                                return Ok(format!("Configured {name_owned}."));
                            }

                            if action_owned == "spawn" {
                                if state.agents.contains_key(&name_owned) {
                                    return Ok(format!("{name_owned} already exists; use send."));
                                }
                                let anchor = tx
                                    .create_task(
                                        &anchor_task(),
                                        Value::Null,
                                        TaskOptions {
                                            background: true,
                                            ..TaskOptions::new(TaskOwnership::Conversation)
                                        },
                                    )
                                    .await?;
                                let child = tx
                                    .create_conversation(ConversationOwnership::Task {
                                        task_id: anchor,
                                    })
                                    .await?
                                    .id;
                                let mut patch = Map::new();
                                let (prov, mid) = resolved_model.clone().unwrap_or_default();
                                patch.insert(
                                    "model".into(),
                                    json!({ "provider": prov, "modelId": mid }),
                                );
                                let tl = resolved_thinking
                                    .clone()
                                    .unwrap_or_else(|| "off".to_owned());
                                patch.insert("thinkingLevel".into(), json!(tl));
                                let tc = resolved_tools.take().unwrap_or(ToolChoice {
                                    chosen: Vec::new(),
                                    tools: Vec::new(),
                                    loaded: Vec::new(),
                                });
                                for (k, v) in tool_choice_patch(&tc) {
                                    patch.insert(k, v);
                                }
                                let own_loc = match target_location {
                                    Some(Some(loc)) => {
                                        patch.insert("cwd".into(), json!(loc.cwd));
                                        Some(loc)
                                    }
                                    _ => None,
                                };
                                patch.insert(
                                    "instructions".into(),
                                    json!(format!(
                                        "You are the subagent \"{name_owned}\", working for a main agent. Do the requested work and answer concisely; your final answer is reported back to it. With the team tool you can ask the main agent a question or tell it something while you keep working, and message the other subagents."
                                    )),
                                );
                                configure(tx, child, &patch).await?;
                                state.agents.insert(
                                    name_owned.clone(),
                                    Subagent {
                                        conversation_id: child,
                                        model: format!("{prov}/{mid}"),
                                        thinking_level: tl,
                                        tools: tc.chosen,
                                        location: own_loc,
                                        unplaced_cwd: None,
                                        parent: None,
                                        reported: Vec::new(),
                                        archived: None,
                                    },
                                );
                            }

                            let Some(target) = state.agents.get_mut(&name_owned) else {
                                return Ok(format!("No subagent named {name_owned}."));
                            };
                            target.archived = None;
                            target.parent = Some(caller_conv_id);
                            let conv_id = target.conversation_id;
                            if let Some(ref tc) = narrowed {
                                let patch = tool_choice_patch(tc);
                                configure(tx, conv_id, &patch).await?;
                                target.tools = tc.chosen.clone();
                            }
                            let reporter_input = ReporterInput {
                                name: name_owned.clone(),
                                conversation_id: conv_id,
                                parent: Some(caller_conv_id),
                                message: message_owned,
                                follow_up,
                            };
                            let reporter_id = tx
                                .create_task(
                                    &reporter_task(),
                                    serde_json::to_value(reporter_input)?,
                                    TaskOptions {
                                        background: true,
                                        ..TaskOptions::new(TaskOwnership::Conversation)
                                    },
                                )
                                .await?;
                            state
                                .reporters
                                .insert(caller_task_id.to_string(), reporter_id);
                            tx.doc(&doc, &doc_addr, None)
                                .await?
                                .set(&[], JsonValue::from_serde(serde_json::to_value(&state)?))?;
                            Ok(if action_owned == "send" {
                                format!("Sent to {name_owned}.")
                            } else {
                                format!("Started {name_owned}.")
                            })
                        },
                        ctx,
                    )
                    .await?;

                let snap_doc = subagents_doc();
                let snap_addr = resolve_address(&snap_doc, Some(ROOT_CONVERSATION_ID), None)?;
                let updated: Option<Subagent> = harness
                    .session
                    .snapshot(&snap_doc, &snap_addr, ctx)
                    .await?
                    .and_then(|raw| {
                        serde_json::from_value::<SubagentState>(Value::Object((*raw).clone())).ok()
                    })
                    .and_then(|s| s.agents.get(name).cloned());

                let mut extra = Map::new();
                if let Some(current) = updated {
                    extra.insert("conversationId".into(), json!(current.conversation_id));
                    extra.insert("model".into(), json!(current.model));
                    extra.insert("thinkingLevel".into(), json!(current.thinking_level));
                    extra.insert("tools".into(), json!(current.tools));
                    if let Some(loc) = current.location {
                        extra.insert("cwd".into(), json!(loc.cwd));
                        if let Some(env) = loc.env {
                            extra.insert("env".into(), json!(env));
                        }
                    }
                }
                Ok(subagent_reply(action, Some(name), result_text, extra, false).into())
            })
        }),
    };

    Extension {
        name: SUBAGENT_EXTENSION.into(),
        tasks: vec![anchor_task(), reporter_task()],
        tools: vec![subagent_tool],
        ..Default::default()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamToolArgs {
    action: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    intent: Option<AgentMessageIntent>,
    #[serde(default)]
    steer: Option<bool>,
}

fn team_reply(
    action: &str,
    to: Option<&str>,
    text: impl Into<String>,
    is_error: bool,
) -> ToolResult {
    let mut details = Map::new();
    details.insert("action".into(), json!(action));
    if let Some(t) = to {
        details.insert("to".into(), json!(t));
    }
    ToolResult {
        output: Some(vec![InputContent::Text(text.into().into())]),
        details: Some(Value::Object(details)),
        is_error: if is_error { Some(true) } else { None },
        ..Default::default()
    }
}

/// Builds the `p2-subagent-team` extension (`team` tool for subagent-to-parent/sibling messaging).
pub fn team_extension() -> Extension {
    let parameters = json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["send", "answer", "list"] },
            "to": { "type": "string", "minLength": 1, "maxLength": 200 },
            "name": { "type": "string", "minLength": 1, "maxLength": 200 },
            "message": { "type": "string", "minLength": 1, "maxLength": 32768 },
            "intent": { "type": "string", "enum": ["question", "note"] },
            "steer": { "type": "boolean" }
        },
        "required": ["action"],
        "additionalProperties": true
    });
    let description = [
        "Message the main agent or the other subagents while you work, without ending your turn.",
        "- send (to: \"parent\" or a subagent name, message; optional intent \"question\" (default) or \"note\", steer):",
        "  delivers it as a message from you; it starts the receiver's turn if it is idle, else waits for its next step",
        "  (steer: true interrupts its current step). A question asks for a reply, which arrives as a message to you; a note needs none.",
        "- answer (name): the latest completed answer of another subagent.",
        "- list: the other subagents and whether they are working.",
        "Only message when it helps the work. Never send acknowledgements or thanks, and do not reply to notes.",
    ]
    .join("\n");

    let team_tool = ToolRegistration {
        declaration: Tool {
            name: TEAM_TOOL.into(),
            description,
            parameters,
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: Some(vec!["model".into()]),
        deferred: true,
        execute: Arc::new(|raw_args, inv, ctx| {
            Box::pin(async move {
                let args: TeamToolArgs = serde_json::from_value(raw_args)
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                let action = args.action.as_str();
                let to_opt = args.to.as_deref();
                let harness = inv.harness();
                let doc = subagents_doc();
                let doc_addr = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
                let state: SubagentState =
                    match harness.session.snapshot(&doc, &doc_addr, ctx).await? {
                        Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
                        None => SubagentState::default(),
                    };
                let Some((self_name, self_agent)) = state
                    .agents
                    .iter()
                    .find(|(_, a)| a.conversation_id == inv.record.conversation_id)
                else {
                    return Ok(team_reply(
                        action,
                        to_opt,
                        "Only subagents can use the team tool.",
                        true,
                    )
                    .into());
                };

                let siblings: Vec<(&String, &Subagent)> = state
                    .agents
                    .iter()
                    .filter(|(n, _)| *n != self_name)
                    .collect();

                if action == "list" {
                    let mut lines = Vec::new();
                    for (name, agent) in siblings {
                        let busy =
                            is_conversation_busy(&harness.session, agent.conversation_id, ctx)
                                .await?;
                        let archived = if agent.archived.unwrap_or(false) {
                            " · archived"
                        } else {
                            ""
                        };
                        lines.push(format!(
                            "{name}: {}{archived}",
                            if busy { "working" } else { "idle" }
                        ));
                    }
                    let text = if lines.is_empty() {
                        "No other subagents.".into()
                    } else {
                        lines.join("\n")
                    };
                    return Ok(team_reply(action, to_opt, text, false).into());
                }

                if action == "answer" {
                    let Some(target_name) = args.name.as_deref() else {
                        return Ok(team_reply(
                            action,
                            to_opt,
                            "No other subagent named (none).",
                            true,
                        )
                        .into());
                    };
                    let Some((_, sibling)) = siblings.iter().find(|(n, _)| **n == target_name)
                    else {
                        return Ok(team_reply(
                            action,
                            to_opt,
                            format!("No other subagent named {target_name}."),
                            true,
                        )
                        .into());
                    };
                    let answer =
                        latest_answer(&harness.session, sibling.conversation_id, ctx).await?;
                    let text =
                        answer.unwrap_or_else(|| format!("{target_name} has not answered yet."));
                    return Ok(team_reply(action, to_opt, text, false).into());
                }

                let Some(to) = to_opt else {
                    return Ok(team_reply(
                        action,
                        to_opt,
                        "send needs to (\"parent\" or a subagent name).",
                        true,
                    )
                    .into());
                };
                let Some(message_text) = args.message.as_deref().filter(|m| !m.trim().is_empty())
                else {
                    return Ok(team_reply(action, Some(to), "send needs a message.", true).into());
                };
                let sibling = if to == "parent" {
                    None
                } else {
                    match siblings.iter().find(|(n, _)| **n == to) {
                        Some((_, s)) => Some(*s),
                        None => {
                            return Ok(team_reply(
                                action,
                                Some(to),
                                format!("No other subagent named {to}."),
                                true,
                            )
                            .into());
                        }
                    }
                };
                let target_id = match sibling {
                    Some(s) => s.conversation_id,
                    None => match self_agent.parent {
                        Some(p) => p,
                        None => main_conversation(&harness.session, ctx).await?,
                    },
                };
                let Some(target) = harness.conversation(target_id, ctx).await? else {
                    return Ok(team_reply(
                        action,
                        Some(to),
                        format!("{to} has no conversation."),
                        true,
                    )
                    .into());
                };
                let msg = AgentMessage {
                    from: AgentMessageFrom {
                        name: self_name.clone(),
                        conversation_id: inv.record.conversation_id,
                    },
                    intent: args.intent.unwrap_or(AgentMessageIntent::Question),
                    text: message_text.to_owned(),
                };
                let reply_with = if sibling.is_some() {
                    format!("team send to {self_name}")
                } else {
                    format!("subagent send to {self_name}")
                };
                let (content, entry) = agent_message_input(&msg, &reply_with)?;
                let call_id = inv
                    .record
                    .input
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                target
                    .submit(
                        SubmissionDraft {
                            request_id: Some(format!("team:{}:{call_id}", inv.record.id)),
                            notify: None,
                            body: SubmissionBody::Input {
                                content,
                                entry: Some(entry),
                                when_busy: Some(
                                    if args.steer.unwrap_or(false) {
                                        "steer"
                                    } else {
                                        "followUp"
                                    }
                                    .into(),
                                ),
                                when_idle: None,
                                on_abort: Some("keep".into()),
                            },
                        },
                        ctx,
                    )
                    .await?;
                Ok(team_reply(action, Some(to), format!("Sent to {to}."), false).into())
            })
        }),
    };

    Extension {
        name: "p2-subagent-team".into(),
        tools: vec![team_tool],
        ..Default::default()
    }
}
