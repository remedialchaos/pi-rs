use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context as TaskContext, Poll};

use futures_util::{Stream, StreamExt};
use indexmap::IndexMap;
use pi_rs_ai::{
    AssistantContent, AssistantMessageEvent, Attribution, ClientIdentity, Context, FinalMessage,
    Model, StringOrBlocks, SystemMessage, Tool, ToolReference, ToolResultMessage,
    normalize_context, transcript::declarations_equal,
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};

use crate::{
    AgentContext, AgentError, AgentEvent, AgentEventSink, AgentLoopConfig, AgentMessage,
    AgentTurnContext, AgentTurnDecision, CancellationToken, GetQueuedMessages,
    PrepareRequestContext, Result, StreamFn, ThinkingLevel, default_stream_fn,
    tool::execute_tool_calls,
    types::{system_messages, unix_millis},
};

pub static DEFAULT_AGENT_IDENTITY: LazyLock<ClientIdentity> = LazyLock::new(|| ClientIdentity {
    attribution: Some(Some(Attribution {
        name: "pi".into(),
        url: "https://pi.dev".into(),
        categories: Some(vec!["cli-agent".into()]),
    })),
    ..ClientIdentity::default()
});

type Messages = Arc<[AgentMessage]>;

/// Single-consumer events; the final result can be awaited without draining them.
pub struct AgentEventStream {
    events: mpsc::UnboundedReceiver<AgentEvent>,
    result: watch::Receiver<Option<Result<Messages>>>,
}

impl AgentEventStream {
    pub async fn result(&self) -> Result<Messages> {
        self.result
            .clone()
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|result| result.clone())
            .ok_or(AgentError::MissingAgentResult)?
    }
}

impl Stream for AgentEventStream {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<AgentEvent>> {
        self.events.poll_recv(cx)
    }
}

pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<AgentEventStream> {
    spawn_loop(Some(prompts), context, config, signal, stream_fn)
}

pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<AgentEventStream> {
    check_continuation(&context)?;
    spawn_loop(None, context, config, signal, stream_fn)
}

fn spawn_loop(
    prompts: Option<Vec<AgentMessage>>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<AgentEventStream> {
    let runtime = Handle::try_current().map_err(|_| AgentError::MissingRuntime)?;
    let (tx, events) = mpsc::unbounded_channel();
    let (complete, result) = watch::channel(None);
    runtime.spawn(async move {
        let emit = |event| {
            let _ = tx.send(event);
            Box::pin(std::future::ready(Ok(()))) as _
        };
        let messages = run(prompts, context, config, &emit, signal, stream_fn).await;
        complete.send_replace(Some(messages));
    });
    Ok(AgentEventStream { events, result })
}

pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: &AgentEventSink<'_>,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Messages> {
    run(Some(prompts), context, config, emit, signal, stream_fn).await
}

pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: &AgentEventSink<'_>,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Messages> {
    check_continuation(&context)?;
    run(None, context, config, emit, signal, stream_fn).await
}

fn check_continuation(context: &AgentContext) -> Result<()> {
    match context.messages.last() {
        None => Err(AgentError::EmptyContext),
        Some(AgentMessage::Assistant(_)) => Err(AgentError::AssistantTail),
        Some(_) => Ok(()),
    }
}

async fn run(
    prompts: Option<Vec<AgentMessage>>,
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    emit: &AgentEventSink<'_>,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Messages> {
    let signal = signal.unwrap_or_else(|| config.stream_options.request.signal.clone());
    let mut messages =
        prompts.map_or_else(Vec::new, |prompts| declare_tool_changes(&context, prompts));
    context.messages.extend(messages.iter().cloned());
    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    for message in &messages {
        emit_message(message, emit).await?;
    }
    let stream_fn = stream_fn.map_or_else(default_stream_fn, Ok)?;
    let mut pending = poll_queue(&config.get_steering_messages).await;
    let mut previous: Option<(FinalMessage, Vec<Arc<ToolResultMessage>>)> = None;

    loop {
        let mut prepared = Vec::new();
        if let Some((assistant, tool_results)) = previous.take() {
            if let Some(prepare) = &config.hooks.prepare_next_turn
                && let Some(update) = prepare(
                    AgentTurnContext {
                        message: &assistant,
                        tool_results: &tool_results,
                        context: &context,
                        new_messages: &messages,
                    },
                    &signal,
                )
                .await?
            {
                prepared = update.messages;
                apply_update(
                    &mut context,
                    &mut config,
                    update.context,
                    update.model,
                    update.thinking_level,
                );
            }
            // A long preparation can receive more input. Never drain a second time if a
            // message was already selected for this turn (including a selected follow-up).
            if pending.is_empty() {
                pending = poll_queue(&config.get_steering_messages).await;
            }
            emit(AgentEvent::TurnStart).await?;
        }
        if prepared.is_empty() {
            prepared = std::mem::take(&mut pending);
        } else {
            prepared.append(&mut pending);
        }
        for message in declare_tool_changes(&context, prepared) {
            emit_message(&message, emit).await?;
            context.messages.push(message.clone());
            messages.push(message);
        }
        if let Some(prepare) = &config.hooks.prepare_request
            && let Some(update) = prepare(
                PrepareRequestContext {
                    context: &context,
                    model: &config.model,
                    thinking_level: thinking_level(&config),
                },
                &signal,
            )
            .await?
        {
            apply_update(
                &mut context,
                &mut config,
                update.context,
                update.model,
                update.thinking_level,
            );
        }

        let assistant = stream_assistant(&context, &config, &signal, emit, &stream_fn).await?;
        let failed = assistant.stop_reason.failed();
        let message = AgentMessage::Assistant(assistant.clone());
        context.messages.push(message.clone());
        messages.push(message.clone());
        let mut tool_results = Vec::new();
        let mut requires_provider_call = false;
        if !failed {
            let calls: Vec<_> = assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect();
            if !calls.is_empty() {
                let batch =
                    execute_tool_calls(&context, &assistant, &calls, &config, &signal, emit)
                        .await?;
                requires_provider_call = !batch.terminate;
                tool_results = batch.messages;
                for result in &tool_results {
                    let message = AgentMessage::ToolResult(result.clone());
                    context.messages.push(message.clone());
                    messages.push(message);
                }
            }
        }
        let decision = match &config.hooks.finish_turn {
            Some(finish) => {
                finish(
                    AgentTurnContext {
                        message: &assistant,
                        tool_results: &tool_results,
                        context: &context,
                        new_messages: &messages,
                    },
                    &signal,
                )
                .await?
            }
            None => None,
        };
        emit(AgentEvent::TurnEnd {
            message,
            tool_results: tool_results.clone(),
        })
        .await?;
        if failed || decision == Some(AgentTurnDecision::End) {
            break;
        }

        pending = poll_queue(&config.get_steering_messages).await;
        if !requires_provider_call && pending.is_empty() {
            pending = poll_queue(&config.get_follow_up_messages).await;
            if pending.is_empty() && decision != Some(AgentTurnDecision::Continue) {
                break;
            }
        }
        previous = Some((assistant, tool_results));
    }
    let messages: Messages = messages.into();
    emit(AgentEvent::AgentEnd {
        messages: messages.clone(),
    })
    .await?;
    Ok(messages)
}

fn apply_update(
    context: &mut AgentContext,
    config: &mut AgentLoopConfig,
    next_context: Option<AgentContext>,
    model: Option<Model>,
    level: Option<ThinkingLevel>,
) {
    if let Some(next_context) = next_context {
        *context = next_context;
    }
    if let Some(model) = model {
        config.model = model;
    }
    if let Some(level) = level {
        config.stream_options.reasoning = (level != ThinkingLevel::Off).then_some(level);
    }
}

fn thinking_level(config: &AgentLoopConfig) -> ThinkingLevel {
    config
        .stream_options
        .reasoning
        .unwrap_or(ThinkingLevel::Off)
}

async fn poll_queue(get: &Option<GetQueuedMessages>) -> Vec<AgentMessage> {
    match get {
        Some(get) => get().await,
        None => Vec::new(),
    }
}

async fn emit_message(message: &AgentMessage, emit: &AgentEventSink<'_>) -> Result<()> {
    emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await?;
    emit(AgentEvent::MessageEnd {
        message: message.clone(),
    })
    .await
}

fn declare_tool_changes(
    context: &AgentContext,
    mut pending: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let system_index = pending
        .iter()
        .rposition(|message| matches!(message, AgentMessage::System(_)));
    let mut previous = IndexMap::new();
    for system in system_messages(&context.messages).chain(system_messages(
        system_index.map_or(pending.as_slice(), |i| &pending[..i]),
    )) {
        for old in system.tools_removed.iter().flatten() {
            previous.shift_remove(old.name.as_str());
        }
        for tool in system.tools_added.iter().flatten() {
            previous.insert(tool.name.as_str(), tool);
        }
    }
    let current: IndexMap<&str, &Tool> = context
        .tools
        .iter()
        .map(|tool| (tool.declaration().name.as_str(), tool.declaration()))
        .collect();
    let added: Vec<_> = context
        .tools
        .iter()
        .map(|tool| tool.declaration())
        .filter(|tool| {
            previous
                .get(tool.name.as_str())
                .is_none_or(|old| !declarations_equal(old, tool))
        })
        .cloned()
        .collect();
    let removed: Vec<_> = previous
        .into_iter()
        .filter(|(name, old)| {
            current
                .get(name)
                .is_none_or(|tool| !declarations_equal(old, tool))
        })
        .map(|(name, _)| ToolReference { name: name.into() })
        .collect();
    if let Some(i) = system_index {
        if let AgentMessage::System(system) = &mut pending[i]
            && (!added.is_empty()
                || !removed.is_empty()
                || system.tools_added.as_ref().is_some_and(|v| !v.is_empty())
                || system.tools_removed.as_ref().is_some_and(|v| !v.is_empty()))
        {
            let system = Arc::make_mut(system);
            system.tools_added = (!added.is_empty()).then_some(added);
            system.tools_removed = (!removed.is_empty()).then_some(removed);
        }
    } else if !added.is_empty() || !removed.is_empty() {
        let index = pending
            .iter()
            .position(|message| !matches!(message, AgentMessage::System(_)))
            .unwrap_or(pending.len());
        pending.insert(
            index,
            SystemMessage {
                content: StringOrBlocks::String(String::new()),
                tools_added: (!added.is_empty()).then_some(added),
                tools_removed: (!removed.is_empty()).then_some(removed),
                timestamp: unix_millis(),
                ..SystemMessage::default()
            }
            .into(),
        );
    }
    pending
}

async fn stream_assistant(
    context: &AgentContext,
    config: &AgentLoopConfig,
    signal: &CancellationToken,
    emit: &AgentEventSink<'_>,
    stream_fn: &StreamFn,
) -> Result<pi_rs_ai::FinalMessage> {
    let transformed;
    let messages = if let Some(transform) = &config.hooks.transform_context {
        transformed = transform(&context.messages, signal).await;
        transformed.as_slice()
    } else {
        &context.messages
    };
    let messages = match &config.hooks.convert_to_llm {
        Some(convert) => convert(messages).await,
        None => messages.iter().filter_map(AgentMessage::to_llm).collect(),
    };
    let transcript = normalize_context(Context {
        messages,
        ..Context::default()
    });
    let mut options = config.stream_options.clone();
    if let Some(get) = &config.hooks.get_api_key
        && let Some(key) = get(&config.model.provider)
            .await
            .filter(|key| !key.is_empty())
    {
        options.request.api_key = Some(key);
    }
    if options.request.identity.attribution.is_none() {
        options.request.identity.attribution = DEFAULT_AGENT_IDENTITY.attribution.clone();
    }
    options.request.signal = signal.clone();
    let mut response = stream_fn(config.model.clone(), transcript, options).await?;
    let mut started = false;
    let mut terminal = None;
    while let Some(event) = response.next().await {
        use AssistantMessageEvent as E;
        match &event {
            E::Start { partial } => {
                let snapshot = partial.read().unwrap_or_else(|e| e.into_inner()).clone();
                emit(AgentEvent::MessageStart {
                    message: snapshot.into(),
                })
                .await?;
                started = true;
            }
            E::Done { message, .. } | E::Error { error: message, .. } => {
                terminal = Some(message.clone());
                break;
            }
            E::TextStart { partial, .. }
            | E::TextDelta { partial, .. }
            | E::TextEnd { partial, .. }
            | E::ThinkingStart { partial, .. }
            | E::ThinkingDelta { partial, .. }
            | E::ThinkingEnd { partial, .. }
            | E::ToolCallStart { partial, .. }
            | E::ToolCallDelta { partial, .. }
            | E::ToolCallEnd { partial, .. } => {
                if started {
                    emit(AgentEvent::MessageUpdate {
                        message: partial.clone(),
                        assistant_message_event: event,
                    })
                    .await?;
                }
            }
        }
    }
    let mut final_message = match terminal {
        Some(message) => message,
        None => response.result().await?,
    };
    let level = Some(thinking_level(config));
    if final_message.thinking_level != level {
        Arc::make_mut(&mut final_message).thinking_level = level;
    }
    let message = AgentMessage::Assistant(final_message.clone());
    if !started {
        emit(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await?;
    }
    emit(AgentEvent::MessageEnd { message }).await?;
    Ok(final_message)
}
