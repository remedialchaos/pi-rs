use super::{UsageBucket, record_usage};
use crate::{
    ConversationId, DocDefinition, DocFamilyToken, EntryDraft, EntryRecord, JsonObject,
    documents::{DocumentSemantics, InitialDocument},
    session::{Result, Transaction},
    tools::{DiagnosticSeverity, ToolDiagnostic, ToolResult},
};
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

pub static NESTED_CALL_DOC: LazyLock<DocFamilyToken> = LazyLock::new(|| {
    Arc::new(DocDefinition {
        kind: "pi.tool.nested-call".into(),
        version: 1,
        semantics: DocumentSemantics::Task,
        initial: InitialDocument::Family(Arc::new(|_| Ok(JsonObject::new()))),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| false),
    })
});

pub fn harness_error(code: &str, message: &str) -> ToolResult {
    ToolResult {
        output: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![ToolDiagnostic::new(
            DiagnosticSeverity::Error,
            Some(code),
            message,
        )]),
        ..Default::default()
    }
}

pub async fn append_tool_result(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    call_id: &str,
    name: &str,
    result: ToolResult,
    timestamp: u64,
    duration_ms: Option<u64>,
) -> Result<EntryRecord> {
    let diagnostics = result.diagnostics.unwrap_or_default();
    let mut content = result
        .output
        .unwrap_or_default()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !diagnostics.is_empty() {
        let lines = diagnostics
            .iter()
            .map(|d| {
                let severity = match d.severity {
                    DiagnosticSeverity::Info => "info",
                    DiagnosticSeverity::Warn => "warn",
                    DiagnosticSeverity::Error => "error",
                };
                format!("[{severity}] {}", d.message)
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.push(json!({
            "type": "text",
            "text": format!("<harness>\n{lines}\n</harness>")
        }));
    }
    let mut msg = json!({
        "role": "toolResult",
        "toolCallId": call_id,
        "toolName": name,
        "content": content,
        "isError": result.is_error.unwrap_or(false),
        "timestamp": timestamp
    });
    if let Some(details) = result.details {
        msg["details"] = details;
    }
    if let Some(usage) = result.usage {
        record_usage(tx, conversation, UsageBucket::Tools, name, &usage).await?;
        msg["usage"] = serde_json::to_value(usage)?;
    }
    if let Some(duration) = duration_ms {
        msg["durationMs"] = Value::from(duration);
    }
    tx.append_entry(
        conversation,
        EntryDraft {
            kind: "pi.tool-result".into(),
            model: Some(vec![msg]),
            data: Some(json!({"diagnostics":diagnostics})),
            head: None,
            edits: None,
        },
    )
    .await
}

use super::{Agent, Invocation, tool_extension};
use crate::{
    TaskOutcome, TaskState,
    session::Error,
    tasks::{MigratedTask, PhaseHandler, Task, TaskDefinition},
};
use pi_rs_ai::types::ToolCall;
use pi_rs_chord::context::Context;
use pi_rs_telemetry::TelemetrySpan;

pub fn tool_task() -> Arc<Task<Invocation>> {
    let handler: PhaseHandler<Invocation> = Arc::new(|_, inv, ctx| {
        Box::pin(async move { inv.run_tool(ctx).await.map_err(Into::into) })
    });
    Arc::new(crate::define_task(TaskDefinition {
        name: "pi.tool".into(),
        version: 4,
        initial: Arc::new(|_| Ok(json!({"phase":"call"}))),
        phases: ["call", "execute", "parked"]
            .into_iter()
            .map(|p| (p.into(), handler.clone()))
            .collect(),
        abort: handler,
        migrate: Some(Arc::new(|mut input, checkpoint, from| {
            if from == 1 {
                input["kind"] = json!("model");
            }
            Ok(MigratedTask { input, checkpoint })
        })),
        hooks: (),
    }))
}

impl Invocation {
    /// Report a failure of the tool's code as an `extension_error`, naming the extension that
    /// provides or wraps the tool when the agent resolves one.
    fn report_tool_error(&self, agent: &Agent, name: &str, message: String) {
        match tool_extension(agent, name) {
            Some(extension) => self.report_hook_error(extension, vec![message]),
            None => self
                .harness
                .session
                .harness_report(crate::session::ReportKind::Extension, &message),
        }
    }

    async fn run_tool(&self, ctx: &Context) -> Result<()> {
        let conversation = self.harness.handle(self.record.conversation_id);
        let input = &self.record.input;
        let nested = input["kind"] == "nested";
        let call: ToolCall = if nested {
            serde_json::from_value(input["call"].clone())?
        } else {
            let assistant = input["assistant"]
                .as_u64()
                .ok_or_else(|| Error::invalid("Tool call has no assistant entry"))?;
            let stored = self
                .harness
                .session
                .entry(assistant, ctx)
                .await?
                .ok_or_else(|| Error::invalid("Missing assistant entry"))?;
            serde_json::from_value(
                stored
                    .entry
                    .model
                    .iter()
                    .flatten()
                    .flat_map(|m| m["content"].as_array().into_iter().flatten())
                    .find(|c| c["type"] == "toolCall" && c["id"] == input["callId"])
                    .ok_or_else(|| Error::invalid("Missing assistant tool call"))?
                    .clone(),
            )?
        };
        ctx.telemetry().set_attributes(
            [
                ("tool.name".into(), call.name.clone().into()),
                ("tool.call_id".into(), call.id.clone().into()),
            ]
            .into(),
        );
        let agent = conversation.agent(ctx).await?;
        let tool = (if nested {
            &agent.callable
        } else {
            &agent.tools
        })
        .iter()
        .find(|t| t.declaration.name == call.name && input["unoffered"] != true);
        let TaskState::Running { checkpoint, wake } = &self.record.state else {
            return Err(Error::invalid("Tool must be running"));
        };
        let cutoff = self.restart_cutoff();
        let cutoff_error = cutoff.as_ref().map(|(frequency, _)| {
            format!(
                "Tool {} was interrupted {frequency}; it is not run again",
                call.name
            )
        });
        let result = if self.record.abort_requested {
            if (checkpoint["phase"] == "parked" || checkpoint.get("wake").is_some())
                && let Some(cancel) = tool.and_then(|t| t.cancel.as_ref())
                && let Err(error) = cancel(checkpoint["arguments"].clone(), self, ctx).await
            {
                ctx.check()?;
                self.report_tool_error(&agent, &call.name, error.to_string());
            }
            let (code, message) = if input["unoffered"] == true {
                (
                    "tool_unavailable",
                    format!("Tool {} is not available", call.name),
                )
            } else if self.record.abort_reason != Some(crate::TaskAbortReason::Restart) {
                ("aborted", format!("Tool {} was aborted", call.name))
            } else {
                match checkpoint["phase"].as_str() {
                    Some("execute") => (
                        "interrupted",
                        format!(
                            "Tool {} was interrupted by a restart and may have partially run",
                            call.name
                        ),
                    ),
                    Some("parked") => (
                        "abandoned",
                        format!(
                            "Tool {} was parked: its caller ended with a restart",
                            call.name
                        ),
                    ),
                    _ => (
                        "abandoned",
                        format!(
                            "Tool {} was not started: its caller ended with a restart",
                            call.name
                        ),
                    ),
                }
            };
            harness_error(code, &message)
        } else if let Some(message) = &cutoff_error {
            harness_error("restart_limit", message)
        } else if let Some(tool) = tool {
            if checkpoint["phase"] == "call" {
                let mut checked_call = call.clone();
                let mut denied = None;
                if let Some(repair) = &tool.prepare_arguments {
                    match repair(serde_json::to_value(&checked_call.arguments)?)
                        .and_then(|v| Ok(serde_json::from_value(v)?))
                    {
                        Ok(args) => checked_call.arguments = args,
                        Err(error) => denied = Some(error.to_string()),
                    }
                }
                for hook in agent.hooks("pi.tool", "beforeTool") {
                    match (hook.1)(serde_json::to_value(&checked_call)?, self, ctx).await {
                        Ok(Some(decision)) => {
                            if let Some(block) = decision["block"].as_str() {
                                denied = Some(block.to_owned());
                                break;
                            }
                            if let Some(sleep) = decision.get("sleep") {
                                let signals = serde_json::from_value(sleep["signals"].clone())?;
                                let state = TaskState::Sleeping {
                                    checkpoint: json!({"phase":"call"}),
                                    until: sleep["until"].as_u64(),
                                    signals,
                                };
                                return self.commit(ctx, async |_, _| Ok(state)).await;
                            }
                            if let Some(args) = decision.get("arguments") {
                                checked_call.arguments = serde_json::from_value(args.clone())?;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            ctx.check()?;
                            denied = Some(error.to_string());
                            break;
                        }
                    }
                }
                if let Some(reason) = denied {
                    harness_error("blocked", &format!("Tool call blocked: {reason}"))
                } else {
                    match pi_rs_ai::utils::validate_tool_arguments(&tool.declaration, &checked_call)
                    {
                        Ok(args) => {
                            return self
                                .commit(ctx, async |_, _| {
                                    Ok(TaskState::Running {
                                        checkpoint: json!({
                                            "phase": "execute",
                                            "arguments": args,
                                            "replay": if tool.replay_safe{"safe"}else{"unsafe"}
                                        }),
                                        wake: None,
                                    })
                                })
                                .await;
                        }
                        Err(e) => harness_error("invalid_arguments", &e.to_string()),
                    }
                }
            } else if checkpoint["phase"] == "parked" {
                let mut intent = checkpoint.clone();
                intent["phase"] = json!("execute");
                intent["replay"] = json!(if tool.replay_safe { "safe" } else { "unsafe" });
                if let Some(reason) = wake {
                    intent["wake"] = serde_json::to_value(reason)?;
                }
                return self
                    .commit(ctx, async |_, _| {
                        Ok(TaskState::Running {
                            checkpoint: intent,
                            wake: None,
                        })
                    })
                    .await;
            } else if self.record.restarts.unwrap_or(0) > 0
                && (checkpoint["replay"] != "safe" || !tool.replay_safe)
            {
                harness_error(
                    "interrupted",
                    &format!(
                        "Tool {} was interrupted and may have partially run",
                        call.name
                    ),
                )
            } else {
                if let Some(limits) = tool.output_limits {
                    *self.progress.lock().unwrap_or_else(|e| e.into_inner()) =
                        super::tool_progress::Progress::new(limits);
                }
                let run = (tool.execute)(checkpoint["arguments"].clone(), self, ctx);
                tokio::pin!(run);
                let interval = std::time::Duration::from_millis(
                    (self.harness.settings)()["progress"]["outputIntervalMs"]
                        .as_u64()
                        .unwrap_or(100),
                );
                let mut dirty = false;
                let mut deadline = tokio::time::Instant::now();
                let execution = loop {
                    tokio::select! {
                        result=&mut run=>break result,
                        ()=self.progress_notice.notified()=>{if !dirty{deadline=tokio::time::Instant::now()+interval;dirty=true;}},
                        ()=tokio::time::sleep_until(deadline),if dirty=>{self.publish_tool_progress(ctx).await?;dirty=false;},
                    }
                };
                match execution {
                    Ok(super::ToolExecution::Result(result)) => *result,
                    Ok(super::ToolExecution::Park(park)) => {
                        return self
                            .commit(ctx, async |_, _| {
                                Ok(park
                                    .state(checkpoint["arguments"].clone(), &checkpoint["replay"]))
                            })
                            .await;
                    }
                    Err(e) => {
                        ctx.check()?;
                        harness_error("tool_error", &e.to_string())
                    }
                }
            }
        } else {
            harness_error(
                "tool_unavailable",
                &format!("Tool {} is not available", call.name),
            )
        };
        let mut result = result;
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(&mut result);
        let run_after_tool = if self.record.abort_requested {
            let limit = (self.harness.settings)()["restartLimit"]
                .as_u64()
                .unwrap_or(super::DEFAULT_RESTART_LIMIT);
            self.record.restarts.unwrap_or(0) < limit.saturating_sub(1)
        } else {
            cutoff.as_ref().is_none_or(|(_, salvage)| *salvage)
        };
        if run_after_tool {
            for hook in agent.hooks("pi.tool", "afterTool") {
                let change = (hook.1)(json!({"call":call,"result":result}), self, ctx)
                    .await
                    .and_then(|value| {
                        value
                            .map(serde_json::from_value)
                            .transpose()
                            .map_err(Error::from)
                    });
                match change {
                    Ok(Some(next)) => result = next,
                    Ok(None) => {}
                    Err(error) => {
                        ctx.check()?;
                        self.report_hook_error(&hook.0, vec![error.to_string()]);
                    }
                }
            }
        }
        if let Some(tool) = tool {
            use pi_rs_ai::types::InputContent;
            use pi_rs_chord::output::{OutputLimits, Retain, bound_output};
            let limits = tool.output_limits.unwrap_or(OutputLimits {
                max_bytes: crate::truncate::DEFAULT_MAX_BYTES,
                max_lines: crate::truncate::DEFAULT_MAX_LINES,
                retain: Retain::Head,
            });
            let output = result.output.get_or_insert_with(Vec::new);
            let joined = output
                .iter()
                .filter_map(|block| match block {
                    InputContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            let bounded = bound_output(&joined.into(), limits);
            if bounded.dropped_bytes > 0 {
                let selected = match limits.retain {
                    Retain::Head => output
                        .iter()
                        .position(|b| matches!(b, InputContent::Text(_))),
                    Retain::Tail => output
                        .iter()
                        .rposition(|b| matches!(b, InputContent::Text(_))),
                };
                *output = std::mem::take(output)
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, b)| match b {
                        InputContent::Text(mut t) if Some(i) == selected => {
                            t.text = String::from_utf16_lossy(&bounded.text.0);
                            Some(InputContent::Text(t))
                        }
                        InputContent::Text(_) => None,
                        other => Some(other),
                    })
                    .collect();
                let kept = if limits.retain == Retain::Head {
                    "beginning"
                } else {
                    "end"
                };
                result
                    .diagnostics
                    .get_or_insert_with(Vec::new)
                    .push(ToolDiagnostic::new(
                        DiagnosticSeverity::Warn,
                        Some("truncated"),
                        format!(
                            "Output truncated to its {kept}: {} lines, {} bytes dropped",
                            bounded.dropped_lines, bounded.dropped_bytes
                        ),
                    ));
            }
            let name = &tool.declaration.name;
            let broken = match (&tool.structured_output_schema, &result.structured_output) {
                (None, Some(_)) => Some(format!(
                    "Tool {name} returned structuredOutput but declares no structuredOutputSchema"
                )),
                (Some(_), None) if result.is_error != Some(true) => {
                    Some(format!("Tool {name} returned no structuredOutput"))
                }
                (Some(schema), Some(value)) => {
                    let options = pi_rs_schema::CompileOptions::new()
                        .with_formats(pi_rs_schema::formats::formats())
                        .with_unresolved_refs(pi_rs_schema::UnresolvedRefs::Reject);
                    let validator = pi_rs_schema::compile_with(schema, &options)
                        .map_err(|e| Error::invalid(e.to_string()))?;
                    validator.validate(&pi_rs_schema::Value::from(value)).err().and_then(|errors|errors.into_iter().next()).map(|e|{
                        let path=e.path.trim_start_matches('/').split('/').map(|p|p.replace("~1","/").replace("~0","~")).collect::<Vec<_>>().join(".");
                        let path = if path.is_empty() { "root" } else { &path };
                        format!("Tool {name} returned structuredOutput that does not match its schema: {path}: {}", e.message)
                    })
                }
                _ => None,
            };
            if let Some(error) = broken {
                result.structured_output = None;
                if nested {
                    result.is_error = Some(true);
                    result
                        .diagnostics
                        .get_or_insert_with(Vec::new)
                        .push(ToolDiagnostic::new(
                            DiagnosticSeverity::Error,
                            Some("invalid_structured_output"),
                            error,
                        ));
                } else {
                    // The model never sees structured output, so its call stands; the host
                    // learns of the broken tool and the extension that owns it.
                    self.report_tool_error(&agent, &call.name, error);
                }
            }
        }
        self.commit(ctx, async |tx, _| {
            let now = (self.harness.now)();
            let duration = self
                .record
                .started_at
                .map(|start| now.saturating_sub(start));
            let mut entry_id = None;
            let value = if nested {
                let parent = input["parent"]
                    .as_u64()
                    .ok_or_else(|| Error::invalid("Missing nested caller"))?;
                let key = input["key"]
                    .as_str()
                    .ok_or_else(|| Error::invalid("Missing nested key"))?;
                let address = crate::documents::resolve_address(
                    &NESTED_CALL_DOC,
                    Some(parent),
                    Some(key.into()),
                )?;
                let mut receipt = serde_json::to_value(&result)?;
                let fields = receipt
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid("Invalid tool result"))?;
                let output = fields.remove("output").unwrap_or_else(|| json!([]));
                fields.remove("control");
                fields.insert("taskId".into(), json!(self.record.id));
                fields.entry("isError").or_insert(json!(false));
                fields.entry("diagnostics").or_insert(json!([]));
                if !fields.contains_key("structuredOutput")
                    && tool.is_none_or(|t| t.structured_output_schema.is_none())
                {
                    let structured = match output.as_array().map(Vec::as_slice) {
                        Some([]) => json!(""),
                        Some([only]) if only["type"] == "text" => only["text"].clone(),
                        Some([only]) => only.clone(),
                        _ => output,
                    };
                    fields.insert("structuredOutput".into(), structured);
                }
                if let Some(duration) = duration {
                    fields.insert("durationMs".into(), json!(duration));
                }
                if let Some(usage) = result.usage {
                    record_usage(tx, conversation.id, UsageBucket::Tools, &call.name, &usage)
                        .await?;
                }
                let member = tx
                    .doc(&NESTED_CALL_DOC, &address, Some(&Value::Null))
                    .await?;
                if serde_json::to_value(member.value()?.to_json())?["taskId"] != self.record.id {
                    return Err(Error::invalid(
                        "Nested call is absent from its caller index",
                    ));
                }
                member.set(
                    &[pi_rs_chord::delta::Segment::Key("result".into())],
                    receipt.into(),
                )?;
                json!({"kind":"nested"})
            } else {
                let control = result.control.clone();
                let entry = append_tool_result(
                    tx,
                    conversation.id,
                    &call.id,
                    &call.name,
                    result,
                    now,
                    duration,
                )
                .await?;
                entry_id = Some(entry.id);
                let mut value = json!({"kind":"model","entryId":entry.id});
                if let Some(control) = control {
                    value["control"] = serde_json::to_value(control)?;
                }
                value
            };
            let address =
                crate::documents::resolve_address(&super::LIVE_DOC, Some(conversation.id), None)?;
            let live = tx.doc(&super::LIVE_DOC, &address, None).await?;
            let key = if nested { "nestedTools" } else { "tools" };
            let mut slots = serde_json::to_value(live.value()?.to_json())?[key]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(slot) = slots.iter_mut().find(|s| s["taskId"] == self.record.id) {
                slot["status"] = json!("done");
                if let Some(id) = entry_id {
                    slot["entry"] = json!(id);
                }
                live.set(
                    &[pi_rs_chord::delta::Segment::Key(key.into())],
                    json!(slots).into(),
                )?;
            }
            Ok(TaskState::Terminal {
                outcome: if self.record.abort_requested {
                    TaskOutcome::Aborted {
                        reason: None,
                        result: Some(value),
                    }
                } else if let Some(message) = cutoff_error {
                    TaskOutcome::Failed {
                        error: crate::TaskOutcomeError {
                            message,
                            detail: Some(json!({"reason":"restart_limit"})),
                        },
                        result: Some(value),
                    }
                } else {
                    TaskOutcome::Completed { result: value }
                },
            })
        })
        .await
    }
}

impl Invocation {
    pub async fn execute_tool(
        &self,
        key: &str,
        name: &str,
        args: crate::JsonObject,
        progress: bool,
        abandon_on_restart: bool,
        ctx: &Context,
    ) -> Result<Value> {
        check_key(key)?;
        let parent_call = self.record.input["callId"]
            .as_str()
            .or_else(|| self.record.input["call"]["id"].as_str())
            .ok_or_else(|| Error::invalid("Nested tool execution requires a tool caller"))?;
        let call = json!({
            "type": "toolCall",
            "name": name,
            "id": format!("{parent_call}/{key}"),
            "arguments": args
        });
        let parent = self.record.id;
        let address =
            crate::documents::resolve_address(&NESTED_CALL_DOC, Some(parent), Some(key.into()))?;
        let (id, replayed) = self.harness.session.commit(async |tx| {
            let current = tx
                .task(parent)
                .await?
                .ok_or_else(|| Error::invalid("Missing nested caller"))?;
            if current.state != self.record.state || current.abort_requested {
                return Err(Error::invalid("Nested caller invocation ended"));
            }
            let member = tx
                .doc(&NESTED_CALL_DOC, &address, Some(&Value::Null))
                .await?;
            let prior = serde_json::to_value(member.value()?.to_json())?;
            if let Some(id) = prior["taskId"].as_u64() {
                let existing = tx
                    .task(id)
                    .await?
                    .ok_or_else(|| Error::invalid("Missing keyed task"))?;
                if existing.input["kind"] != "nested"
                    || existing.input["call"]["name"] != name
                    || existing.input["call"]["arguments"] != call["arguments"]
                {
                    return Err(Error::invalid(format!(
                        "Nested call {} was already made with another tool or other arguments",
                        call["id"]
                    )));
                }
                return Ok((id, prior.get("result").is_some()));
            }
            let mut input =
                json!({
                    "kind": "nested",
                    "parent": parent,
                    "parentCallId": parent_call,
                    "key": key,
                    "call": call
                });
            if !progress {
                input["progress"] = json!(false);
            }
            let mut options =
                crate::session::TaskOptions::new(crate::TaskOwnership::Task { task_id: parent });
            options.abandon_on_restart = abandon_on_restart;
            let id = tx.create_task(tool_task().as_ref(), input, options).await?;
            let member = tx
                .doc(&NESTED_CALL_DOC, &address, Some(&Value::Null))
                .await?;
            member.set(
                &[pi_rs_chord::delta::Segment::Key("kind".into())],
                json!("nested").into(),
            )?;
            member.set(
                &[pi_rs_chord::delta::Segment::Key("taskId".into())],
                json!(id).into(),
            )?;
            let live_addr = crate::documents::resolve_address(
                &super::LIVE_DOC,
                Some(self.record.conversation_id),
                None,
            )?;
            let live = tx.doc(&super::LIVE_DOC, &live_addr, None).await?;
            let mut slots = serde_json::to_value(live.value()?.to_json())?["nestedTools"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            slots.push(json!({
                "callId": call["id"],
                "parentCallId": parent_call,
                "parentTaskId": parent,
                "name": name,
                "taskId": id,
                "arguments": args,
                "status": "pending"
            }));
            live.set(
                &[pi_rs_chord::delta::Segment::Key("nestedTools".into())],
                json!(slots).into(),
            )?;
            Ok((id, false))
        }, ctx).await?;
        if replayed {
            ctx.telemetry().add_event(
                "pi.durable.replay",
                [("tool.name".into(), name.into())].into(),
            );
        }
        let task = self.harness.wait_for_task(id, ctx).await?;
        if let Some(result) = self
            .harness
            .snapshot(&NESTED_CALL_DOC, &address, ctx)
            .await?
            .and_then(|s| s.get("result").cloned())
        {
            return Ok(result);
        }
        Err(Error::invalid(format!(
            "Nested tool {name} ended without a result: {:?}",
            task.state
        )))
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.contains('/')
        || key == "__proto__"
        || (!key.starts_with('0') && key.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Error::invalid(format!(
            "Nested call key {key:?} must be non-empty, without \"/\", not \"__proto__\", and not a positive integer"
        )));
    }
    Ok(())
}
impl Invocation {
    pub async fn env(&self, ctx: &Context) -> Result<Option<Arc<dyn crate::env::ExecutionEnv>>> {
        self.harness
            .build_env(self.record.conversation_id, ctx)
            .await
    }
    pub fn now(&self) -> u64 {
        (self.harness.now)()
    }
    /// The host's current `Models` with the Harness identity applied, read at each access.
    pub fn models(&self) -> pi_rs_ai::registry::Models {
        self.harness.models()
    }
    pub fn call_id(&self) -> Option<&str> {
        self.record.input["callId"]
            .as_str()
            .or_else(|| self.record.input["call"]["id"].as_str())
    }
    pub fn tool_checkpoint(&self) -> Option<&Value> {
        match &self.record.state {
            TaskState::Running { checkpoint, .. } => checkpoint.get("checkpoint"),
            _ => None,
        }
    }
    pub fn tool_wake(&self) -> Option<&Value> {
        match &self.record.state {
            TaskState::Running { checkpoint, .. } => checkpoint.get("wake"),
            _ => None,
        }
    }
    pub async fn entry(
        &self,
        id: crate::EntryId,
        ctx: &Context,
    ) -> Result<Option<crate::EntryRecord>> {
        Ok(self
            .harness
            .session
            .visible_entry(self.record.conversation_id, id, ctx)
            .await?
            .map(|stored| stored.entry))
    }
    pub async fn entries(
        &self,
        query: crate::EntryQuery,
        limit: usize,
        cursor: Option<&crate::Cursor>,
        ctx: &Context,
    ) -> Result<crate::Page<crate::EntryRecord>> {
        self.harness
            .handle(self.record.conversation_id)
            .entries(query, limit, cursor, ctx)
            .await
    }
    pub async fn create_task(
        &self,
        task: &Task<Invocation>,
        input: Value,
        key: Option<&str>,
        abandon_on_restart: Option<bool>,
        ctx: &Context,
    ) -> Result<crate::TaskId> {
        if let Some(key) = key {
            check_key(key)?;
        }
        if key.is_none() && self.tool_wake().is_some() {
            return Err(Error::invalid("A task created after a wake needs a key"));
        }
        let parent = self.record.id;
        let addr = key
            .map(|key| {
                crate::documents::resolve_address(&NESTED_CALL_DOC, Some(parent), Some(key.into()))
            })
            .transpose()?;
        self.harness
            .session
            .commit(
                async |tx| {
                    let current = tx
                        .task(parent)
                        .await?
                        .ok_or_else(|| Error::invalid("Missing caller"))?;
                    if current.state != self.record.state || current.abort_requested {
                        return Err(Error::invalid("Caller invocation ended"));
                    }
                    if let Some(address) = &addr {
                        let doc = tx
                            .doc(&NESTED_CALL_DOC, address, Some(&Value::Null))
                            .await?;
                        let prior = serde_json::to_value(doc.value()?.to_json())?;
                        if let Some(id) = prior["taskId"].as_u64() {
                            let existing = tx
                                .task(id)
                                .await?
                                .ok_or_else(|| Error::invalid("Missing keyed task"))?;
                            if prior["kind"] != "task"
                                || existing.kind != task.name()
                                || existing.input != input
                            {
                                return Err(Error::invalid(format!(
                                    "Key {key:?} already identifies another call or task"
                                )));
                            }
                            return Ok(id);
                        }
                    }
                    let mut options =
                        crate::session::TaskOptions::new(crate::TaskOwnership::Task {
                            task_id: parent,
                        });
                    options.abandon_on_restart = abandon_on_restart.unwrap_or(matches!(
                        &current.state,
                        TaskState::Running { checkpoint, .. }
                            if current.kind == "pi.tool" && checkpoint["replay"] != "safe"
                    ));
                    let id = tx.create_task(task, input, options).await?;
                    if let Some(address) = addr {
                        let doc = tx
                            .doc(&NESTED_CALL_DOC, &address, Some(&Value::Null))
                            .await?;
                        doc.set(
                            &[pi_rs_chord::delta::Segment::Key("taskId".into())],
                            json!(id).into(),
                        )?;
                        doc.set(
                            &[pi_rs_chord::delta::Segment::Key("kind".into())],
                            json!("task").into(),
                        )?;
                    }
                    Ok(id)
                },
                ctx,
            )
            .await
    }
}
