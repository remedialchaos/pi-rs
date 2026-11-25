use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use pi_rs_ai::{
    AssistantMessage, StopReason, ToolResultMessage, ToolResultRole, utils::validate_tool_arguments,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{
    AfterToolCallContext, AgentContext, AgentEvent, AgentEventSink, AgentMessage, AgentTool,
    AgentToolCall, AgentToolCallOutcome, AgentToolResult, BeforeToolCallContext, CancellationToken,
    Result, ToolCallHooks, ToolExecutionMode, types::unix_millis,
};

pub type ToolUpdateSink<'a> = dyn Fn(AgentToolResult) -> BoxFuture<'a, Result<()>> + Sync + 'a;

pub struct RunToolCallOptions<'a> {
    pub tools: &'a [Arc<dyn AgentTool>],
    pub assistant_message: &'a AssistantMessage,
    pub context: &'a AgentContext,
    pub hooks: &'a ToolCallHooks,
    pub signal: &'a CancellationToken,
    pub on_update: Option<&'a ToolUpdateSink<'a>>,
}

/// Runs a nested tool through the same validation and policy hooks as a model-issued call.
/// Tool failures are returned as outcomes, not loop failures.
pub async fn run_tool_call(
    call: &AgentToolCall,
    options: RunToolCallOptions<'_>,
) -> AgentToolCallOutcome {
    let run = ToolRun {
        context: options.context,
        assistant: options.assistant_message,
        hooks: options.hooks,
        signal: options.signal,
    };
    match run.prepare(call, options.tools).await {
        Preparation::Immediate(outcome) => outcome,
        Preparation::Ready(prepared) => match run.execute(prepared, options.on_update).await {
            Ok(outcome) => outcome,
            Err(error) => failure(call, error.to_string()),
        },
    }
}

struct ToolRun<'a> {
    context: &'a AgentContext,
    assistant: &'a AssistantMessage,
    hooks: &'a ToolCallHooks,
    signal: &'a CancellationToken,
}

struct Prepared<'a> {
    tool: &'a dyn AgentTool,
    call: &'a AgentToolCall,
    args: Value,
}

enum Preparation<'a> {
    Immediate(AgentToolCallOutcome),
    Ready(Prepared<'a>),
}

impl ToolRun<'_> {
    async fn prepare<'a>(
        &self,
        call: &'a AgentToolCall,
        tools: &'a [Arc<dyn AgentTool>],
    ) -> Preparation<'a> {
        if self.signal.is_cancelled() {
            return Preparation::Immediate(failure(call, "Operation aborted"));
        }
        let Some(tool) = tools
            .iter()
            .find(|tool| tool.declaration().name == call.name)
        else {
            return Preparation::Immediate(failure(call, format!("Tool {} not found", call.name)));
        };
        let mut prepared_call = call.clone();
        let args = tool
            .prepare_arguments(&mut prepared_call.arguments)
            .and_then(|()| {
                validate_tool_arguments(tool.declaration(), &prepared_call).map_err(Into::into)
            });
        let mut args = match args {
            Ok(args) => args,
            Err(error) => return Preparation::Immediate(failure(call, error.to_string())),
        };
        if let Some(before) = &self.hooks.before_tool_call {
            let context = BeforeToolCallContext {
                assistant_message: self.assistant,
                tool_call: call,
                args: &mut args,
                context: self.context,
            };
            match before(context, self.signal).await {
                Err(error) => return Preparation::Immediate(failure(call, error.to_string())),
                _ if self.signal.is_cancelled() => {
                    return Preparation::Immediate(failure(call, "Operation aborted"));
                }
                Ok(Some(decision)) if decision.block => {
                    let mut result = error_result(
                        decision
                            .reason
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "Tool execution was blocked".into()),
                    );
                    result.terminate = decision.terminate.then_some(true);
                    return Preparation::Immediate(AgentToolCallOutcome {
                        tool_call: call.clone(),
                        result: Arc::new(result),
                        is_error: true,
                        duration_ms: None,
                    });
                }
                Ok(_) => {}
            }
        }
        Preparation::Ready(Prepared {
            tool: tool.as_ref(),
            call,
            args,
        })
    }

    async fn execute(
        &self,
        prepared: Prepared<'_>,
        on_update: Option<&ToolUpdateSink<'_>>,
    ) -> Result<AgentToolCallOutcome> {
        let Prepared { tool, call, args } = prepared;
        let (executed, duration_ms) = if let Some(on_update) = on_update {
            let (send, mut receive) = mpsc::unbounded_channel();
            // Closing the sender slot at settlement also invalidates copies retained by the tool.
            let slot = Arc::new(Mutex::new(Some(send)));
            let update = Arc::new({
                let slot = slot.clone();
                move |result| {
                    if let Some(tx) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        let _ = tx.send(result);
                    }
                }
            });
            let execute = async {
                let start = Instant::now();
                let result = tool.execute(&call.id, &args, self.signal, update).await;
                let duration = elapsed_ms(start);
                slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                (result, duration)
            };
            let emit_updates = async {
                let mut pending = FuturesUnordered::new();
                let mut closed = false;
                let mut first_error = None;
                loop {
                    tokio::select! {
                        biased;
                        result = pending.next(), if !pending.is_empty() => {
                            if let Some(Err(error)) = result {
                                first_error.get_or_insert(error);
                            }
                        }
                        partial = receive.recv(), if !closed => {
                            if let Some(partial) = partial {
                                pending.push(on_update(partial));
                            } else {
                                closed = true;
                            }
                        }
                        else => return first_error.map_or(Ok(()), Err),
                    }
                }
            };
            let (executed, emitted) = tokio::join!(execute, emit_updates);
            emitted?;
            executed
        } else {
            let start = Instant::now();
            let result = tool
                .execute(&call.id, &args, self.signal, Arc::new(|_| {}))
                .await;
            (result, elapsed_ms(start))
        };
        let (mut result, mut is_error) = match executed {
            Ok(result) => {
                let is_error = result.is_error == Some(true);
                (result, is_error)
            }
            Err(error) => (error_result(error.to_string()), true),
        };
        if let Some(after) = &self.hooks.after_tool_call {
            let context = AfterToolCallContext {
                assistant_message: self.assistant,
                tool_call: call,
                args: &args,
                result: &result,
                is_error,
                context: self.context,
            };
            match after(context, self.signal).await {
                Ok(Some(patch)) => {
                    if let Some(content) = patch.content {
                        result.content = content;
                        result.structured_content = None;
                    }
                    if let Some(structured) =
                        patch.structured_content.filter(|value| !value.is_null())
                    {
                        result.structured_content = Some(structured);
                    }
                    if let Some(details) = patch.details.filter(|value| !value.is_null()) {
                        result.details = Some(details);
                    }
                    if patch.usage.is_some() {
                        result.usage = patch.usage;
                    }
                    if patch.terminate.is_some() {
                        result.terminate = patch.terminate;
                    }
                    is_error = patch.is_error.unwrap_or(is_error);
                }
                Ok(None) => {}
                Err(error) => {
                    result = error_result(error.to_string());
                    is_error = true;
                }
            }
        }
        Ok(AgentToolCallOutcome {
            tool_call: call.clone(),
            result: Arc::new(result),
            is_error,
            duration_ms: Some(duration_ms),
        })
    }

    async fn execute_started(
        &self,
        prepared: Prepared<'_>,
        emit: &AgentEventSink<'_>,
    ) -> Result<AgentToolCallOutcome> {
        let call = prepared.call;
        let update = |partial_result| {
            emit(AgentEvent::ToolExecutionUpdate {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                args: call.arguments.clone(),
                partial_result,
            })
        };
        self.execute(prepared, Some(&update)).await
    }
}

pub(crate) struct ToolBatch {
    pub messages: Vec<Arc<ToolResultMessage>>,
    pub terminate: bool,
}

pub(crate) async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    calls: &[&AgentToolCall],
    config: &crate::AgentLoopConfig,
    signal: &CancellationToken,
    emit: &AgentEventSink<'_>,
) -> Result<ToolBatch> {
    let run = ToolRun {
        context,
        assistant,
        hooks: &config.hooks.tool_calls,
        signal,
    };
    let mut batch = ToolBatch {
        messages: Vec::with_capacity(calls.len()),
        terminate: !calls.is_empty(),
    };
    let truncated = assistant.stop_reason == StopReason::Length;
    let sequential = truncated
        || config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context
                .tools
                .iter()
                .find(|tool| tool.declaration().name == call.name)
                .is_some_and(|tool| tool.execution_mode() == Some(ToolExecutionMode::Sequential))
        });
    if sequential {
        for call in calls {
            emit_start(call, emit).await?;
            let outcome = if truncated {
                failure(
                    call,
                    format!(
                        "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                        call.name
                    ),
                )
            } else {
                match run.prepare(call, &context.tools).await {
                    Preparation::Immediate(outcome) => outcome,
                    Preparation::Ready(prepared) => run.execute_started(prepared, emit).await?,
                }
            };
            emit_end(&outcome, emit).await?;
            batch.record(outcome, emit).await?;
        }
        return Ok(batch);
    }

    // All policy/validation runs in source order before any allowed tool executes. The execution
    // events then follow completion order, but the persisted artifacts remain in source order.
    let mut ordered: Vec<Option<AgentToolCallOutcome>> = (0..calls.len()).map(|_| None).collect();
    let mut executing = FuturesUnordered::new();
    for (index, call) in calls.iter().copied().enumerate() {
        emit_start(call, emit).await?;
        match run.prepare(call, &context.tools).await {
            Preparation::Immediate(outcome) => {
                emit_end(&outcome, emit).await?;
                ordered[index] = Some(outcome);
            }
            Preparation::Ready(prepared) => {
                let run = &run;
                executing.push(async move {
                    let outcome = if signal.is_cancelled() {
                        failure(call, "Operation aborted")
                    } else {
                        run.execute_started(prepared, emit).await?
                    };
                    emit_end(&outcome, emit).await?;
                    Ok::<_, crate::AgentError>((index, outcome))
                });
            }
        }
    }
    // Don't drop already-running tools if one listener fails; allow every execution to settle.
    let mut first_error = None;
    while let Some(result) = executing.next().await {
        match result {
            Ok((index, outcome)) => ordered[index] = Some(outcome),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    for outcome in ordered.into_iter().flatten() {
        batch.record(outcome, emit).await?;
    }
    Ok(batch)
}

impl ToolBatch {
    async fn record(
        &mut self,
        outcome: AgentToolCallOutcome,
        emit: &AgentEventSink<'_>,
    ) -> Result<()> {
        let result = Arc::unwrap_or_clone(outcome.result);
        self.terminate &= result.terminate == Some(true);
        let message = Arc::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: outcome.tool_call.id,
            tool_name: outcome.tool_call.name,
            content: result.content,
            details: result.details,
            usage: result.usage,
            nested_calls: None,
            is_error: outcome.is_error,
            timestamp: unix_millis(),
            duration_ms: outcome.duration_ms,
        });
        let artifact = AgentMessage::ToolResult(message.clone());
        emit(AgentEvent::MessageStart {
            message: artifact.clone(),
        })
        .await?;
        emit(AgentEvent::MessageEnd { message: artifact }).await?;
        self.messages.push(message);
        Ok(())
    }
}

async fn emit_start(call: &AgentToolCall, emit: &AgentEventSink<'_>) -> Result<()> {
    emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    })
    .await
}

async fn emit_end(outcome: &AgentToolCallOutcome, emit: &AgentEventSink<'_>) -> Result<()> {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        result: outcome.result.clone(),
        is_error: outcome.is_error,
        duration_ms: outcome.duration_ms,
    })
    .await
}

fn failure(call: &AgentToolCall, error: impl Into<String>) -> AgentToolCallOutcome {
    AgentToolCallOutcome {
        tool_call: call.clone(),
        result: Arc::new(error_result(error)),
        is_error: true,
        duration_ms: None,
    }
}

fn error_result(error: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        details: Some(Value::Object(Default::default())),
        ..AgentToolResult::text(error)
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from((start.elapsed().as_micros() + 500) / 1000).unwrap_or(u64::MAX)
}
