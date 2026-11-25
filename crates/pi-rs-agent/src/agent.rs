use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use indexmap::IndexMap;
use pi_rs_ai::{
    Api, AssistantContent, AssistantMessage, BaseModel, ImageContent, InputContent, Message, Model,
    StopReason, StreamOptions, StringOrBlocks, SystemMessage, Transport, UserMessage, UserRole,
    transcript::{get_current_system_message, system_message_text},
};

use crate::{
    AgentContext, AgentError, AgentEvent, AgentHooks, AgentListener, AgentLoopConfig, AgentMessage,
    AgentState, AgentTool, CancellationToken, PrepareNextTurnWithSignal, ProxyOptions, QueueMode,
    Result, StreamFn, StreamingMessage, ThinkingLevel, ToolExecutionMode, default_stream_fn,
    proxy_stream_fn, run_agent_loop, run_agent_loop_continue,
    types::{system_messages, unix_millis},
};

#[derive(Default)]
pub struct AgentInitialState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
}

#[derive(Default)]
pub struct AgentOptions {
    pub initial_state: AgentInitialState,
    pub stream_fn: Option<StreamFn>,
    pub stream_options: StreamOptions,
    pub hooks: AgentHooks,
    /// Signal-only form used when there is no context-aware hook in `hooks`.
    pub prepare_next_turn: Option<PrepareNextTurnWithSignal>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: ToolExecutionMode,
    pub proxy: Option<ProxyOptions>,
}

pub enum Prompt {
    Text(String),
    Message(AgentMessage),
    Messages(Vec<AgentMessage>),
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}
impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}
impl From<AgentMessage> for Prompt {
    fn from(message: AgentMessage) -> Self {
        Self::Message(message)
    }
}
impl From<Vec<AgentMessage>> for Prompt {
    fn from(messages: Vec<AgentMessage>) -> Self {
        Self::Messages(messages)
    }
}

/// Clones share one conversation. State and queue operations do not await listeners or hold a lock
/// across user code. Subscribe with an explicit unsubscribe handle; dropping it does not unsubscribe.
#[derive(Clone)]
pub struct Agent(Arc<Mutex<Inner>>);

struct Inner {
    state: AgentState,
    stream_fn: Option<StreamFn>,
    stream_options: StreamOptions,
    hooks: AgentHooks,
    tool_execution: ToolExecutionMode,
    proxy: Option<ProxyOptions>,
    steering: PendingQueue,
    follow_up: PendingQueue,
    listeners: BTreeMap<u64, Arc<AgentListener>>,
    next_listener: u64,
    active: Option<ActiveRun>,
}

struct PendingQueue {
    mode: QueueMode,
    messages: VecDeque<AgentMessage>,
}

impl PendingQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            messages: VecDeque::new(),
        }
    }
    fn size(&self) -> usize {
        match self.mode {
            QueueMode::OneAtATime => self.messages.len().min(1),
            QueueMode::All => self.messages.len(),
        }
    }
    fn peek(&self) -> Vec<AgentMessage> {
        self.messages.iter().take(self.size()).cloned().collect()
    }
    fn drain(&mut self) -> Vec<AgentMessage> {
        self.messages.drain(..self.size()).collect()
    }
}

struct ActiveRun {
    signal: CancellationToken,
    settled: CancellationToken,
}

struct RunGuard<'a> {
    agent: &'a Agent,
    finished: bool,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.agent.lock();
        let active = inner.active.take();
        inner.state.is_streaming = false;
        inner.state.streaming_message = None;
        inner.state.pending_tool_calls.clear();
        drop(inner);
        if let Some(active) = active {
            if !self.finished {
                active.signal.cancel();
            }
            active.settled.cancel();
        }
    }
}

pub struct Subscription {
    agent: Weak<Mutex<Inner>>,
    id: u64,
}

impl Subscription {
    pub fn unsubscribe(&self) -> bool {
        self.agent.upgrade().is_some_and(|agent| {
            agent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .listeners
                .remove(&self.id)
                .is_some()
        })
    }
}

impl AgentState {
    pub fn system_prompt(&self) -> String {
        replay_system(&self.messages)
            .as_ref()
            .map_or_else(String::new, system_message_text)
    }
}

impl Agent {
    pub fn new(mut options: AgentOptions) -> Result<Self> {
        let stream_fn = match options.stream_fn {
            Some(stream_fn) => Some(stream_fn),
            None if options.proxy.is_some() => default_stream_fn().ok(),
            None => Some(default_stream_fn()?),
        };
        let AgentInitialState {
            system_prompt,
            model,
            thinking_level,
            tools,
            mut messages,
        } = options.initial_state;
        if !matches!(messages.first(), Some(AgentMessage::System(_)))
            && (system_prompt.as_ref().is_some_and(|text| !text.is_empty()) || !tools.is_empty())
        {
            messages.insert(
                0,
                SystemMessage {
                    content: StringOrBlocks::String(system_prompt.unwrap_or_default()),
                    tools_added: (!tools.is_empty()).then(|| {
                        tools
                            .iter()
                            .map(|tool| tool.declaration().clone())
                            .collect()
                    }),
                    ..SystemMessage::default()
                }
                .into(),
            );
        }
        if options.hooks.prepare_next_turn.is_none()
            && let Some(legacy) = options.prepare_next_turn
        {
            options.hooks.prepare_next_turn = Some(Arc::new(move |_, signal| legacy(signal)));
        }
        options
            .stream_options
            .transport
            .get_or_insert(Transport::Auto);
        Ok(Self(Arc::new(Mutex::new(Inner {
            state: AgentState {
                model: model.unwrap_or_else(unknown_model),
                thinking_level: thinking_level.unwrap_or(ThinkingLevel::Off),
                tools,
                messages,
                is_streaming: false,
                streaming_message: None,
                pending_tool_calls: Default::default(),
                error_message: None,
            },
            stream_fn,
            stream_options: options.stream_options,
            hooks: options.hooks,
            tool_execution: options.tool_execution,
            proxy: options.proxy,
            steering: PendingQueue::new(options.steering_mode),
            follow_up: PendingQueue::new(options.follow_up_mode),
            listeners: BTreeMap::new(),
            next_listener: 0,
            active: None,
        }))))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(&self, listener: Arc<AgentListener>) -> Subscription {
        let mut inner = self.lock();
        let id = if let Some((&id, _)) = inner
            .listeners
            .iter()
            .find(|(_, existing)| Arc::ptr_eq(existing, &listener))
        {
            id
        } else {
            let id = inner.next_listener;
            inner.next_listener += 1;
            inner.listeners.insert(id, listener);
            id
        };
        Subscription {
            agent: Arc::downgrade(&self.0),
            id,
        }
    }

    pub fn state(&self) -> AgentState {
        self.lock().state.clone()
    }
    pub fn system_prompt(&self) -> String {
        self.lock().state.system_prompt()
    }

    /// Adds a transcript system update; upstream system prompt text is replayed, not overwritten.
    pub fn append_system_prompt(&self, text: impl Into<String>) {
        self.append_message(
            SystemMessage {
                content: StringOrBlocks::String(text.into()),
                timestamp: unix_millis(),
                ..SystemMessage::default()
            }
            .into(),
        );
    }

    pub fn model(&self) -> Model {
        self.lock().state.model.clone()
    }
    pub fn set_model(&self, model: Model) {
        self.lock().state.model = model;
    }
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.lock().state.thinking_level
    }
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.lock().state.thinking_level = level;
    }
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.lock().state.tools.clone()
    }
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.lock().state.tools = tools;
    }
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.lock().state.messages.clone()
    }
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.lock().state.messages = messages;
    }
    pub fn append_message(&self, message: AgentMessage) {
        self.lock().state.messages.push(message);
    }
    pub fn clear_messages(&self) {
        self.lock().state.messages.clear();
    }
    pub fn steering_mode(&self) -> QueueMode {
        self.lock().steering.mode
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.lock().steering.mode = mode;
    }
    pub fn follow_up_mode(&self) -> QueueMode {
        self.lock().follow_up.mode
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.lock().follow_up.mode = mode;
    }
    pub fn tool_execution(&self) -> ToolExecutionMode {
        self.lock().tool_execution
    }
    pub fn set_tool_execution(&self, mode: ToolExecutionMode) {
        self.lock().tool_execution = mode;
    }
    pub fn session_id(&self) -> Option<String> {
        self.lock().stream_options.session_id.clone()
    }
    pub fn set_session_id(&self, session_id: Option<String>) {
        self.lock().stream_options.session_id = session_id;
    }
    pub fn max_retry_delay_ms(&self) -> Option<u64> {
        self.lock().stream_options.request.max_retry_delay_ms
    }
    pub fn set_max_retry_delay_ms(&self, delay: Option<u64>) {
        self.lock().stream_options.request.max_retry_delay_ms = delay;
    }
    pub fn transport(&self) -> Transport {
        self.lock()
            .stream_options
            .transport
            .unwrap_or(Transport::Auto)
    }
    pub fn set_transport(&self, transport: Transport) {
        self.lock().stream_options.transport = Some(transport);
    }
    pub fn thinking_budgets(&self) -> IndexMap<ThinkingLevel, u64> {
        self.lock().stream_options.thinking_budgets.clone()
    }
    pub fn set_thinking_budgets(&self, budgets: IndexMap<ThinkingLevel, u64>) {
        self.lock().stream_options.thinking_budgets = budgets;
    }
    pub fn proxy_options(&self) -> Option<ProxyOptions> {
        self.lock().proxy.clone()
    }
    pub fn set_proxy_options(&self, proxy: Option<ProxyOptions>) {
        self.lock().proxy = proxy;
    }
    pub fn stream_options(&self) -> StreamOptions {
        self.lock().stream_options.clone()
    }
    pub fn set_stream_options(&self, options: StreamOptions) {
        self.lock().stream_options = options;
    }
    pub fn set_stream_fn(&self, stream_fn: StreamFn) {
        self.lock().stream_fn = Some(stream_fn);
    }
    pub fn hooks(&self) -> AgentHooks {
        self.lock().hooks.clone()
    }
    pub fn set_hooks(&self, hooks: AgentHooks) {
        self.lock().hooks = hooks;
    }

    pub fn steer(&self, message: AgentMessage) {
        self.lock().steering.messages.push_back(message);
    }
    pub fn follow_up(&self, message: AgentMessage) {
        self.lock().follow_up.messages.push_back(message);
    }
    pub fn clear_steering_queue(&self) {
        self.lock().steering.messages.clear();
    }
    pub fn clear_follow_up_queue(&self) {
        self.lock().follow_up.messages.clear();
    }
    pub fn clear_all_queues(&self) {
        let mut inner = self.lock();
        inner.steering.messages.clear();
        inner.follow_up.messages.clear();
    }
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.lock();
        !inner.steering.messages.is_empty() || !inner.follow_up.messages.is_empty()
    }
    pub fn peek_queued_messages(&self) -> Vec<AgentMessage> {
        let inner = self.lock();
        if inner.steering.messages.is_empty() {
            inner.follow_up.peek()
        } else {
            inner.steering.peek()
        }
    }
    pub fn signal(&self) -> Option<CancellationToken> {
        self.lock().active.as_ref().map(|run| run.signal.clone())
    }
    pub fn abort(&self) {
        if let Some(signal) = self.signal() {
            signal.cancel();
        }
    }
    pub fn wait_for_idle(&self) -> impl Future<Output = ()> + use<> {
        let settled = self.lock().active.as_ref().map(|run| run.settled.clone());
        async move {
            if let Some(settled) = settled {
                settled.cancelled().await;
            }
        }
    }

    pub fn reset(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.active.is_some() {
            return Err(AgentError::ResetInProgress);
        }
        let baseline = replay_system(&inner.state.messages);
        inner.state.messages.clear();
        if let Some(baseline) = baseline {
            inner.state.messages.push(baseline.into());
        }
        inner.state.streaming_message = None;
        inner.state.pending_tool_calls.clear();
        inner.state.error_message = None;
        inner.steering.messages.clear();
        inner.follow_up.messages.clear();
        Ok(())
    }

    pub async fn prompt(&self, input: impl Into<Prompt>) -> Result<()> {
        let messages = match input.into() {
            Prompt::Text(text) => vec![prompt_text(text, Vec::new())],
            Prompt::Message(message) => vec![message],
            Prompt::Messages(messages) => messages,
        };
        self.run(Some(messages)).await
    }

    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> Result<()> {
        self.run(Some(vec![prompt_text(text.into(), images)])).await
    }

    pub async fn continue_run(&self) -> Result<()> {
        self.run(None).await
    }

    async fn run(&self, prompts: Option<Vec<AgentMessage>>) -> Result<()> {
        let (mut guard, request) = self.begin(prompts)?;
        let RunRequest {
            prompts,
            context,
            config,
            signal,
            stream_fn,
        } = request;
        let emit = |event| {
            let signal = &signal;
            Box::pin(async move { self.process_event(event, signal).await }) as _
        };
        let result = if let Some(prompts) = prompts {
            run_agent_loop(
                prompts,
                context,
                config,
                &emit,
                Some(signal.clone()),
                stream_fn,
            )
            .await
        } else {
            run_agent_loop_continue(context, config, &emit, Some(signal.clone()), stream_fn).await
        };
        let result = match result {
            Ok(_) => Ok(()),
            Err(error) => self.handle_run_failure(error, &signal).await,
        };
        guard.finished = true;
        result
    }

    fn begin(&self, prompts: Option<Vec<AgentMessage>>) -> Result<(RunGuard<'_>, RunRequest)> {
        let is_prompt = prompts.is_some();
        let mut inner = self.lock();
        if inner.active.is_some() {
            return Err(if is_prompt {
                AgentError::PromptInProgress
            } else {
                AgentError::ContinueInProgress
            });
        }
        let mut prompts = prompts;
        let mut skip_steering = false;
        if !is_prompt {
            if inner
                .state
                .messages
                .iter()
                .all(|message| matches!(message, AgentMessage::System(_)))
            {
                return Err(AgentError::NoConversation);
            }
            if matches!(
                inner.state.messages.last(),
                Some(AgentMessage::Assistant(_))
            ) {
                let mut queued = inner.steering.drain();
                skip_steering = !queued.is_empty();
                if !skip_steering {
                    queued = inner.follow_up.drain();
                }
                if queued.is_empty() {
                    return Err(AgentError::AssistantTail);
                }
                prompts = Some(queued);
            }
        }
        let context = AgentContext {
            messages: inner.state.messages.clone(),
            tools: inner.state.tools.clone(),
        };
        let signal = CancellationToken::new();
        let mut config = AgentLoopConfig::new(inner.state.model.clone());
        config.stream_options = inner.stream_options.clone();
        config
            .stream_options
            .transport
            .get_or_insert(Transport::Auto);
        let reasoning = inner.state.thinking_level;
        config.stream_options.reasoning = (reasoning != ThinkingLevel::Off).then_some(reasoning);
        config.hooks = inner.hooks.clone();
        config.tool_execution = inner.tool_execution;
        let agent = self.clone();
        let skip = std::sync::atomic::AtomicBool::new(skip_steering);
        config.get_steering_messages = Some(Arc::new(move || {
            let messages = if skip.swap(false, std::sync::atomic::Ordering::Relaxed) {
                Vec::new()
            } else {
                agent.lock().steering.drain()
            };
            Box::pin(std::future::ready(messages))
        }));
        let agent = self.clone();
        config.get_follow_up_messages = Some(Arc::new(move || {
            Box::pin(std::future::ready(agent.lock().follow_up.drain()))
        }));
        let stream_fn = inner
            .proxy
            .clone()
            .map(proxy_stream_fn)
            .or_else(|| inner.stream_fn.clone());
        inner.active = Some(ActiveRun {
            signal: signal.clone(),
            settled: CancellationToken::new(),
        });
        inner.state.is_streaming = true;
        inner.state.streaming_message = None;
        inner.state.error_message = None;
        Ok((
            RunGuard {
                agent: self,
                finished: false,
            },
            RunRequest {
                prompts,
                context,
                config,
                signal,
                stream_fn,
            },
        ))
    }

    async fn handle_run_failure(
        &self,
        error: AgentError,
        signal: &CancellationToken,
    ) -> Result<()> {
        let mut assistant = AssistantMessage::pending(&self.model());
        assistant.stop_reason = if signal.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        assistant.content.push(AssistantContent::Text("".into()));
        assistant.error_message = Some(error.to_string());
        let message = AgentMessage::from(assistant);
        self.process_event(
            AgentEvent::MessageStart {
                message: message.clone(),
            },
            signal,
        )
        .await?;
        self.process_event(
            AgentEvent::MessageEnd {
                message: message.clone(),
            },
            signal,
        )
        .await?;
        self.process_event(
            AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: Vec::new(),
            },
            signal,
        )
        .await?;
        self.process_event(
            AgentEvent::AgentEnd {
                messages: [message].into(),
            },
            signal,
        )
        .await
    }

    async fn process_event(&self, event: AgentEvent, signal: &CancellationToken) -> Result<()> {
        {
            let mut inner = self.lock();
            match &event {
                AgentEvent::MessageStart { message } => {
                    inner.state.streaming_message = Some(StreamingMessage::Fixed(message.clone()));
                }
                AgentEvent::MessageUpdate { message, .. } => {
                    inner.state.streaming_message =
                        Some(StreamingMessage::Assistant(message.clone()));
                }
                AgentEvent::MessageEnd { message } => {
                    inner.state.streaming_message = None;
                    inner.state.messages.push(message.clone());
                }
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    inner.state.pending_tool_calls.insert(tool_call_id.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    inner.state.pending_tool_calls.shift_remove(tool_call_id);
                }
                AgentEvent::TurnEnd {
                    message: AgentMessage::Assistant(message),
                    ..
                } => {
                    if let Some(error) = &message.error_message
                        && !error.is_empty()
                    {
                        inner.state.error_message = Some(error.clone());
                    }
                }
                AgentEvent::AgentEnd { .. } => {
                    inner.state.streaming_message = None;
                }
                _ => {}
            }
        }
        let mut last = Unbounded;
        loop {
            let next = self
                .lock()
                .listeners
                .range((last, Unbounded))
                .next()
                .map(|(&id, listener)| (id, listener.clone()));
            let Some((id, listener)) = next else {
                return Ok(());
            };
            listener(&event, signal).await?;
            last = Excluded(id);
        }
    }
}

struct RunRequest {
    prompts: Option<Vec<AgentMessage>>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: CancellationToken,
    stream_fn: Option<StreamFn>,
}

fn replay_system(messages: &[AgentMessage]) -> Option<SystemMessage> {
    let systems: Vec<_> = system_messages(messages)
        .cloned()
        .map(Message::System)
        .collect();
    get_current_system_message(&systems)
}

fn prompt_text(text: String, images: Vec<ImageContent>) -> AgentMessage {
    let mut content = Vec::with_capacity(1 + images.len());
    content.push(InputContent::Text(text.into()));
    content.extend(images.into_iter().map(InputContent::Image));
    UserMessage {
        role: UserRole::User,
        content: StringOrBlocks::Blocks(content),
        timestamp: unix_millis(),
    }
    .into()
}

fn unknown_model() -> Model {
    Model {
        base: BaseModel {
            id: "unknown".into(),
            name: "unknown".into(),
            api: Api::Custom("unknown".into()),
            provider: "unknown".into(),
            base_url: String::new(),
            input: Vec::new(),
            input_limits: None,
            cost: Default::default(),
            headers: None,
        },
        kind: None,
        reasoning: false,
        thinking_level_map: None,
        prompt_cache: None,
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        compat: None,
    }
}
