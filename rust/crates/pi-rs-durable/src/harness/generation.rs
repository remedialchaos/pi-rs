use super::{LIVE_DOC, UsageBucket, record_usage};
use crate::{
    ConversationId, EntryDraft, EntryRecord,
    documents::resolve_address,
    session::{Result, Transaction},
};
use pi_rs_ai::types::AssistantMessage;
use serde_json::Value;

pub async fn append_assistant(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    message: AssistantMessage,
) -> Result<EntryRecord> {
    let key = format!("{}/{}", message.provider, message.model);
    record_usage(tx, conversation, UsageBucket::Models, &key, &message.usage).await?;
    tx.append_entry(
        conversation,
        EntryDraft {
            kind: "pi.assistant".into(),
            model: Some(vec![serde_json::to_value(message)?]),
            ..Default::default()
        },
    )
    .await
}

/// Preserve the last committed response when recovery interrupts a request.
pub async fn convert_partial(tx: &mut Transaction<'_>, conversation: ConversationId) -> Result<()> {
    let address = resolve_address(&LIVE_DOC, Some(conversation), None)?;
    let state = serde_json::to_value(tx.doc(&LIVE_DOC, &address, None).await?.value()?.to_json())?;
    if let Some(partial) = state["generation"].get("message") {
        let mut message = partial.clone();
        message["stopReason"] = Value::from("aborted");
        append_assistant(tx, conversation, serde_json::from_value(message)?).await?;
    }
    Ok(())
}

use super::{Invocation, PartialChanges};
use crate::session::{CommitOptions, CommitPersistence, Error};
use futures_util::StreamExt;
use pi_rs_ai::{
    model::Model,
    registry::Models,
    request::StreamOptions,
    types::{AssistantMessageEvent as Event, FinalMessage, PartialMessage},
};
use pi_rs_chord::{context::Context, delta::Segment};
use std::time::Duration;
use tokio::time::Instant;

pub(super) fn stream_options(
    checkpoint: &Value,
    session_id: String,
    ctx: &Context,
) -> Result<StreamOptions> {
    let value = &checkpoint["streamOptions"];
    let mut opts = StreamOptions {
        session_id: Some(session_id),
        telemetry: super::telemetry::active_context(ctx),
        transport: serde_json::from_value(value["transport"].clone())?,
        cache_retention: serde_json::from_value(value["cacheRetention"].clone())?,
        websocket_connect_timeout_ms: value["websocketConnectTimeoutMs"].as_u64(),
        reasoning: serde_json::from_value(checkpoint["thinkingLevel"].clone())?,
        deferred: value.get("deferred").cloned(),
        ..Default::default()
    };
    if opts.reasoning == Some(pi_rs_ai::model::ThinkingLevel::Off) {
        opts.reasoning = None;
    }
    if let Some(v) = value.get("headers") {
        opts.request.headers = serde_json::from_value(v.clone())?;
    }
    if let Some(v) = value.get("metadata") {
        opts.metadata = serde_json::from_value(v.clone())?;
    }
    if let Some(v) = value.get("thinkingBudgets") {
        opts.thinking_budgets = serde_json::from_value(v.clone())?;
    }
    opts.request.timeout_ms = value["timeoutMs"].as_u64();
    opts.request.max_retries = serde_json::from_value(value["maxRetries"].clone())?;
    opts.request.max_retry_delay_ms = value["maxRetryDelayMs"].as_u64();
    Ok(opts)
}

pub struct GenerationRequest {
    pub model: Model,
    pub context: pi_rs_ai::types::Context,
    pub options: StreamOptions,
    pub attempt: u64,
    pub partial_interval: Duration,
}

impl Invocation {
    pub async fn stream_response(
        &self,
        models: &Models,
        request: GenerationRequest,
        ctx: &Context,
    ) -> Result<FinalMessage> {
        let token = request.options.request.signal.clone();
        let _on_cancel = ctx
            .abort_signal()
            .map(|signal| signal.subscribe(move |_| token.cancel()));
        let mut events = models.stream_simple(request.model, request.context, request.options);
        let mut changes = PartialChanges::default();
        let mut pending: Option<PartialMessage> = None;
        let mut deadline = Instant::now();
        let address = resolve_address(&LIVE_DOC, Some(self.record.conversation_id), None)?;
        loop {
            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else { break };
                    let partial = match &event {
                        Event::Start { partial }
                        | Event::TextStart { partial, .. }
                        | Event::TextDelta { partial, .. }
                        | Event::TextEnd { partial, .. }
                        | Event::ThinkingStart { partial, .. }
                        | Event::ThinkingDelta { partial, .. }
                        | Event::ThinkingEnd { partial, .. }
                        | Event::ToolCallStart { partial, .. }
                        | Event::ToolCallDelta { partial, .. }
                        | Event::ToolCallEnd { partial, .. } => partial,
                        Event::Done { .. } | Event::Error { .. } => break,
                    };
                    if partial.read().unwrap_or_else(|e| e.into_inner()).content.is_empty() {
                        continue;
                    }
                    changes.observe(&event);
                    if pending.is_none() {
                        deadline = Instant::now() + request.partial_interval;
                    }
                    pending = Some(partial.clone());
                }
                () = tokio::time::sleep_until(deadline), if pending.is_some() => {
                    let Some(partial) = pending.take() else { continue };
                    let mut message = serde_json::to_value(&*partial.read().unwrap_or_else(|e| e.into_inner()))?;
                    let taken = changes.take(&mut message);
                    self.harness.session.commit_with(async |tx|{
                        let current = tx.task(self.record.id).await?;
                        if current.is_none_or(|r| r.state != self.record.state || r.abort_requested) {
                            return Err(Error::invalid("Generation invocation ended"));
                        }
                        let live = tx.doc(&LIVE_DOC, &address, None).await?;
                        if live
                            .value()?
                            .at(&[Segment::Key("generation".into())])
                            .is_err()
                        {
                            live.set(
                                &[Segment::Key("generation".into())],
                                serde_json::json!({"attempt":request.attempt}).into(),
                            )?;
                        }
                        changes.commit(live, &mut message, &taken)
                    },ctx,Default::default(),CommitOptions{persistence:CommitPersistence::Deferred{within:None}}).await?;
                }
            }
        }
        ctx.check()?;
        events
            .result()
            .await
            .map_err(|e| Error::Callback(Box::new(e)))
    }
}

use super::{RunEnd, apply_boundary, end_run, prepare_boundary};
use crate::{
    EntryId, SubmissionId, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskState,
    session::TaskOptions,
    tasks::{MigratedTask, PhaseHandler, Task, TaskDefinition},
};
use serde_json::json;
use std::sync::Arc;

pub fn generation_task() -> Arc<Task<Invocation>> {
    let phase: PhaseHandler<Invocation> = Arc::new(|_, runtime, ctx| {
        Box::pin(async move { runtime.generate(ctx).await.map_err(Into::into) })
    });
    Arc::new(crate::define_task(TaskDefinition {
        name: "pi.generation".into(),
        version: 3,
        initial: Arc::new(|_| Ok(json!({"phase":"prepare","attempt":1}))),
        phases: [
            "prepare",
            "request",
            "poll",
            "retry",
            "tools",
            "afterTools",
            "yield",
            "finish",
            "drain",
        ]
        .into_iter()
        .map(|p| (p.into(), phase.clone()))
        .collect(),
        abort: Arc::new(|_, inv, ctx| {
            Box::pin(async move {
                let TaskState::Running { checkpoint, .. } = &inv.record.state else {
                    return Err(Error::invalid("Generation abort requires running state").into());
                };
                let phase = checkpoint["phase"].as_str();
                inv.cancel_deferred_checkpoint(checkpoint, ctx).await?;
                inv.commit(ctx, async |tx, record| {
                    let address = resolve_address(&LIVE_DOC, Some(record.conversation_id), None)?;
                    let live = serde_json::to_value(
                        tx.doc(&LIVE_DOC, &address, None).await?.value()?.to_json(),
                    )?;
                    convert_partial(tx, record.conversation_id).await?;
                    if phase == Some("tools") {
                        for call_id in checkpoint["pending"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                        {
                            if let Some(slot) = live["tools"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .find(|s| s["callId"] == call_id)
                                && let Some(name) = slot["name"].as_str()
                            {
                                let result = if slot["unoffered"] == true {
                                    super::harness_error(
                                        "tool_unavailable",
                                        &format!("Tool {name} is not available"),
                                    )
                                } else {
                                    super::harness_error(
                                        "aborted",
                                        &format!("Tool {name} was aborted"),
                                    )
                                };
                                super::append_tool_result(
                                    tx,
                                    record.conversation_id,
                                    call_id,
                                    name,
                                    result,
                                    (inv.harness.now)(),
                                    None,
                                )
                                .await?;
                            }
                        }
                    }
                    let end = if phase == Some("yield")
                        && let Some(answer) = checkpoint["answer"].as_u64()
                    {
                        RunEnd::Done { answer }
                    } else {
                        RunEnd::Unanswered {
                            reason: "aborted".into(),
                            detail: None,
                            settlement: None,
                        }
                    };
                    end_run(tx, record.conversation_id, record.id, end).await?;
                    Ok(TaskState::Terminal {
                        outcome: TaskOutcome::Aborted {
                            reason: None,
                            result: None,
                        },
                    })
                })
                .await
                .map_err(Into::into)
            })
        }),
        migrate: Some(Arc::new(|input, checkpoint, _| {
            Ok(MigratedTask { input, checkpoint })
        })),
        hooks: (),
    }))
}

pub(super) async fn start_run(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    inputs: Vec<SubmissionId>,
    first: Option<EntryId>,
) -> Result<()> {
    start_run_with_failure(tx, conversation, inputs, first, false).await
}

async fn start_run_with_failure(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    inputs: Vec<SubmissionId>,
    first: Option<EntryId>,
    keep_failure: bool,
) -> Result<()> {
    let mut options = TaskOptions::new(TaskOwnership::Conversation);
    options.conversation_id = Some(conversation);
    options.submission_ids = Some(inputs.clone());
    let task_id = tx
        .create_task(generation_task().as_ref(), json!({}), options)
        .await?;
    let mut run = json!({"taskId":task_id,"inputs":inputs,"turns":0});
    if let Some(first) = first {
        run["first"] = json!(first);
    }
    let address = resolve_address(&LIVE_DOC, Some(conversation), None)?;
    let live = tx.doc(&LIVE_DOC, &address, None).await?;
    live.set(&[Segment::Key("run".into())], run.into())?;
    if !keep_failure
        && live
            .value()?
            .at(&[Segment::Key("runFailure".into())])
            .is_ok()
    {
        live.delete(&[Segment::Key("runFailure".into())])?;
    }
    Ok(())
}

impl Invocation {
    async fn cancel_deferred_checkpoint(&self, checkpoint: &Value, ctx: &Context) -> Result<()> {
        let poll = if checkpoint["phase"] == "poll" {
            Some(checkpoint)
        } else {
            checkpoint.get("poll")
        };
        if let Some(poll) = poll
            && let Some((provider, model)) = poll["model"]["provider"]
                .as_str()
                .zip(poll["model"]["modelId"].as_str())
            && let Some(model) = self.harness.models.get_model(provider, model)
            && let Ok(handle) = serde_json::from_value(poll["handle"].clone())
        {
            let options = StreamOptions {
                telemetry: super::telemetry::active_context(ctx),
                ..Default::default()
            };
            let _ = self
                .harness
                .models
                .cancel_deferred(model, handle, options)
                .await;
            ctx.check().map_err(Error::from)?;
        }
        Ok(())
    }

    async fn interrupted_failure(
        &self,
        tx: &mut Transaction<'_>,
        settings: &Value,
        reason: &str,
        text: &str,
        salvage: bool,
    ) -> Result<TaskState> {
        let conversation = self.record.conversation_id;
        let boundary = if settings["afterFailedRun"] == "continue" {
            Some(
                prepare_boundary(
                    tx,
                    conversation,
                    settings["steeringMode"] == "all",
                    settings["followUpMode"] == "all",
                )
                .await?,
            )
        } else {
            None
        };
        if salvage {
            convert_partial(tx, conversation).await?;
        }
        self.generation_failure(tx, conversation, boundary, settings, reason, text)
            .await
    }

    async fn generation_failure(
        &self,
        tx: &mut Transaction<'_>,
        conversation: ConversationId,
        boundary: Option<super::Boundary>,
        settings: &Value,
        reason: &str,
        text: &str,
    ) -> Result<TaskState> {
        let id = self.record.id;
        let policy = settings["afterFailedRun"].as_str().unwrap_or("keep");
        let boundary = match (policy, boundary) {
            ("continue", Some(b)) => Some(b),
            ("continue", None) => Some(
                prepare_boundary(
                    tx,
                    conversation,
                    settings["steeringMode"] == "all",
                    settings["followUpMode"] == "all",
                )
                .await?,
            ),
            _ => None,
        };
        let settlement = if reason == "no_model" {
            Some(crate::SubmissionSettlement::Unanswered {
                reason: reason.into(),
                detail: None,
            })
        } else {
            None
        };
        end_run(
            tx,
            conversation,
            id,
            RunEnd::Unanswered {
                reason: reason.into(),
                detail: (reason != "nothing_to_answer").then(|| text.to_owned()),
                settlement,
            },
        )
        .await?;
        if policy == "withdraw" {
            super::withdraw_inputs(tx, conversation, "run_failed", false).await?;
        }
        if let Some(boundary) = boundary {
            let placed = apply_boundary(tx, boundary, true, (self.harness.now)()).await?;
            if !placed.users.is_empty() {
                start_run_with_failure(tx, conversation, placed.users, placed.first, true).await?;
            }
        }
        Ok(TaskState::Terminal {
            outcome: TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: text.into(),
                    detail: Some(json!({"reason":reason})),
                },
                result: None,
            },
        })
    }

    async fn generate(&self, ctx: &Context) -> Result<()> {
        use pi_rs_ai::types::StopReason;
        let TaskState::Running { checkpoint, .. } = &self.record.state else {
            return Err(Error::invalid("Generation must be running"));
        };
        let conversation = self.harness.handle(self.record.conversation_id);
        let id = self.record.id;
        let attempt = checkpoint["attempt"].as_u64().unwrap_or(1);
        let phase = checkpoint["phase"].as_str().unwrap_or("");
        let now = || (self.harness.now)();
        let settings = (self.harness.settings)();
        if let Some((frequency, salvage)) = self.restart_cutoff() {
            if salvage {
                self.cancel_deferred_checkpoint(checkpoint, ctx).await?;
            }
            let text = format!("The generation was interrupted {frequency} in its {phase} phase");
            return self
                .commit(ctx, async |tx, _| {
                    if matches!(phase, "finish" | "drain" | "yield")
                        && let Some(answer) = checkpoint["answer"]
                            .as_u64()
                            .or_else(|| checkpoint["run"]["answer"].as_u64())
                    {
                        end_run(tx, conversation.id, id, RunEnd::Done { answer }).await?;
                        return Ok(TaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: json!({"entryId":answer}),
                            },
                        });
                    }
                    self.interrupted_failure(tx, &settings, "restart_limit", &text, salvage)
                        .await
                })
                .await;
        }
        if matches!(phase, "request" | "poll") && self.record.restarts.is_some() {
            let backoff = super::retry::backoff(&settings, attempt);
            if backoff.is_none() {
                self.cancel_deferred_checkpoint(checkpoint, ctx).await?;
            }
            let stopped = if phase == "poll" {
                "the deferred answer was fetched"
            } else {
                "the model was answering"
            };

            return self
                .commit(ctx, async |tx, _| {
                    let text = format!("The host stopped while {stopped}, on attempt {attempt}");
                    let Some(delay) = backoff else {
                        return self
                            .interrupted_failure(tx, &settings, "interrupted", &text, true)
                            .await;
                    };
                    convert_partial(tx, conversation.id).await?;
                    let until = now().saturating_add(delay);
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    tx.doc(&LIVE_DOC, &address, None).await?.set(
                        &[Segment::Key("generation".into())],
                        json!({"attempt":attempt,"retry":{"at":until,"error":text}}).into(),
                    )?;
                    let mut next = json!({"phase":"retry","attempt":attempt,"until":until});
                    if let Some(compacted) = checkpoint.get("compacted") {
                        next["compacted"] = compacted.clone();
                    }
                    if phase == "poll" {
                        let mut poll = json!({});
                        for field in ["model", "thinkingLevel", "cutoff", "handle"] {
                            if let Some(value) = checkpoint.get(field) {
                                poll[field] = value.clone();
                            }
                        }
                        next["poll"] = poll;
                    }
                    Ok(TaskState::Sleeping {
                        checkpoint: next,
                        until: Some(until),
                        signals: None,
                    })
                })
                .await;
        }
        if let Some(until) = match phase {
            "retry" => checkpoint["until"].as_u64(),
            "poll" => checkpoint["pollAt"].as_u64(),
            _ => None,
        }
        .filter(|until| *until > now())
        {
            return self
                .commit(ctx, async |_, _| {
                    Ok(TaskState::Sleeping {
                        checkpoint: checkpoint.clone(),
                        until: Some(until),
                        signals: None,
                    })
                })
                .await;
        }
        if phase == "retry" {
            return self
                .commit(ctx, async |tx, _| {
                    let mut next = checkpoint.get("poll").cloned().unwrap_or_else(|| json!({}));
                    let mut progress = json!({"attempt":attempt+1});
                    if checkpoint.get("poll").is_some() {
                        let at = now();
                        next["phase"] = json!("poll");
                        next["pollAt"] = json!(at);
                        progress["deferred"] = json!({"pollAt":at});
                    } else {
                        next["phase"] = json!("prepare");
                    }
                    next["attempt"] = json!(attempt + 1);
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    tx.doc(&LIVE_DOC, &address, None)
                        .await?
                        .set(&[Segment::Key("generation".into())], progress.into())?;
                    if let Some(compacted) = checkpoint.get("compacted") {
                        next["compacted"] = compacted.clone();
                    }
                    Ok(TaskState::Running {
                        checkpoint: next,
                        wake: None,
                    })
                })
                .await;
        }
        let agent = conversation.agent(ctx).await?;
        if matches!(phase, "finish" | "yield" | "drain") {
            return self.finish_answer(checkpoint, &agent, &settings, ctx).await;
        }
        if matches!(phase, "tools" | "afterTools") {
            let children: Vec<crate::TaskId> = serde_json::from_value(checkpoint["tools"].clone())?;
            let assistant = checkpoint["assistant"]
                .as_u64()
                .ok_or_else(|| Error::invalid("Tool round has no assistant entry"))?;
            let pending: Vec<String> = serde_json::from_value(
                checkpoint
                    .get("pending")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )?;
            if let Some((next, rest)) = pending.split_first() {
                return self
                    .commit(ctx, async |tx, _| {
                        let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                        let value = serde_json::to_value(
                            tx.doc(&LIVE_DOC, &address, None).await?.value()?.to_json(),
                        )?;
                        let slot = value["tools"].as_array().and_then(|slots| {
                            slots
                                .iter()
                                .enumerate()
                                .find(|(_, s)| s["callId"] == *next && s.get("taskId").is_none())
                        });
                        let mut input = json!({"kind":"model","assistant":assistant,"callId":next});
                        if slot.is_some_and(|(_, s)| s["unoffered"] == true) {
                            input["unoffered"] = json!(true);
                        }
                        let mut options = TaskOptions::new(TaskOwnership::Task { task_id: id });
                        options.conversation_id = Some(conversation.id);
                        let child = tx
                            .create_task(super::tool_task().as_ref(), input, options)
                            .await?;
                        if let Some((i, _)) = slot {
                            tx.doc(&LIVE_DOC, &address, None).await?.set(
                                &[
                                    Segment::Key("tools".into()),
                                    Segment::Index(i),
                                    Segment::Key("taskId".into()),
                                ],
                                json!(child).into(),
                            )?;
                        }
                        let mut tools = children;
                        tools.push(child);
                        Ok(TaskState::Waiting {
                            checkpoint: json!({
                                "phase": "tools",
                                "assistant": assistant,
                                "tools": tools,
                                "pending": rest
                            }),
                            on: vec![child],
                            policy: crate::JoinPolicy::AllSettled,
                        })
                    })
                    .await;
            }
            let mut hook_terminate = false;
            if phase == "afterTools" {
                for child in serde_json::from_value::<Vec<u64>>(checkpoint["tasks"].clone())? {
                    if let Some(task) = self.harness.session.task(child, ctx).await?
                        && matches!(&task.state,TaskState::Terminal{outcome:TaskOutcome::Completed{result}} if result["terminate"]==true)
                    {
                        hook_terminate = true;
                    }
                }
            } else if !agent.hooks("pi.generation", "afterTools").is_empty() {
                let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                let live = self
                    .harness
                    .session
                    .snapshot(&LIVE_DOC, &address, ctx)
                    .await?
                    .unwrap_or_default();
                let results = live["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["entry"].as_u64())
                    .collect::<Vec<_>>();
                let decisions = self
                    .hook_decisions(
                        &agent,
                        "afterTools",
                        json!({"assistant":assistant,"results":results}),
                        ctx,
                    )
                    .await?;
                if !decisions.tasks.is_empty() {
                    return self
                        .commit(ctx, async |tx, _| {
                            let tasks = self.hook_children(tx, &decisions).await?;
                            if tasks.is_empty() {
                                return Ok(TaskState::Running {
                                    checkpoint: json!({
                                        "phase": "afterTools",
                                        "assistant": assistant,
                                        "tools": children,
                                        "tasks": []
                                    }),
                                    wake: None,
                                });
                            }
                            Ok(TaskState::Waiting {
                                checkpoint: json!({
                                    "phase": "afterTools",
                                    "assistant": assistant,
                                    "tools": children,
                                    "tasks": tasks
                                }),
                                on: tasks,
                                policy: crate::JoinPolicy::AllSettled,
                            })
                        })
                        .await;
                }
            }
            return self
                .commit(ctx, async |tx, _| {
                    let mut boundary = prepare_boundary(
                        tx,
                        conversation.id,
                        settings["steeringMode"] == "all",
                        settings["followUpMode"] == "all",
                    )
                    .await?;
                    let mut added = Vec::new();
                    let mut handoff = None;
                    let mut terminate = !children.is_empty();
                    for child in children {
                        let task = tx.task(child).await?;
                        let control = task.as_ref().and_then(|t| match &t.state {
                            TaskState::Terminal {
                                outcome: TaskOutcome::Completed { result },
                            } => result.get("control"),
                            _ => None,
                        });
                        terminate &= control.is_some_and(|v| v["terminate"] == true);
                        if let Some(control) = control {
                            added.extend(
                                control["addTools"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned),
                            );
                            if let Some(text) = control["handoff"].as_str() {
                                handoff = Some(text.to_owned());
                            }
                        }
                    }
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    let state = serde_json::to_value(
                        tx.doc(&LIVE_DOC, &address, None).await?.value()?.to_json(),
                    )?;
                    terminate |= hook_terminate;
                    if !added.is_empty() {
                        super::add_tools(tx, conversation.id, &added).await?;
                    }
                    if let Some(content) = handoff {
                        let entry = tx
                            .append_entry(
                                conversation.id,
                                EntryDraft {
                                    kind: "pi.reset".into(),
                                    head: Some(crate::EntryHead::SelfEntry),
                                    model: Some(vec![
                                        json!({"role":"user","content":content,"timestamp":now()}),
                                    ]),
                                    ..Default::default()
                                },
                            )
                            .await?;
                        boundary.head = Some(entry.id);
                    }
                    let turns = state["run"]["turns"].as_u64().unwrap_or(0);
                    if !terminate
                        && agent.state["maxTurns"]
                            .as_u64()
                            .is_some_and(|limit| turns >= limit)
                    {
                        return self
                            .generation_failure(
                                tx,
                                conversation.id,
                                Some(boundary),
                                &settings,
                                "turn_limit",
                                &format!("{turns} turn{}", if turns == 1 { "" } else { "s" }),
                            )
                            .await;
                    }
                    let placed = apply_boundary(tx, boundary, terminate, now()).await?;
                    if terminate || placed.reset {
                        let end = if terminate {
                            RunEnd::Done { answer: assistant }
                        } else {
                            RunEnd::Unanswered {
                                reason: "reset".into(),
                                detail: None,
                                settlement: None,
                            }
                        };
                        end_run(tx, conversation.id, id, end).await?;
                        if !placed.users.is_empty() {
                            start_run(tx, conversation.id, placed.users, placed.first).await?;
                        }
                    } else {
                        let mut options = TaskOptions::new(TaskOwnership::Conversation);
                        options.conversation_id = Some(conversation.id);
                        let child = tx
                            .create_task(generation_task().as_ref(), json!({}), options)
                            .await?;
                        let doc = tx.doc(&LIVE_DOC, &address, None).await?;
                        let mut run = state["run"].clone();
                        if run["taskId"] == id {
                            run["taskId"] = json!(child);
                            if let Some(inputs) = run["inputs"].as_array_mut() {
                                inputs.extend(placed.users.into_iter().map(Value::from));
                            }
                            doc.set(&[Segment::Key("run".into())], run.into())?;
                        }
                        for key in ["tools", "nestedTools"] {
                            if state.get(key).is_some() {
                                doc.delete(&[Segment::Key(key.into())])?;
                            }
                        }
                    }
                    Ok(TaskState::Terminal {
                        outcome: TaskOutcome::Completed {
                            result: json!({"entryId":assistant}),
                        },
                    })
                })
                .await;
        }
        let reference = if phase == "prepare" {
            &agent.state["model"]
        } else {
            &checkpoint["model"]
        };
        let model = reference["provider"]
            .as_str()
            .zip(reference["modelId"].as_str())
            .and_then(|(p, m)| self.harness.models.get_model(p, m));
        let Some(model) = model else {
            let text = if reference.is_null() {
                "No model is configured".into()
            } else {
                format!(
                    "Model {}/{} is not available",
                    reference["provider"].as_str().unwrap_or(""),
                    reference["modelId"].as_str().unwrap_or("")
                )
            };
            return self
                .commit(ctx, async |tx, _| {
                    self.generation_failure(tx, conversation.id, None, &settings, "no_model", &text)
                        .await
                })
                .await;
        };
        let policy = super::compaction_policy(
            &settings["compaction"],
            &format!("{}/{}", model.provider, model.id),
            &agent.state["compaction"],
        );
        let keep = policy["keepRecentTokens"].as_u64().unwrap_or(20000);
        let compacted = checkpoint["compacted"].as_u64();
        if phase == "prepare"
            && let Some(child) = compacted
        {
            let task = self.harness.session.task(child, ctx).await?;
            let outcome = task.as_ref().and_then(|t| match &t.state {
                TaskState::Terminal { outcome } => Some(outcome),
                _ => None,
            });
            let failure = if let Some(text) = checkpoint["overflow"].as_str() {
                (!matches!(outcome,Some(TaskOutcome::Completed {result}) if result["entryId"].is_u64())).then(||text.to_owned())
            } else if checkpoint["threshold"] == true {
                match outcome {
                    Some(TaskOutcome::Failed { error, .. } | TaskOutcome::Faulted { error }) => {
                        Some(format!("Compaction failed: {}", error.message))
                    }
                    Some(TaskOutcome::Orphaned { reason }) => {
                        Some(format!("Compaction failed: {reason}"))
                    }
                    Some(TaskOutcome::Aborted { .. }) => Some("Compaction was aborted".into()),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(text) = failure {
                return self
                    .commit(ctx, async |tx, _| {
                        self.generation_failure(
                            tx,
                            conversation.id,
                            None,
                            &settings,
                            "model_error",
                            &text,
                        )
                        .await
                    })
                    .await;
            }
        }
        let view = conversation
            .context(checkpoint["cutoff"].as_u64(), ctx)
            .await?;
        if phase == "prepare" {
            if !view
                .messages
                .iter()
                .rev()
                .find(|m| m["role"] != "system")
                .is_some_and(|m| m["role"] == "user" || m["role"] == "toolResult")
            {
                return self
                    .commit(ctx, async |tx, _| {
                        self.generation_failure(
                            tx,
                            conversation.id,
                            None,
                            &settings,
                            "nothing_to_answer",
                            "The conversation has nothing to answer",
                        )
                        .await
                    })
                    .await;
            }
            let shown = super::replay_sections(&view.messages);
            let mut desired = indexmap::IndexMap::new();
            let input = json!({
                "conversationId": conversation.id,
                "agent": agent.state,
                "shown": shown
            });
            for section in &agent.sections {
                match (section.render)(&input, ctx).await {
                    Ok(Some(text)) => {
                        desired.insert(
                            section.key.clone(),
                            if section.tag {
                                format!("<{0}>\n{1}\n</{0}>", section.key, text)
                            } else {
                                text
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if ctx.check().is_err() {
                            return Err(e);
                        }
                        if let Some(text) = shown.get(&section.key) {
                            desired.insert(section.key.clone(), text.clone());
                        }
                    }
                }
            }
            let tools = agent
                .tools
                .iter()
                .map(|t| t.declaration.clone())
                .collect::<Vec<_>>();
            let entries = super::plan_system_entries(&view, &desired, &tools, now())?;
            let mut background = false;
            if compacted.is_none() && policy["enabled"] == true && model.context_window > 0 {
                let tokens = super::estimate_context(&view, &entries)?;
                let limit = model.context_window as f64
                    - policy["reserveTokens"].as_f64().unwrap_or(16384.0);
                let margin = policy["backgroundTokens"].as_f64().unwrap_or(32768.0);
                if (tokens > limit || margin > 0.0 && tokens > limit - margin)
                    && super::select_cut(&view, keep)?.is_some()
                {
                    if tokens > limit {
                        return self
                            .commit(ctx, async |tx, _| {
                                let child = super::create_compaction(
                                    tx,
                                    conversation.id,
                                    json!({"reason":"threshold"}),
                                    Some(id),
                                )
                                .await?;
                                Ok(TaskState::Waiting {
                                    checkpoint: json!({
                                        "phase": "prepare",
                                        "attempt": attempt,
                                        "compacted": child,
                                        "threshold": true
                                    }),
                                    on: vec![child],
                                    policy: crate::JoinPolicy::AllSettled,
                                })
                            })
                            .await;
                    }
                    background = true;
                }
            }
            return self.commit(ctx,async |tx,_|{
                let mut cutoff = tx
                    .latest_entry_id(conversation.id)
                    .await?
                    .ok_or_else(|| Error::invalid("Empty request history"))?;
                for entry in entries {
                    cutoff = tx.append_entry(conversation.id, entry).await?.id;
                }
                if background {
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    let live = tx.doc(&LIVE_DOC, &address, None).await?.value()?.to_json();
                    if serde_json::to_value(live)?.get("compactions").is_none() {
                        super::create_compaction(tx, conversation.id, json!({"reason":"threshold"}), None)
                            .await?;
                    }
                }
                let mut request = json!({
                    "phase": "request",
                    "attempt": attempt,
                    "model": reference,
                    "thinkingLevel": agent.state["thinkingLevel"].as_str().unwrap_or("off"),
                    "streamOptions": settings.get("stream").cloned().unwrap_or_else(||json!({})),
                    "cutoff": cutoff
                });
                if let Some(child) = compacted {
                    request["compacted"] = json!(child);
                }
                Ok(TaskState::Running {
                    checkpoint: request,
                    wake: None,
                })
            }).await;
        }
        let session_id =
            super::ensure_provider_session_id(&self.harness.session, conversation.id, ctx).await?;
        let mut opts = stream_options(checkpoint, session_id, ctx)?;
        let hooks = agent.hooks("pi.generation", "beforeProviderRequest");
        if !hooks.is_empty() {
            let hooks = std::sync::Arc::new(hooks);
            let invocation = self.clone();
            let context = ctx.clone();
            opts.request.on_payload = Some(std::sync::Arc::new(move |mut payload, model| {
                let (invocation, context, hooks) =
                    (invocation.clone(), context.clone(), hooks.clone());
                Box::pin(async move {
                    for hook in hooks.iter() {
                        let input = json!({"model":model,"payload":payload});
                        if let Some(replacement) = invocation
                            .invoke_hook(hook, input, &context)
                            .await
                            .map_err(|e| pi_rs_ai::Error::Protocol(e.to_string()))?
                        {
                            payload = replacement.get("payload").cloned().ok_or_else(|| {
                                pi_rs_ai::Error::Protocol(
                                    "beforeProviderRequest returned no payload".into(),
                                )
                            })?;
                        }
                    }
                    Ok(payload)
                })
            }));
        }
        let message = if phase == "poll" {
            self.harness
                .models
                .fetch_deferred(
                    model,
                    serde_json::from_value(checkpoint["handle"].clone())?,
                    opts,
                )
                .await
                .map_err(|e| Error::Callback(Box::new(e)))?
        } else {
            let mut provider_messages = json!({"messages":view.messages});
            for hook in agent.hooks("pi.generation", "beforeRequest") {
                if let Some(next) = self
                    .invoke_hook(&hook, provider_messages.clone(), ctx)
                    .await?
                {
                    provider_messages = next;
                }
            }
            let messages = provider_messages["messages"]
                .as_array()
                .ok_or_else(|| Error::invalid("beforeRequest returned invalid messages"))?
                .iter()
                .cloned()
                .map(serde_json::from_value)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            self.stream_response(
                &self.harness.models,
                GenerationRequest {
                    model,
                    context: pi_rs_ai::types::Context {
                        messages,
                        ..Default::default()
                    },
                    options: opts,
                    attempt,
                    partial_interval: Duration::from_millis(
                        settings["progress"]["partialIntervalMs"]
                            .as_u64()
                            .unwrap_or(100),
                    ),
                },
                ctx,
            )
            .await?
        };
        if message.stop_reason == StopReason::Deferred
            && let Some(handle) = &message.deferred
        {
            let until = now()
                .saturating_add(handle.poll_after_ms.unwrap_or(5000))
                .max(checkpoint["pollAt"].as_u64().unwrap_or(0).saturating_add(1));
            return self
                .commit(ctx, async |tx, _| {
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    tx.doc(&LIVE_DOC, &address, None).await?.set(
                        &[Segment::Key("generation".into())],
                        json!({"attempt":attempt,"deferred":{"pollAt":until}}).into(),
                    )?;
                    Ok(TaskState::Sleeping {
                        until: Some(until),
                        signals: None,
                        checkpoint: json!({
                            "phase": "poll",
                            "attempt": attempt,
                            "model": reference,
                            "thinkingLevel": checkpoint["thinkingLevel"],
                            "cutoff": checkpoint["cutoff"],
                            "handle": handle,
                            "pollAt": until
                        }),
                    })
                })
                .await;
        }
        for hook in agent.hooks("pi.generation", "afterResponse") {
            self.invoke_hook(&hook, serde_json::to_value(&*message)?, ctx)
                .await?;
        }
        if message.stop_reason == StopReason::Error
            && pi_rs_ai::utils::is_context_overflow(&message, None)
            && compacted.is_none()
            && policy["enabled"] == true
            && super::select_cut(&view, keep)?.is_some()
        {
            return self.commit(ctx,async |tx,_|{
                let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                append_assistant(tx, conversation.id, (*message).clone()).await?;
                tx.doc(&LIVE_DOC, &address, None)
                    .await?
                    .delete(&[Segment::Key("generation".into())])?;
                let child =
                    super::create_compaction(tx, conversation.id, json!({"reason":"overflow"}), Some(id))
                        .await?;
                Ok(TaskState::Waiting {
                    checkpoint: json!({
                        "phase": "prepare",
                        "attempt": attempt,
                        "compacted": child,
                        "overflow": message.error_message.as_deref().unwrap_or("Context overflow")
                    }),
                    on: vec![child],
                    policy: crate::JoinPolicy::AllSettled,
                })
            }).await;
        }
        let is_answer = matches!(message.stop_reason, StopReason::Stop | StopReason::Length)
            || message.stop_reason == StopReason::ToolUse
                && !message
                    .content
                    .iter()
                    .any(|block| matches!(block, pi_rs_ai::types::AssistantContent::ToolCall(_)));
        let decisions = if is_answer {
            self.hook_decisions(&agent, "onYield", serde_json::to_value(&*message)?, ctx)
                .await?
        } else {
            Default::default()
        };
        self.commit(ctx, async |tx, _| {
            let boundary = prepare_boundary(
                tx,
                conversation.id,
                settings["steeringMode"] == "all",
                settings["followUpMode"] == "all",
            )
            .await?;
            let entry = append_assistant(tx, conversation.id, (*message).clone()).await?;
            if !matches!(
                message.stop_reason,
                StopReason::Stop | StopReason::Length | StopReason::ToolUse
            ) {
                let text = message.error_message.clone().unwrap_or_else(|| {
                    format!(
                        "Model response ended with stop reason {}",
                        message.stop_reason.as_str()
                    )
                });
                if message.stop_reason == StopReason::Error
                    && !pi_rs_ai::utils::is_context_overflow(&message, None)
                    && let Some(delay) = super::retry::delay(&settings, &text, attempt)
                {
                    let until = now().saturating_add(delay);
                    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                    tx.doc(&LIVE_DOC, &address, None).await?.set(
                        &[Segment::Key("generation".into())],
                        json!({"attempt":attempt,"retry":{"at":until,"error":text}}).into(),
                    )?;
                    return Ok(TaskState::Sleeping {
                        checkpoint: json!({"phase":"retry","attempt":attempt,"until":until}),
                        until: Some(until),
                        signals: None,
                    });
                }
                return self
                    .generation_failure(
                        tx,
                        conversation.id,
                        Some(boundary),
                        &settings,
                        "model_error",
                        &text,
                    )
                    .await;
            }
            let mut children = Vec::new();
            let sequential = settings["toolExecution"] == "sequential" || message.content.iter().any(|block| {
                matches!(block, pi_rs_ai::types::AssistantContent::ToolCall(call)
                    if agent.tools.iter().any(|tool| tool.declaration.name == call.name && tool.sequential))
            });
            let mut slots = Vec::new();
            let mut pending = Vec::new();
            for block in message
                .content
                .iter()
                .filter(|_| message.stop_reason == StopReason::ToolUse)
            {
                let pi_rs_ai::types::AssistantContent::ToolCall(call) = block else {
                    continue;
                };
                let mut input = json!({"kind":"model","assistant":entry.id,"callId":call.id});
                let mut slot = json!({"callId":call.id,"name":call.name,"status":"pending"});
                if !agent.tools.iter().any(|t| t.declaration.name == call.name) {
                    input["unoffered"] = json!(true);
                    slot["unoffered"] = json!(true);
                }
                if sequential && !children.is_empty() {
                    pending.push(call.id.clone());
                    slots.push(slot);
                    continue;
                }
                let mut options = TaskOptions::new(TaskOwnership::Task { task_id: id });
                options.conversation_id = Some(conversation.id);
                let child = tx
                    .create_task(super::tool_task().as_ref(), input, options)
                    .await?;
                children.push(child);
                slot["taskId"] = json!(child);
                slots.push(slot);
            }
            if !children.is_empty() {
                let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                let doc = tx.doc(&LIVE_DOC, &address, None).await?;
                doc.set(&[Segment::Key("tools".into())], json!(slots).into())?;
                let state = serde_json::to_value(doc.value()?.to_json())?;
                if state.get("generation").is_some() {
                    doc.delete(&[Segment::Key("generation".into())])?;
                }
                if state["run"]["taskId"] == id {
                    let mut run = state["run"].clone();
                    run["turns"] = json!(run["turns"].as_u64().unwrap_or(0) + 1);
                    if run.get("first").is_none() {
                        run["first"] = json!(entry.id);
                    }
                    doc.set(&[Segment::Key("run".into())], run.into())?;
                }
                return Ok(TaskState::Waiting {
                    checkpoint: json!({
                        "phase": "tools",
                        "assistant": entry.id,
                        "tools": children,
                        "pending": pending
                    }),
                    on: children,
                    policy: crate::JoinPolicy::AllSettled,
                });
            }
            let tasks = self.hook_children(tx, &decisions).await?;
            if !tasks.is_empty() {
                let address = resolve_address(&LIVE_DOC, Some(conversation.id), None)?;
                let doc = tx.doc(&LIVE_DOC, &address, None).await?;
                let state = serde_json::to_value(doc.value()?.to_json())?;
                if state.get("generation").is_some() {
                    doc.delete(&[Segment::Key("generation".into())])?;
                }
                doc.set(
                    &[Segment::Key("run".into()), Segment::Key("yield".into())],
                    json!({"answer":entry.id,"tasks":tasks}).into(),
                )?;
                return Ok(TaskState::Waiting {
                    checkpoint: json!({"phase":"yield","answer":entry.id,"tasks":tasks}),
                    on: tasks,
                    policy: crate::JoinPolicy::AllSettled,
                });
            }
            let continuation = decisions.continuation.clone();
            self.conclude_answer(
                tx,
                boundary,
                entry.id,
                continuation,
                !agent.hooks("pi.generation", "afterRun").is_empty(),
            )
            .await
        })
        .await
    }
}
