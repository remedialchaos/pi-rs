//! Nested tool calls initiated inside a running tool (`ctx.execute_tool()`), such as
//! `codemode` scripts or WASM extension tool handlers.
//!
//! Port of `packages/coding-agent/src/core/nested-tool-calls.ts`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use indexmap::IndexMap;
use pi_rs_agent::{
    AgentContext, AgentTool, AgentToolCall, AgentToolCallOutcome, AgentToolResult,
    CancellationToken, ToolCallHooks, ToolExecutionMode,
};
use pi_rs_agent::{RunToolCallOptions, ToolUpdateSink, run_tool_call};
use pi_rs_ai::faux::faux_assistant_message;
use pi_rs_ai::{
    InputContent, JsonObject, NestedToolCallRecord, NestedToolCalls, NestedToolStatus, Usage,
};
use serde_json::{Value, json};

use super::extensions::types::ToolOutcome;
use super::usage_totals::combine_usage;

pub const MAX_NESTED_CALLS: usize = 256;
pub const MAX_NESTED_ARG_BYTES_PER_CALL: usize = 8 * 1024;
pub const MAX_NESTED_ARG_BYTES_TOTAL: usize = 32 * 1024;
pub const MAX_NESTED_ERROR_CHARS: usize = 500;

/// What nested calls of one model-issued tool call leave on its `ToolResultMessage`.
#[derive(Clone, Debug, PartialEq)]
pub struct NestedCallSummary {
    pub calls: Option<NestedToolCalls>,
    pub usage: Option<Usage>,
}

/// Collects nested calls of one model-issued tool call, including grandchild calls.
pub struct NestedCallRecorder {
    calls: Vec<NestedToolCallRecord>,
    started_at: Vec<Instant>,
    complete: bool,
    argument_bytes: usize,
    usage: Option<Usage>,
}

impl Default for NestedCallRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl NestedCallRecorder {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            started_at: Vec::new(),
            complete: true,
            argument_bytes: 0,
            usage: None,
        }
    }

    /// Record a call as it starts. Returns `None` when the call cap is reached.
    pub fn start(&mut self, tool_call: &AgentToolCall) -> Option<usize> {
        if self.calls.len() >= MAX_NESTED_CALLS {
            self.complete = false;
            return None;
        }
        let encoded = serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "{}".into());
        let bytes = encoded.len();
        let (arguments, arguments_bytes) = if bytes > MAX_NESTED_ARG_BYTES_PER_CALL
            || self.argument_bytes + bytes > MAX_NESTED_ARG_BYTES_TOTAL
        {
            self.complete = false;
            (None, Some(bytes))
        } else {
            self.argument_bytes += bytes;
            (Some(tool_call.arguments.clone()), None)
        };
        let index = self.calls.len();
        self.calls.push(NestedToolCallRecord {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments,
            arguments_bytes,
            status: NestedToolStatus::Unfinished,
            duration_ms: None,
            error: None,
        });
        self.started_at.push(Instant::now());
        Some(index)
    }

    pub fn finish(&mut self, index: Option<usize>, is_error: bool, error_text: &str) {
        let Some(index) = index else {
            return;
        };
        let elapsed_ms = self
            .started_at
            .get(index)
            .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        if let Some(record) = self.calls.get_mut(index) {
            record.status = if is_error {
                NestedToolStatus::Error
            } else {
                NestedToolStatus::Ok
            };
            record.duration_ms = Some(elapsed_ms);
            if is_error && !error_text.is_empty() {
                let truncated: String = error_text.chars().take(MAX_NESTED_ERROR_CHARS).collect();
                record.error = Some(truncated);
            }
        }
    }

    pub fn add_usage(&mut self, usage: &Usage) {
        self.usage = Some(match self.usage {
            Some(existing) => combine_usage(&existing, usage),
            None => *usage,
        });
    }

    pub fn total_usage(&self) -> Option<Usage> {
        self.usage
    }

    pub fn snapshot(&self) -> Option<NestedToolCalls> {
        if self.calls.is_empty() && self.complete {
            return None;
        }
        let all_finished = self
            .calls
            .iter()
            .all(|call| call.status != NestedToolStatus::Unfinished);
        Some(NestedToolCalls {
            calls: self.calls.clone(),
            complete: self.complete && all_finished,
        })
    }
}

/// Session events emitted for nested tool calls.
#[derive(Clone, Debug)]
pub enum NestedToolExecutionEvent {
    Start {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        parent_tool_call_id: String,
    },
    Update {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
        parent_tool_call_id: String,
    },
    End {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        duration_ms: Option<u64>,
        parent_tool_call_id: String,
    },
}

struct CallScope {
    recorder: Arc<Mutex<NestedCallRecorder>>,
    next_id: usize,
    holds_queue: bool,
}

pub type GetToolsFn = Arc<dyn Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync>;
pub type IsSequentialFn = Arc<dyn Fn() -> bool + Send + Sync>;
pub type NestedEventFn = Arc<dyn Fn(NestedToolExecutionEvent) + Send + Sync>;

pub struct NestedToolCallRunner {
    scopes: Mutex<IndexMap<String, CallScope>>,
    sequential_lock: tokio::sync::Mutex<()>,
    get_tools: GetToolsFn,
    is_sequential: IsSequentialFn,
    hooks: ToolCallHooks,
    on_event: Mutex<NestedEventFn>,
}

impl NestedToolCallRunner {
    pub fn new(
        get_tools: GetToolsFn,
        is_sequential: IsSequentialFn,
        hooks: ToolCallHooks,
        on_event: NestedEventFn,
    ) -> Self {
        Self {
            scopes: Mutex::new(IndexMap::new()),
            sequential_lock: tokio::sync::Mutex::new(()),
            get_tools,
            is_sequential,
            hooks,
            on_event: Mutex::new(on_event),
        }
    }

    pub fn set_event_sink(&self, on_event: NestedEventFn) {
        *self
            .on_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = on_event;
    }

    fn emit(&self, event: NestedToolExecutionEvent) {
        let sink = self
            .on_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        sink(event);
    }

    /// Execute `name` as a nested call under `caller_id`, generating `<caller_id>/<n>`.
    pub async fn execute(
        &self,
        caller_id: &str,
        name: &str,
        args: Value,
        signal: Option<&CancellationToken>,
        deliver_as_update: bool,
        on_update: Option<&(dyn Fn(AgentToolResult) + Send + Sync)>,
    ) -> ToolOutcome {
        let (child_id, recorder, holds_queue) = {
            let mut scopes = self
                .scopes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let scope = scopes
                .entry(caller_id.to_owned())
                .or_insert_with(|| CallScope {
                    recorder: Arc::new(Mutex::new(NestedCallRecorder::new())),
                    next_id: 1,
                    holds_queue: false,
                });
            let id = format!("{caller_id}/{}", scope.next_id);
            scope.next_id += 1;
            (id, Arc::clone(&scope.recorder), scope.holds_queue)
        };

        let args_map: JsonObject = match args {
            Value::Object(map) => map,
            _ => JsonObject::new(),
        };
        let tool_call = AgentToolCall {
            id: child_id.clone(),
            name: name.to_owned(),
            arguments: args_map.clone(),
            ..AgentToolCall::default()
        };
        let args_value = Value::Object(args_map);

        let record_index = recorder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .start(&tool_call);

        self.emit(NestedToolExecutionEvent::Start {
            tool_call_id: child_id.clone(),
            tool_name: name.to_owned(),
            args: args_value.clone(),
            parent_tool_call_id: caller_id.to_owned(),
        });

        let tools = (self.get_tools)();
        let exclusive = !holds_queue
            && ((self.is_sequential)()
                || tools
                    .iter()
                    .find(|tool| tool.declaration().name == name)
                    .is_some_and(|tool| {
                        tool.execution_mode() == Some(ToolExecutionMode::Sequential)
                    }));

        let seq_guard = if exclusive {
            Some(self.sequential_lock.lock().await)
        } else {
            None
        };

        {
            let mut scopes = self
                .scopes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scopes.insert(
                child_id.clone(),
                CallScope {
                    recorder: Arc::clone(&recorder),
                    next_id: 1,
                    holds_queue: holds_queue || exclusive,
                },
            );
        }

        let default_signal = CancellationToken::new();
        let effective_signal = signal.unwrap_or(&default_signal);
        let synthetic_assistant = faux_assistant_message(Vec::new());
        let agent_context = AgentContext {
            messages: Vec::new(),
            tools: tools.clone(),
        };

        let update_sink = |partial: AgentToolResult| {
            let child_id = child_id.clone();
            let name = name.to_owned();
            let args_value = args_value.clone();
            let caller_id = caller_id.to_owned();
            let fut: futures_util::future::BoxFuture<'_, pi_rs_agent::Result<()>> =
                Box::pin(async move {
                    if let Some(cb) = on_update {
                        cb(partial.clone());
                    }
                    if deliver_as_update {
                        self.emit(NestedToolExecutionEvent::Update {
                            tool_call_id: child_id,
                            tool_name: name,
                            args: args_value,
                            partial_result: partial,
                            parent_tool_call_id: caller_id,
                        });
                    }
                    Ok(())
                });
            fut
        };
        let update_sink_ref: &ToolUpdateSink<'_> = &update_sink;

        let outcome: AgentToolCallOutcome = run_tool_call(
            &tool_call,
            RunToolCallOptions {
                tools: &tools,
                assistant_message: &synthetic_assistant,
                context: &agent_context,
                hooks: &self.hooks,
                signal: effective_signal,
                on_update: Some(update_sink_ref),
            },
        )
        .await;

        {
            let mut scopes = self
                .scopes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scopes.shift_remove(&child_id);
        }
        drop(seq_guard);

        let err_text = text_of(&outcome.result);
        {
            let mut rec = recorder
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            rec.finish(record_index, outcome.is_error, &err_text);
            if let Some(usage) = &outcome.result.usage {
                rec.add_usage(usage);
            }
        }

        let result_value = &*outcome.result;
        self.emit(NestedToolExecutionEvent::End {
            tool_call_id: child_id.clone(),
            tool_name: name.to_owned(),
            result: result_value.clone(),
            is_error: outcome.is_error,
            duration_ms: outcome.duration_ms,
            parent_tool_call_id: caller_id.to_owned(),
        });

        ToolOutcome {
            tool_call: json!({
                "type": "toolCall",
                "id": child_id,
                "name": name,
                "arguments": tool_call.arguments,
            }),
            result: serde_json::to_value(result_value).unwrap_or_else(|_| json!({ "content": [] })),
            is_error: outcome.is_error,
            nested_calls: Vec::new(),
        }
    }

    pub fn take_record(&self, tool_call_id: &str) -> Option<NestedCallSummary> {
        let scope = self
            .scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .shift_remove(tool_call_id)?;
        let rec = scope
            .recorder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(NestedCallSummary {
            calls: rec.snapshot(),
            usage: rec.total_usage(),
        })
    }

    pub fn clear(&self) {
        self.scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            InputContent::Text(text) => Some(text.text.as_str()),
            InputContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_agent::AgentToolResult;
    use pi_rs_ai::{Cost, Tool};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct EchoTool(Tool);

    impl AgentTool for EchoTool {
        fn declaration(&self) -> &Tool {
            &self.0
        }
        fn label(&self) -> &str {
            "echo"
        }
        fn execute<'a>(
            &'a self,
            tool_call_id: &'a str,
            params: &'a Value,
            _signal: &'a CancellationToken,
            _on_update: pi_rs_agent::AgentToolUpdateCallback,
        ) -> futures_util::future::BoxFuture<'a, pi_rs_agent::Result<AgentToolResult>> {
            let msg = params["msg"].as_str().unwrap_or("ok").to_owned();
            let id = tool_call_id.to_owned();
            Box::pin(async move {
                let mut res = AgentToolResult::text(format!("{id}:{msg}"));
                res.usage = Some(Usage {
                    input: 10.0,
                    output: 5.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    cache_write_1h: None,
                    reasoning: None,
                    total_tokens: 15.0,
                    cost: Cost {
                        total: 0.25,
                        ..Cost::default()
                    },
                });
                Ok(res)
            })
        }
    }

    #[tokio::test]
    async fn nested_runner_assigns_hierarchical_ids_emits_parent_and_records_usage() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded_events = Arc::clone(&events);
        let tool: Arc<dyn AgentTool> = Arc::new(EchoTool(Tool {
            name: "echo".into(),
            description: "echo".into(),
            parameters: json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } }
            }),
            constrained_sampling: None,
        }));
        let blocked = Arc::new(AtomicBool::new(false));
        let blocked_flag = Arc::clone(&blocked);
        let hooks = ToolCallHooks {
            before_tool_call: Some(Arc::new(move |ctx, _| {
                let is_blocked =
                    blocked_flag.load(Ordering::SeqCst) && ctx.args["msg"].as_str() == Some("deny");
                Box::pin(async move {
                    if is_blocked {
                        Ok(Some(pi_rs_agent::BeforeToolCallResult {
                            block: true,
                            reason: Some("blocked nested".into()),
                            terminate: false,
                        }))
                    } else {
                        Ok(None)
                    }
                })
            })),
            after_tool_call: None,
        };
        let runner = NestedToolCallRunner::new(
            Arc::new(move || vec![Arc::clone(&tool)]),
            Arc::new(|| true),
            hooks,
            Arc::new(move |ev| {
                recorded_events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ev);
            }),
        );

        let first = runner
            .execute("call-7", "echo", json!({"msg": "a"}), None, false, None)
            .await;
        assert!(!first.is_error);
        assert_eq!(first.tool_call["id"], "call-7/1");

        blocked.store(true, Ordering::SeqCst);
        let second = runner
            .execute("call-7", "echo", json!({"msg": "deny"}), None, false, None)
            .await;
        assert!(second.is_error);
        assert_eq!(second.tool_call["id"], "call-7/2");

        let summary = runner.take_record("call-7").expect("nested summary");
        let calls = summary.calls.expect("calls recorded");
        assert!(calls.complete);
        assert_eq!(calls.calls.len(), 2);
        assert_eq!(calls.calls[0].id, "call-7/1");
        assert_eq!(calls.calls[0].status, NestedToolStatus::Ok);
        assert_eq!(calls.calls[1].id, "call-7/2");
        assert_eq!(calls.calls[1].status, NestedToolStatus::Error);
        assert!(
            calls.calls[1]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("blocked nested"))
        );
        let usage = summary.usage.expect("usage summed");
        assert_eq!(usage.input, 10.0);
        assert_eq!(usage.cost.total, 0.25);

        let captured = events.lock().unwrap_or_else(|e| e.into_inner());
        assert!(captured.iter().any(|ev| matches!(
            ev,
            NestedToolExecutionEvent::Start { tool_call_id, parent_tool_call_id, .. }
            if tool_call_id == "call-7/1" && parent_tool_call_id == "call-7"
        )));
    }
}
