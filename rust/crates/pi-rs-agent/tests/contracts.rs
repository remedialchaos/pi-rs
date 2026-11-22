#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::{StreamExt, future::BoxFuture};
use pi_rs_agent::*;
use pi_rs_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEventStream, InputContent, JsonObject,
    Message, Model, StopReason, StreamOptions, StringOrBlocks, Tool, ToolCall, TranscriptContext,
    faux::{FauxOptions, faux_provider, faux_text},
};
use serde_json::{Value, json};

#[path = "cases/controller.rs"]
mod controller;
#[path = "cases/loop_contract.rs"]
mod loop_contract;
#[path = "cases/proxy_contract.rs"]
mod proxy_contract;
#[path = "cases/tools.rs"]
mod tools;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn model() -> Model {
    let faux = faux_provider(FauxOptions {
        api: Some("faux-test".into()),
        ..FauxOptions::default()
    });
    // INFALLIBLE: faux_provider always supplies a default model for an empty model list.
    faux.models[0].clone()
}

fn reply(content: Vec<AssistantContent>, reason: StopReason) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&model());
    message.content = content;
    message.stop_reason = reason;
    message
}

fn answer(text: &str) -> AssistantMessage {
    reply(vec![faux_text(text)], StopReason::Stop)
}

fn call<const N: usize>(id: &str, name: &str, args: [(&str, Value); N]) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        ..ToolCall::default()
    }
}

fn tool_reply(calls: Vec<ToolCall>) -> AssistantMessage {
    reply(
        calls.into_iter().map(AssistantContent::ToolCall).collect(),
        StopReason::ToolUse,
    )
}

#[derive(Clone, Default)]
struct Requests(Arc<Mutex<Vec<(Model, TranscriptContext, StreamOptions)>>>);

impl Requests {
    fn users(&self) -> Vec<Vec<String>> {
        locked(&self.0)
            .iter()
            .map(|(_, ctx, _)| {
                ctx.messages()
                    .iter()
                    .filter_map(|message| {
                        let Message::User(message) = message else {
                            return None;
                        };
                        Some(match &message.content {
                            StringOrBlocks::String(text) => text.clone(),
                            StringOrBlocks::Blocks(blocks) => {
                                pi_rs_ai::transcript::input_text(blocks, " ")
                            }
                        })
                    })
                    .collect()
            })
            .collect()
    }
}

fn scripted(steps: impl IntoIterator<Item = AssistantMessage>) -> (StreamFn, Requests) {
    let steps = Mutex::new(steps.into_iter().collect::<VecDeque<_>>());
    let requests = Requests::default();
    let log = requests.clone();
    let stream = Arc::new(move |model, context, options| {
        locked(&log.0).push((model, context, options));
        let step = locked(&steps).pop_front();
        Box::pin(async move {
            let step =
                step.ok_or_else(|| AgentError::Callback("unexpected provider request".into()))?;
            let (send, stream) = AssistantMessageEventStream::channel();
            send.finish(step);
            Ok(stream)
        }) as _
    });
    (stream, requests)
}

async fn collect(stream: AgentEventStream) -> TestResult<(Vec<AgentEvent>, Arc<[AgentMessage]>)> {
    let mut stream = stream;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    Ok((events, stream.result().await?))
}

fn event_type(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

fn input_text(content: &[InputContent]) -> String {
    pi_rs_ai::transcript::input_text(content, "\n")
}

fn tool_texts(messages: &[AgentMessage]) -> Vec<(&str, String, bool)> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(tool) => Some((
                tool.tool_call_id.as_str(),
                input_text(&tool.content),
                tool.is_error,
            )),
            _ => None,
        })
        .collect()
}

type Execute = dyn for<'a> Fn(
        &'a str,
        &'a Value,
        &'a CancellationToken,
        AgentToolUpdateCallback,
    ) -> BoxFuture<'a, pi_rs_agent::Result<AgentToolResult>>
    + Send
    + Sync;
type Prepare = dyn Fn(&mut JsonObject) -> pi_rs_agent::Result<()> + Send + Sync;

struct MockTool {
    declaration: Tool,
    execute: Box<Execute>,
    mode: Option<ToolExecutionMode>,
    prepare: Option<Box<Prepare>>,
}

impl MockTool {
    fn new(
        name: &str,
        schema: Value,
        execute: impl for<'a> Fn(
            &'a str,
            &'a Value,
            &'a CancellationToken,
            AgentToolUpdateCallback,
        ) -> BoxFuture<'a, pi_rs_agent::Result<AgentToolResult>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            declaration: Tool {
                name: name.into(),
                description: format!("Test {name}"),
                parameters: schema,
                constrained_sampling: None,
            },
            execute: Box::new(execute),
            mode: None,
            prepare: None,
        }
    }

    fn echo(name: &str, terminate: bool) -> Self {
        Self::new(
            name,
            json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
            move |_, args, _, _| {
                Box::pin(async move {
                    let value = args
                        .get("value")
                        .and_then(Value::as_str)
                        .ok_or_else(|| AgentError::Callback("echo did not receive text".into()))?;
                    Ok(AgentToolResult {
                        terminate: Some(terminate),
                        ..AgentToolResult::text(format!("echo:{value}"))
                    })
                })
            },
        )
    }
}

impl AgentTool for MockTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }
    fn label(&self) -> &str {
        &self.declaration.name
    }
    fn execute<'a>(
        &'a self,
        id: &'a str,
        args: &'a Value,
        signal: &'a CancellationToken,
        update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, pi_rs_agent::Result<AgentToolResult>> {
        (self.execute)(id, args, signal, update)
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.mode
    }
    fn prepare_arguments(&self, args: &mut JsonObject) -> pi_rs_agent::Result<()> {
        match &self.prepare {
            Some(prepare) => prepare(args),
            None => Ok(()),
        }
    }
}
