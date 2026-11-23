use super::{ContextView, order_tool_results};
use crate::session::Result;
use pi_rs_ai::simple_options::estimate_message_tokens;
use serde_json::Value;
use std::collections::HashSet;

pub fn compaction_policy(settings: &Value, model_key: &str, conversation: &Value) -> Value {
    let local = &conversation["modelOverrides"][model_key];
    let global = &settings["modelOverrides"][model_key];
    let mut result = serde_json::Map::new();
    for key in [
        "enabled",
        "reserveTokens",
        "keepRecentTokens",
        "backgroundTokens",
    ] {
        let value = if key == "enabled" && local.get(key).is_none() && conversation[key] == false {
            Value::Bool(false)
        } else {
            [local, global, conversation, settings]
                .into_iter()
                .find_map(|layer| layer.get(key))
                .cloned()
                .unwrap_or_else(|| super::DEFAULT_COMPACTION_POLICY[key].clone())
        };
        result.insert(key.into(), value);
    }
    Value::Object(result)
}

fn cut_candidate(contributions: &[Vec<Value>], at: usize) -> bool {
    let Some(first) = contributions[at].first() else {
        return false;
    };
    if first["role"] == "assistant" {
        return true;
    }
    if first["role"] != "user" {
        return false;
    }
    let calls: HashSet<_> = contributions[..at]
        .iter()
        .flatten()
        .rev()
        .find(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_array())
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "toolCall")
        .filter_map(|b| b["id"].as_str())
        .collect();
    for message in contributions[at..].iter().flatten() {
        if message["role"] == "assistant" {
            break;
        }
        if message["role"] == "toolResult"
            && message["toolCallId"]
                .as_str()
                .is_some_and(|id| calls.contains(id))
        {
            return false;
        }
    }
    true
}

pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Result<Option<usize>> {
    let start = usize::from(view.head.is_some());
    let candidates: Vec<_> = (start..view.contributions.len())
        .filter(|&at| cut_candidate(&view.contributions, at))
        .collect();
    let mut kept = 0;
    for at in (start..view.contributions.len()).rev() {
        for message in &view.contributions[at] {
            kept += estimate_message_tokens(&serde_json::from_value(message.clone())?);
        }
        if kept >= keep_recent_tokens {
            return Ok(candidates
                .iter()
                .find(|&&i| i >= at)
                .or(candidates.last())
                .copied()
                .filter(|&i| view.contributions[start..i].iter().any(|c| !c.is_empty())));
        }
    }
    Ok(None)
}

pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Value> {
    order_tool_results(
        &view
            .contributions
            .iter()
            .take(cut)
            .flatten()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work. If the conversation starts with an earlier summary, preserve its information and fold the newer messages into it.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

use super::{Conversation, Invocation, LIVE_DOC, SubmissionBody, SubmissionDraft};
use crate::{
    EntryDraft, EntryHead, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskState,
    documents::resolve_address,
    session::{Error, TaskOptions, Transaction},
    tasks::{PhaseHandler, Task, TaskDefinition},
};
use pi_rs_chord::{context::Context, delta::Segment};
use serde_json::json;
use std::sync::Arc;

pub fn serialize_conversation(messages: &[Value]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let content = &msg["content"];
        let blocks = content.as_array().into_iter().flatten();
        let text = content.as_str().map(str::to_owned).unwrap_or_else(|| {
            blocks
                .clone()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
        match msg["role"].as_str() {
            Some("user") if !text.is_empty() => parts.push(format!("[User]: {text}")),
            Some("assistant") => {
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls = blocks
                    .filter(|b| b["type"] == "toolCall")
                    .map(|b| {
                        let args = b["arguments"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{}({args})", b["name"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Some("toolResult") if !text.is_empty() => {
                let units: Vec<_> = text.encode_utf16().collect();
                let text = if units.len() > 2000 {
                    format!(
                        "{}\n\n[... {} more characters truncated]",
                        String::from_utf16_lossy(&units[..2000]),
                        units.len() - 2000
                    )
                } else {
                    text
                };
                parts.push(format!("[Tool result]: {text}"));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

pub fn compaction_task() -> Arc<Task<Invocation>> {
    let handle: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move { inv.run_compaction(ctx).await.map_err(Into::into) })
    });
    Arc::new(crate::define_task(TaskDefinition {
        name: "pi.compaction".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({"phase":"select"}))),
        phases: ["select", "summarize", "retry"]
            .into_iter()
            .map(|p| (p.into(), handle.clone()))
            .collect(),
        abort: handle,
        migrate: None,
        hooks: (),
    }))
}

pub async fn create_compaction(
    tx: &mut Transaction<'_>,
    conversation: crate::ConversationId,
    input: Value,
    owner: Option<crate::TaskId>,
) -> Result<crate::TaskId> {
    let mut options = TaskOptions::new(owner.map_or(TaskOwnership::Conversation, |task_id| {
        TaskOwnership::Task { task_id }
    }));
    options.conversation_id = Some(conversation);
    options.background = owner.is_none() && input["reason"] != "manual";
    let reason = input["reason"].clone();
    let id = tx
        .create_task(compaction_task().as_ref(), input, options)
        .await?;
    let address = resolve_address(&LIVE_DOC, Some(conversation), None)?;
    let doc = tx.doc(&LIVE_DOC, &address, None).await?;
    let mut items = serde_json::to_value(doc.value()?.to_json())?["compactions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    items.push(json!({
        "taskId": id,
        "trigger": reason,
        "blocking": owner.is_some(),
        "attempt": 1
    }));
    doc.set(&[Segment::Key("compactions".into())], json!(items).into())?;
    Ok(id)
}

impl Conversation {
    pub async fn compact(
        &self,
        instructions: Option<String>,
        ctx: &Context,
    ) -> Result<crate::TaskId> {
        self.harness.resume()?;
        let mut input = json!({"reason":"manual"});
        if let Some(instructions) = instructions {
            input["instructions"] = json!(instructions);
        }
        self.commit(
            async |tx| create_compaction(tx, self.id, input, None).await,
            ctx,
            Default::default(),
        )
        .await
    }
}

fn summary_entry(first: crate::EntryId, summary: &str, reason: &Value, now: u64) -> EntryDraft {
    let text = format!(
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
        summary
    );
    EntryDraft {
        kind: "pi.compaction".into(),
        head: Some(EntryHead::Id(first)),
        model: Some(vec![json!({
            "role": "user",
            "content": [{"type":"text","text":text}],
            "timestamp": now
        })]),
        data: Some(json!({"reason":reason})),
        edits: None,
    }
}

impl Invocation {
    async fn run_compaction(&self, ctx: &Context) -> Result<()> {
        let conv = self.harness.handle(self.record.conversation_id);
        let TaskState::Running { checkpoint, .. } = &self.record.state else {
            return Err(Error::invalid("Compaction must be running"));
        };
        if checkpoint["phase"] == "retry" && !self.record.abort_requested {
            let until = checkpoint["until"].as_u64().unwrap_or(0);
            return self
                .commit(ctx, async |tx, _| {
                    if until > (self.harness.now)() {
                        return Ok(TaskState::Sleeping {
                            checkpoint: checkpoint.clone(),
                            until: Some(until),
                            signals: None,
                        });
                    }
                    let mut next = checkpoint.clone();
                    let attempt = checkpoint["attempt"].as_u64().unwrap_or(1) + 1;
                    next["phase"] = json!("summarize");
                    next["attempt"] = json!(attempt);
                    if let Some(fields) = next.as_object_mut() {
                        fields.remove("until");
                    }
                    let address = resolve_address(&LIVE_DOC, Some(conv.id), None)?;
                    let doc = tx.doc(&LIVE_DOC, &address, None).await?;
                    let value = serde_json::to_value(doc.value()?.to_json())?;
                    if let Some(index) = value["compactions"]
                        .as_array()
                        .and_then(|items| items.iter().position(|s| s["taskId"] == self.record.id))
                    {
                        let path = [
                            Segment::Key("compactions".into()),
                            Segment::Index(index),
                            Segment::Key("attempt".into()),
                        ];
                        doc.set(&path, json!(attempt).into())?;
                        doc.delete(&[
                            path[0].clone(),
                            path[1].clone(),
                            Segment::Key("retry".into()),
                        ])?;
                    }
                    Ok(TaskState::Running {
                        checkpoint: next,
                        wake: None,
                    })
                })
                .await;
        }
        let agent = conv.agent(ctx).await?;
        let settings = (self.harness.settings)();
        let selecting = checkpoint["phase"] == "select";
        let reference = if selecting {
            &agent.state["model"]
        } else {
            &checkpoint["model"]
        };
        let model = reference["provider"]
            .as_str()
            .zip(reference["modelId"].as_str())
            .and_then(|(p, m)| self.harness.models.get_model(p, m));
        let mut outcome = TaskOutcome::Completed { result: json!({}) };
        let mut entry = None;
        let mut usage = None;
        if self.record.abort_requested {
            outcome = TaskOutcome::Aborted {
                result: None,
                reason: None,
            };
        } else if let Some(model) = model {
            let view = conv.context(checkpoint["tail"].as_u64(), ctx).await?;
            if selecting {
                let key = format!("{}/{}", model.provider, model.id);
                let policy =
                    compaction_policy(&settings["compaction"], &key, &agent.state["compaction"]);
                if let Some(cut) = select_cut(
                    &view,
                    policy["keepRecentTokens"].as_u64().unwrap_or_default(),
                )? {
                    let first = view.entries[cut].id;
                    let mut input = json!({
                        "reason": self.record.input["reason"],
                        "entries": &view.entries[..cut],
                        "messages": summarized_messages(&view,cut),
                        "firstKept": first
                    });
                    if let Some(instructions) = self.record.input.get("instructions") {
                        input["instructions"] = instructions.clone();
                    }
                    let mut decision = None;
                    for hook in agent.hooks("pi.compaction", "beforeCompact") {
                        if let Some(value) = hook(input.clone(), self, ctx).await? {
                            decision = Some(value);
                            break;
                        }
                    }
                    match decision.as_ref().and_then(|v| v["kind"].as_str()) {
                        Some("decline") => {}
                        Some("summary") => {
                            let summary = decision
                                .as_ref()
                                .and_then(|v| v["summary"].as_str())
                                .ok_or_else(|| {
                                    Error::invalid("beforeCompact returned no summary")
                                })?;
                            entry = Some(summary_entry(
                                first,
                                summary,
                                &self.record.input["reason"],
                                (self.harness.now)(),
                            ));
                        }
                        _ => {
                            let limit = (policy["reserveTokens"].as_u64().unwrap_or_default() * 4
                                / 5)
                            .min(if model.max_tokens == 0 {
                                u64::MAX
                            } else {
                                model.max_tokens
                            });
                            let tail = view.entries.iter().map(|e| e.id).max();
                            return self
                                .commit(ctx, async |_, _| {
                                    Ok(TaskState::Running {
                                        checkpoint: json!({
                                            "phase": "summarize",
                                            "attempt": 1,
                                            "model": reference,
                                            "thinkingLevel": agent.state["thinkingLevel"],
                                            "maxTokens": limit,
                                            "tail": tail,
                                            "firstKept": view.entries[cut].id,
                                            "streamOptions": settings["stream"]
                                        }),
                                        wake: None,
                                    })
                                })
                                .await;
                        }
                    }
                }
            } else {
                let first = checkpoint["firstKept"]
                    .as_u64()
                    .ok_or_else(|| Error::invalid("Compaction has no cutoff"))?;
                let cut = view
                    .entries
                    .iter()
                    .position(|e| e.id == first)
                    .ok_or_else(|| Error::invalid("Missing compaction cutoff"))?;
                let focus = self.record.input["instructions"]
                    .as_str()
                    .map(|s| format!("\n\nAdditional focus: {s}"))
                    .unwrap_or_default();
                let prompt = format!(
                    "<conversation>\n{}\n</conversation>\n\n{SUMMARIZATION_PROMPT}{focus}",
                    serialize_conversation(&summarized_messages(&view, cut))
                );
                let now = (self.harness.now)();
                let messages = serde_json::from_value(json!([
                    {"role": "system", "content": SUMMARIZATION_SYSTEM_PROMPT, "timestamp": now},
                    {"role": "user", "content": [{"type": "text", "text": prompt}], "timestamp": now}
                ]))?;
                let session_id =
                    super::ensure_provider_session_id(&self.harness.session, conv.id, ctx).await?;
                let mut opts = super::generation::stream_options(checkpoint, session_id, ctx)?;
                opts.max_tokens = checkpoint["maxTokens"].as_u64();
                opts.cache_retention = Some(pi_rs_ai::model::CacheRetention::None);
                opts.deferred = None;
                let signal = opts.request.signal.clone();
                let _cancel = ctx
                    .abort_signal()
                    .map(|s| s.subscribe(move |_| signal.cancel()));
                let msg = ctx
                    .await_with(self.harness.models.complete_simple(
                        model,
                        pi_rs_ai::types::Context {
                            messages,
                            ..Default::default()
                        },
                        opts,
                    ))
                    .await?
                    .map_err(|e| Error::Callback(Box::new(e)))?;
                ctx.check()?;
                let text = msg.error_message.as_deref().unwrap_or("");
                if msg.stop_reason == pi_rs_ai::StopReason::Error
                    && let Some(delay) = super::retry::delay(
                        &(self.harness.settings)(),
                        text,
                        checkpoint["attempt"].as_u64().unwrap_or(1),
                    )
                {
                    let until = (self.harness.now)().saturating_add(delay);
                    let mut next = checkpoint.clone();
                    next["phase"] = json!("retry");
                    next["until"] = json!(until);
                    return self
                        .commit(ctx, async |tx, _| {
                            super::record_usage(
                                tx,
                                conv.id,
                                super::UsageBucket::Models,
                                &format!("{}/{}", msg.provider, msg.model),
                                &msg.usage,
                            )
                            .await?;
                            let address = resolve_address(&LIVE_DOC, Some(conv.id), None)?;
                            let doc = tx.doc(&LIVE_DOC, &address, None).await?;
                            let state = serde_json::to_value(doc.value()?.to_json())?;
                            if let Some(i) = state["compactions"].as_array().and_then(|items| {
                                items.iter().position(|s| s["taskId"] == self.record.id)
                            }) {
                                doc.set(
                                    &[
                                        Segment::Key("compactions".into()),
                                        Segment::Index(i),
                                        Segment::Key("retry".into()),
                                    ],
                                    json!({"at":until,"error":text}).into(),
                                )?;
                            }
                            Ok(TaskState::Sleeping {
                                checkpoint: next,
                                until: Some(until),
                                signals: None,
                            })
                        })
                        .await;
                }
                usage = Some((format!("{}/{}", msg.provider, msg.model), msg.usage));
                let serialized = serde_json::to_value(&*msg)?;
                let blocks = serialized["content"].as_array().into_iter().flatten();
                let summary = blocks
                    .clone()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if serialized["stopReason"] == "stop"
                    && !summary.trim().is_empty()
                    && !blocks.clone().any(|b| b["type"] == "toolCall")
                {
                    entry = Some(summary_entry(
                        first,
                        summary.trim(),
                        &self.record.input["reason"],
                        now,
                    ));
                } else {
                    outcome = TaskOutcome::Failed {
                        error: TaskOutcomeError {
                            message: match msg.stop_reason {
                                pi_rs_ai::StopReason::Error | pi_rs_ai::StopReason::Aborted => {
                                    format!(
                                        "Summarization failed: {}",
                                        msg.error_message
                                            .as_deref()
                                            .unwrap_or(msg.stop_reason.as_str())
                                    )
                                }
                                pi_rs_ai::StopReason::Length => {
                                    "Summarization hit the token limit; the summary is incomplete"
                                        .into()
                                }
                                _ if blocks.clone().any(|b| b["type"] == "toolCall") => {
                                    "Summarization attempted to call a tool".into()
                                }
                                _ => "Summarization produced no text".into(),
                            },
                            detail: Some(json!({"reason":"model_error"})),
                        },
                        result: None,
                    };
                }
            }
        } else {
            outcome = TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: "No model is configured".into(),
                    detail: Some(json!({"reason":"no_model"})),
                },
                result: None,
            };
        }
        self.commit(ctx, async |tx, _| {
            if let Some((key, usage)) = usage {
                super::record_usage(tx, conv.id, super::UsageBucket::Models, &key, &usage).await?;
            }
            if let Some(entry) = entry {
                let result = if self.record.owner.is_some() {
                    let appended = tx.append_entry(conv.id, entry).await?;
                    json!({"entryId": appended.id})
                } else {
                    let draft = SubmissionDraft {
                        request_id: Some(format!("compaction:{}", self.record.id)),
                        notify: None,
                        body: SubmissionBody::Write { entry },
                    };
                    let submission = conv.admit(tx, draft).await?;
                    json!({"submissionId": submission})
                };
                outcome = TaskOutcome::Completed { result };
            }
            let address = resolve_address(&LIVE_DOC, Some(conv.id), None)?;
            let doc = tx.doc(&LIVE_DOC, &address, None).await?;
            let mut items = serde_json::to_value(doc.value()?.to_json())?["compactions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            items.retain(|s| s["taskId"] != self.record.id);
            if items.is_empty() {
                doc.delete(&[Segment::Key("compactions".into())])?;
            } else {
                doc.set(&[Segment::Key("compactions".into())], json!(items).into())?;
            }
            match &outcome {
                TaskOutcome::Failed { error, .. } => doc.set(
                    &[Segment::Key("compactionFailure".into())],
                    json!({
                        "taskId": self.record.id,
                        "trigger": self.record.input["reason"],
                        "error": error.message
                    })
                    .into(),
                )?,
                TaskOutcome::Completed { result }
                    if result.get("entryId").is_some() || result.get("submissionId").is_some() =>
                {
                    doc.delete(&[Segment::Key("compactionFailure".into())])?;
                }
                _ => {}
            }
            Ok(TaskState::Terminal { outcome })
        })
        .await
    }
}

/// Use the last measured assistant usage after the current head, then estimate trailing messages.
pub fn estimate_context(view: &ContextView, extra: &[EntryDraft]) -> Result<f64> {
    use pi_rs_ai::simple_options::calculate_context_tokens;
    let mut from = 0;
    let mut tokens = 0.0;
    'entries: for (entry, contribution) in view.entries.iter().zip(&view.contributions).rev() {
        if view.head.as_ref().is_some_and(|head| entry.id <= head.id) {
            break;
        }
        for message in contribution
            .iter()
            .rev()
            .filter(|m| m["role"] == "assistant")
        {
            let usage = serde_json::from_value(message["usage"].clone())?;
            let measured = calculate_context_tokens(&usage);
            if measured > 0.0 {
                tokens = measured;
                from = view
                    .messages
                    .iter()
                    .rposition(|m| m == message)
                    .map_or(0, |i| i + 1);
                break 'entries;
            }
        }
    }
    for message in view.messages[from..]
        .iter()
        .chain(extra.iter().flat_map(|e| e.model.iter().flatten()))
    {
        tokens += estimate_message_tokens(&serde_json::from_value(message.clone())?) as f64;
    }
    Ok(tokens)
}
