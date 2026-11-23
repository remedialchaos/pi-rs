//! Per-conversation prompt cache warming scheduler, mirroring `apps/p2/plugins/cache-warming/warmer.ts`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_ai::{
    AssistantMessage, CACHE_WARMING_MAX_AGE_MS, CACHE_WARMING_MAX_IDLE_AGE_MS, CacheWarmingAction,
    CacheWarmingPhase, Context as AiContext, Message, Model, Models, StopReason, StreamOptions,
    evaluate_cache_warming, get_cache_warming_delay_ms, get_cache_warming_request_options,
    get_prompt_cache_ttl_ms, is_cache_warming_replayable,
};
use pi_rs_chord::sync::SyncFuture;
use pi_rs_durable::ConversationId;
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheWarmingMode {
    Off,
    Streaming,
    Idle,
}

impl CacheWarmingMode {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("off") => Self::Off,
            Some("streaming") => Self::Streaming,
            _ => Self::Idle,
        }
    }
}

#[derive(Clone)]
pub struct WarmRequest {
    pub model: Model,
    pub messages: Vec<Message>,
    pub options: StreamOptions,
}

pub trait WarmerHost: Send + Sync + 'static {
    fn mode(&self) -> CacheWarmingMode;
    fn phase(
        &self,
        conversation_id: ConversationId,
        request: WarmRequest,
    ) -> SyncFuture<Option<CacheWarmingPhase>>;
    fn record(&self, conversation_id: ConversationId, message: AssistantMessage) -> SyncFuture<()>;
    fn keep_alive(&self, delta: isize);
}

struct RunHandle {
    id: u64,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct Inner {
    host: Arc<dyn WarmerHost>,
    runs: Mutex<HashMap<ConversationId, RunHandle>>,
    next_id: Mutex<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Manages one active warming timer loop per conversation.
#[derive(Clone)]
pub struct ConversationWarmers {
    inner: Arc<Inner>,
}

impl ConversationWarmers {
    pub fn new(host: Arc<dyn WarmerHost>) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                runs: Mutex::default(),
                next_id: Mutex::new(0),
            }),
        }
    }

    pub fn active_count(&self) -> usize {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn start(
        &self,
        conversation_id: ConversationId,
        message: &AssistantMessage,
        request: WarmRequest,
        models: Models,
    ) {
        self.stop(conversation_id);
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            return;
        }
        if !is_cache_warming_replayable(&request.model, Some(&request.options)) {
            return;
        }
        let Some(ttl_ms) = get_prompt_cache_ttl_ms(&request.model, Some(&request.options)) else {
            return;
        };
        let Some(delay_ms) = get_cache_warming_delay_ms(ttl_ms) else {
            return;
        };
        if !delay_ms.is_finite() || delay_ms < 1.0 {
            return;
        }
        let prompt_tokens =
            message.usage.input + message.usage.cache_read + message.usage.cache_write;
        let started_at = now_ms();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let run_id = {
            let mut id = self.inner.next_id.lock().unwrap_or_else(|p| p.into_inner());
            *id += 1;
            *id
        };

        let inner = self.inner.clone();
        let delay = Duration::from_millis(delay_ms as u64);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                if *cancel_rx.borrow() {
                    return;
                }
                if !wanted(
                    &inner,
                    conversation_id,
                    run_id,
                    &request,
                    prompt_tokens,
                    started_at,
                )
                .await
                {
                    stop_if_matching(&inner, conversation_id, run_id);
                    return;
                }
                let opts = get_cache_warming_request_options(
                    request.options.clone(),
                    request.options.request.signal.clone(),
                );
                let ai_ctx = AiContext {
                    messages: request.messages.clone(),
                    ..Default::default()
                };
                if let Ok(reply) = models
                    .complete_simple(request.model.clone(), ai_ctx, opts)
                    .await
                {
                    if !is_current(&inner, conversation_id, run_id) {
                        return;
                    }
                    if !matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
                        let _ = inner.host.record(conversation_id, (*reply).clone()).await;
                    }
                }
                if !is_current(&inner, conversation_id, run_id) {
                    return;
                }
            }
        });

        self.inner.host.keep_alive(1);
        self.inner
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                conversation_id,
                RunHandle {
                    id: run_id,
                    cancel: cancel_tx,
                    task,
                },
            );
    }

    pub fn stop(&self, conversation_id: ConversationId) {
        let removed = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&conversation_id);
        if let Some(handle) = removed {
            let _ = handle.cancel.send(true);
            handle.task.abort();
            self.inner.host.keep_alive(-1);
        }
    }

    pub fn close(&self) {
        let ids: Vec<ConversationId> = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.stop(id);
        }
    }
}

fn is_current(inner: &Arc<Inner>, conversation_id: ConversationId, run_id: u64) -> bool {
    inner
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&conversation_id)
        .is_some_and(|handle| handle.id == run_id)
}

fn stop_if_matching(inner: &Arc<Inner>, conversation_id: ConversationId, run_id: u64) {
    let removed = {
        let mut runs = inner.runs.lock().unwrap_or_else(|p| p.into_inner());
        if runs
            .get(&conversation_id)
            .is_some_and(|handle| handle.id == run_id)
        {
            runs.remove(&conversation_id)
        } else {
            None
        }
    };
    if let Some(handle) = removed {
        let _ = handle.cancel.send(true);
        inner.host.keep_alive(-1);
    }
}

async fn wanted(
    inner: &Arc<Inner>,
    conversation_id: ConversationId,
    run_id: u64,
    request: &WarmRequest,
    prompt_tokens: f64,
    started_at: u64,
) -> bool {
    let mode = inner.host.mode();
    if mode == CacheWarmingMode::Off {
        return false;
    }
    let phase = match inner.host.phase(conversation_id, request.clone()).await {
        Ok(Some(phase)) => phase,
        _ => return false,
    };
    if phase == CacheWarmingPhase::Idle && mode == CacheWarmingMode::Streaming {
        return false;
    }
    let max_age = if phase == CacheWarmingPhase::Idle {
        CACHE_WARMING_MAX_IDLE_AGE_MS
    } else {
        CACHE_WARMING_MAX_AGE_MS
    };
    if now_ms() > started_at.saturating_add(max_age) {
        return false;
    }
    let decision = evaluate_cache_warming(&request.model, prompt_tokens, phase);
    decision.action == CacheWarmingAction::Warm && is_current(inner, conversation_id, run_id)
}
