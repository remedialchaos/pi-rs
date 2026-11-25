use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::chat_state::ChatState;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, write_partial};
use crate::http::abortable;
use crate::model::{
    AnyModel, Api, BaseModel, CacheRetention, Model, ModelCost, ModelInputLimits,
    ModelInputModality,
};
use crate::providers::keyless_auth;
use crate::registry::Provider;
use crate::request::{ProviderResponse, StreamOptions};
use crate::transcript::system_message_text;
use crate::types::{
    AssistantContent, AssistantMessage, DeferredHandle, InputContent, JsonObject, Message,
    StopReason, StringOrBlocks, TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage,
};

#[derive(Clone, Default)]
pub struct FauxOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Vec<FauxModelDefinition>,
    pub pending_fetches: usize,
    pub poll_after_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<(usize, usize)>,
}
#[derive(Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: bool,
    pub input: Option<Vec<ModelInputModality>>,
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}
impl Default for FauxModelDefinition {
    fn default() -> Self {
        Self {
            id: "faux-1".into(),
            name: Some("Faux Model".into()),
            reasoning: false,
            input: None,
            input_limits: None,
            cost: ModelCost::default(),
            context_window: None,
            max_tokens: None,
        }
    }
}
pub type FauxFactory = Arc<
    dyn for<'a> Fn(
            &'a TranscriptContext,
            &'a StreamOptions,
            &'a FauxState,
            &'a Model,
        ) -> BoxFuture<'a, Result<AssistantMessage>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub enum FauxStep {
    Message(Box<AssistantMessage>),
    Factory(FauxFactory),
}
impl From<AssistantMessage> for FauxStep {
    fn from(message: AssistantMessage) -> Self {
        Self::Message(Box::new(message))
    }
}
#[derive(Default)]
pub struct FauxState {
    calls: AtomicUsize,
    fetches: AtomicUsize,
    cancelled: Mutex<Vec<DeferredHandle>>,
}
impl FauxState {
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn deferred_fetch_count(&self) -> usize {
        self.fetches.load(Ordering::Relaxed)
    }
    pub fn cancelled_deferred(&self) -> Vec<DeferredHandle> {
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
struct Deferred {
    handle: DeferredHandle,
    step: FauxStep,
    context: TranscriptContext,
    options: StreamOptions,
    model: Model,
    pending: AtomicUsize,
    cancelled: std::sync::atomic::AtomicBool,
    result: OnceCell<AssistantMessage>,
}
struct Core {
    state: Arc<FauxState>,
    queue: Mutex<VecDeque<FauxStep>>,
    cache: Mutex<HashMap<String, Vec<String>>>,
    deferred: Mutex<HashMap<String, Arc<Deferred>>>,
    pending: usize,
    poll_after_ms: Option<u64>,
    rate: Option<f64>,
    chunk: (usize, usize),
}
pub struct FauxHandle {
    pub provider: Provider,
    pub models: Arc<[Model]>,
    pub state: Arc<FauxState>,
    core: Arc<Core>,
}
impl FauxHandle {
    pub fn get_model(&self, id: Option<&str>) -> Option<&Model> {
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            self.models.iter().find(|m| m.id == id)
        } else {
            self.models.first()
        }
    }
    pub fn set_responses(&self, responses: impl IntoIterator<Item = FauxStep>) {
        *self.core.queue.lock().unwrap_or_else(|e| e.into_inner()) =
            responses.into_iter().collect();
    }
    pub fn append_responses(&self, responses: impl IntoIterator<Item = FauxStep>) {
        self.core
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(responses);
    }
    pub fn pending_response_count(&self) -> usize {
        self.core
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}
pub fn faux_text(text: impl Into<String>) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        ..TextContent::default()
    })
}
pub fn faux_thinking(thinking: impl Into<String>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: thinking.into(),
        ..ThinkingContent::default()
    })
}
pub fn faux_tool_call(
    name: impl Into<String>,
    arguments: JsonObject,
    id: Option<String>,
) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.unwrap_or_else(|| id_for("tool")),
        name: name.into(),
        arguments,
        ..ToolCall::default()
    })
}
fn id_for(prefix: &str) -> String {
    format!("{prefix}:{}", uuid::Uuid::now_v7())
}
fn faux_model(d: FauxModelDefinition, api: &Api, provider: &str) -> Model {
    Model {
        base: BaseModel {
            id: d.id.clone(),
            name: d.name.unwrap_or(d.id),
            api: api.clone(),
            provider: provider.into(),
            base_url: "http://localhost:0".into(),
            input: d
                .input
                .unwrap_or_else(|| vec![ModelInputModality::Text, ModelInputModality::Image]),
            input_limits: d.input_limits,
            cost: d.cost,
            headers: None,
        },
        reasoning: d.reasoning,
        kind: None,
        thinking_level_map: None,
        prompt_cache: None,
        context_window: d.context_window.unwrap_or(128000),
        max_tokens: d.max_tokens.unwrap_or(16384),
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        compat: None,
    }
}
pub fn faux_assistant_message(content: Vec<AssistantContent>) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&faux_model(
        FauxModelDefinition::default(),
        &Api::from("faux"),
        "faux",
    ));
    m.content = content;
    m.stop_reason = StopReason::Stop;
    m
}
pub fn faux_provider(opts: FauxOptions) -> FauxHandle {
    let name = opts.provider.unwrap_or_else(|| "faux".into());
    let api = Api::from(opts.api.unwrap_or_else(|| id_for("faux")));
    let models = if opts.models.is_empty() {
        vec![FauxModelDefinition::default()]
    } else {
        opts.models
    };
    let models: Arc<[Model]> = models
        .into_iter()
        .map(|d| faux_model(d, &api, &name))
        .collect::<Vec<_>>()
        .into();
    let (min, max) = opts.token_size.unwrap_or((3, 5));
    let min = min.min(max).max(1);
    let max = max.max(min);
    let state = Arc::new(FauxState::default());
    let core = Arc::new(Core {
        state: state.clone(),
        queue: Mutex::default(),
        cache: Mutex::default(),
        deferred: Mutex::default(),
        pending: opts.pending_fetches,
        poll_after_ms: opts.poll_after_ms,
        rate: opts.tokens_per_second,
        chunk: (min, max),
    });
    let mut provider = Provider::new(name, "Faux");
    provider.auth.api_key = Some(keyless_auth());
    provider.chat_apis.insert(api);
    provider.models = models
        .iter()
        .cloned()
        .map(AnyModel::Chat)
        .collect::<Vec<_>>()
        .into();
    let chat = core.clone();
    provider.chat = Some(Arc::new(move |model, context, opts, _| {
        chat.clone().stream(model, Request::Submit(context), opts)
    }));
    let poll = core.clone();
    provider.poll = Some(Arc::new(move |model, handle, opts| {
        poll.clone().stream(model, Request::Poll(handle), opts)
    }));
    let cancel = core.clone();
    provider.cancel = Some(Arc::new(move |model, handle, opts| {
        let core = cancel.clone();
        Box::pin(async move {
            if let Some(entry) = core
                .deferred
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&handle.id)
            {
                entry.cancelled.store(true, Ordering::Relaxed);
            }
            core.state
                .cancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(handle);
            Core::response(&model, &opts).await
        })
    }));
    FauxHandle {
        provider,
        models,
        state,
        core,
    }
}
enum Request {
    Submit(TranscriptContext),
    Poll(DeferredHandle),
}
impl Core {
    fn stream(
        self: Arc<Self>,
        model: Model,
        request: Request,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut state, stream) = ChatState::new(&model);
        let step = if matches!(&request, Request::Submit(_)) {
            self.state.calls.fetch_add(1, Ordering::Relaxed);
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
        } else {
            self.state.fetches.fetch_add(1, Ordering::Relaxed);
            None
        };
        tokio::spawn(async move {
            let result = async {
                Self::response(&model, &opts).await?;
                let message = match request {
                    Request::Submit(ctx) => {
                        if let Some(step) = step {
                            if opts
                                .deferred
                                .as_ref()
                                .is_some_and(|d| !d.is_null() && *d != Value::Bool(false))
                            {
                                let handle = DeferredHandle {
                                    provider: model.provider.clone(),
                                    model_id: model.id.clone(),
                                    api: model.api.clone(),
                                    id: id_for("deferred"),
                                    expires_at: None,
                                    poll_after_ms: self.poll_after_ms,
                                    data: None,
                                };
                                let mut submission = opts.clone();
                                submission.deferred = None;
                                submission.request.signal = Default::default();
                                submission.request.on_response = None;
                                let entry = Arc::new(Deferred {
                                    handle: handle.clone(),
                                    step,
                                    context: ctx,
                                    options: submission,
                                    model: model.clone(),
                                    pending: AtomicUsize::new(self.pending),
                                    cancelled: Default::default(),
                                    result: OnceCell::new(),
                                });
                                self.deferred
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(handle.id.clone(), entry);
                                deferred(&model, handle)
                            } else {
                                self.resolve(step, &ctx, &opts, &model).await?
                            }
                        } else {
                            let mut m = AssistantMessage::pending(&model);
                            m.stop_reason = StopReason::Error;
                            m.error_message = Some("No more faux responses queued".into());
                            self.estimate(&mut m, &ctx, &opts);
                            m
                        }
                    }
                    Request::Poll(handle) => {
                        let entry = self
                            .deferred
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&handle.id)
                            .cloned()
                            .filter(|e| {
                                e.handle.api == handle.api
                                    && e.handle.provider == handle.provider
                                    && e.handle.model_id == handle.model_id
                            })
                            .ok_or_else(|| {
                                Error::Protocol(format!(
                                    "Unknown faux deferred response: {}",
                                    handle.id
                                ))
                            })?;
                        if entry.cancelled.load(Ordering::Relaxed) {
                            return Err(Error::Protocol(format!(
                                "Faux deferred response was cancelled: {}",
                                handle.id
                            )));
                        }
                        if entry
                            .pending
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                                n.checked_sub(1)
                            })
                            .is_ok()
                        {
                            deferred(&model, entry.handle.clone())
                        } else {
                            entry
                                .result
                                .get_or_init(|| async {
                                    self.resolve(
                                        entry.step.clone(),
                                        &entry.context,
                                        &entry.options,
                                        &entry.model,
                                    )
                                    .await
                                    .unwrap_or_else(|e| {
                                        crate::event_stream::fail(
                                            AssistantMessage::pending(&entry.model),
                                            e,
                                        )
                                    })
                                })
                                .await
                                .clone()
                        }
                    }
                };
                self.emit(&mut state, message, &opts).await
            }
            .await;
            state.finish(result);
        });
        stream
    }
    async fn response(model: &Model, opts: &StreamOptions) -> Result<()> {
        if let Some(observer) = &opts.request.on_response {
            abortable(
                &opts.request.signal,
                observer(
                    &ProviderResponse {
                        status: 200,
                        headers: Default::default(),
                    },
                    model,
                ),
            )
            .await?;
        }
        Ok(())
    }
    async fn resolve(
        &self,
        step: FauxStep,
        ctx: &TranscriptContext,
        opts: &StreamOptions,
        model: &Model,
    ) -> Result<AssistantMessage> {
        let mut m = match step {
            FauxStep::Message(m) => *m,
            FauxStep::Factory(f) => f(ctx, opts, &self.state, model).await?,
        };
        m.api.clone_from(&model.api);
        m.provider.clone_from(&model.provider);
        m.model.clone_from(&model.id);
        self.estimate(&mut m, ctx, opts);
        Ok(m)
    }
    fn estimate(&self, m: &mut AssistantMessage, ctx: &TranscriptContext, opts: &StreamOptions) {
        let prompt = ctx.messages().iter().map(message_text).collect::<Vec<_>>();
        let chars = prompt
            .iter()
            .map(|p| p.encode_utf16().count())
            .sum::<usize>()
            + prompt.len().saturating_sub(1) * 2;
        let output = tokens(&assistant_text(&m.content));
        let mut input = chars.div_ceil(4);
        let mut read = 0;
        let mut write = 0;
        if opts.cache_retention != Some(CacheRetention::None)
            && let Some(session) = opts.session_id.as_deref().filter(|s| !s.is_empty())
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(previous) = cache.get(session) {
                fn utf16(parts: &[String]) -> impl Iterator<Item = u16> + '_ {
                    parts
                        .iter()
                        .enumerate()
                        .flat_map(|(i, s)| {
                            (i != 0)
                                .then_some("\n\n")
                                .into_iter()
                                .chain(std::iter::once(s.as_str()))
                        })
                        .flat_map(str::encode_utf16)
                }
                let old = utf16(previous);
                let new = utf16(&prompt);
                read = old.zip(new).take_while(|(a, b)| a == b).count().div_ceil(4);
            }
            write = input.saturating_sub(read);
            input = 0;
            cache.insert(session.to_owned(), prompt);
        }
        m.usage = Usage {
            input: input as f64,
            output: output as f64,
            cache_read: read as f64,
            cache_write: write as f64,
            total_tokens: (input + output + read + write) as f64,
            ..Usage::default()
        };
    }
    async fn emit(
        &self,
        state: &mut ChatState,
        mut message: AssistantMessage,
        opts: &StreamOptions,
    ) -> Result<StopReason> {
        let signal = &opts.request.signal;
        if signal.is_cancelled() {
            return Err(Error::Aborted);
        }
        let content = std::mem::take(&mut message.content);
        let reason = message.stop_reason;
        message.stop_reason = StopReason::Pending;
        *write_partial(&state.message) = message;
        if reason == StopReason::Error && content.is_empty() {
            return Ok(reason);
        }
        state.start();
        for block in content {
            let (i, text) = match block {
                AssistantContent::Text(mut t) => {
                    let text = std::mem::take(&mut t.text);
                    (state.text_start(t.text_signature), text)
                }
                AssistantContent::Thinking(mut t) => {
                    let text = std::mem::take(&mut t.thinking);
                    (state.thinking_start(t), text)
                }
                AssistantContent::ToolCall(mut t) => {
                    let text = serde_json::to_string(&std::mem::take(&mut t.arguments))?;
                    (state.tool_start(t), text)
                }
            };
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let width = rand::random_range(self.chunk.0..=self.chunk.1).saturating_mul(4);
                let mut units = 0;
                let end = rest
                    .char_indices()
                    .find_map(|(start, c)| {
                        units += c.len_utf16();
                        (units > width && start != 0).then_some(start)
                    })
                    .unwrap_or(rest.len());
                let (delta, next) = rest.split_at(end);
                rest = next;
                abortable(signal, async {
                    if let Some(rate) = self.rate.filter(|r| r.is_finite() && *r > 0.0) {
                        let delay = Duration::try_from_secs_f64(tokens(delta) as f64 / rate)
                            .unwrap_or(Duration::MAX);
                        tokio::time::sleep(delay).await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                    Ok(())
                })
                .await?;
                state.delta(i, delta.into())?;
            }
            state.end(i);
        }
        if reason == StopReason::Pending {
            return Err(Error::Protocol(
                "Faux response ended without a stop reason".into(),
            ));
        }
        Ok(reason)
    }
}
fn deferred(model: &Model, handle: DeferredHandle) -> AssistantMessage {
    let mut m = AssistantMessage::pending(model);
    m.stop_reason = StopReason::Deferred;
    m.deferred = Some(handle);
    m
}
fn tokens(s: &str) -> usize {
    s.encode_utf16().count().div_ceil(4)
}
fn assistant_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|b| match b {
            AssistantContent::Text(t) => t.text.clone(),
            AssistantContent::Thinking(t) => t.thinking.clone(),
            AssistantContent::ToolCall(t) => format!(
                "{}:{}",
                t.name,
                serde_json::to_string(&t.arguments).unwrap_or_default()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn input_text(content: &[InputContent]) -> String {
    content
        .iter()
        .map(|b| match b {
            InputContent::Text(t) => t.text.clone(),
            InputContent::Image(i) => {
                format!("[image:{}:{}]", i.mime_type, i.data.encode_utf16().count())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn message_text(m: &Message) -> String {
    match m {
        Message::System(s) => {
            let text = system_message_text(s);
            let mut out = format!("system:{text}");
            let mut needs_newline = !text.is_empty();
            for (prefix, value) in s
                .tools_removed
                .iter()
                .flatten()
                .map(|t| ("tool-:", serde_json::to_string(t)))
                .chain(
                    s.tools_added
                        .iter()
                        .flatten()
                        .map(|t| ("tool+:", serde_json::to_string(t))),
                )
            {
                if let Ok(value) = value {
                    if needs_newline {
                        out.push('\n');
                    }
                    out.push_str(prefix);
                    out.push_str(&value);
                    needs_newline = true;
                }
            }
            out
        }
        Message::User(u) => format!(
            "user:{}",
            match &u.content {
                StringOrBlocks::String(s) => s.clone(),
                StringOrBlocks::Blocks(parts) => input_text(parts),
            }
        ),
        Message::Assistant(a) => format!("assistant:{}", assistant_text(&a.content)),
        Message::ToolResult(t) => {
            let mut text = format!("toolResult:{}", t.tool_name);
            if !t.content.is_empty() {
                text.push('\n');
                text.push_str(&input_text(&t.content));
            }
            text
        }
    }
}
