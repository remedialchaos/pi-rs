//! Background prompt cache warming scheduler.
//!
//! Port of `packages/coding-agent/src/core/cache-warmer.ts`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::cache_warming::{
    CACHE_WARMING_MAX_AGE_MS, CACHE_WARMING_MAX_IDLE_AGE_MS,
    CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS, CacheWarmingAction, CacheWarmingDecision,
    CacheWarmingPhase, evaluate_cache_warming, get_cache_warming_delay_ms,
    get_cache_warming_request_options, get_prompt_cache_ttl_ms, is_cache_warming_replayable,
};
use pi_rs_ai::{Context, Model, StopReason, StreamOptions, Usage};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingMode {
    Off,
    Streaming,
    #[default]
    Idle,
}

impl CacheWarmingMode {
    pub fn from_setting(value: &str) -> Self {
        match value {
            "off" => Self::Off,
            "streaming" => Self::Streaming,
            _ => Self::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Streaming => "streaming",
            Self::Idle => "idle",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingState {
    Inactive,
    Scheduled,
    Refreshing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingStatus {
    pub state: CacheWarmingState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_warm_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<CacheWarmingDecision>,
    #[serde(default)]
    pub extension_override: bool,
}

impl Default for CacheWarmingStatus {
    fn default() -> Self {
        Self {
            state: CacheWarmingState::Inactive,
            reason: Some("waiting for first request".to_owned()),
            next_warm_at: None,
            decision: None,
            extension_override: false,
        }
    }
}

pub struct CacheWarmRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
}

pub type WarmStreamFn = Arc<
    dyn Fn(
            Model,
            Context,
            StreamOptions,
        ) -> BoxFuture<'static, Result<(Option<String>, Usage, StopReason), String>>
        + Send
        + Sync,
>;
pub type WarmModeFn = Arc<dyn Fn() -> CacheWarmingMode + Send + Sync>;
pub type WarmPromptTokensFn = Arc<dyn Fn() -> f64 + Send + Sync>;
pub type WarmIsCurrentFn = Arc<dyn Fn() -> bool + Send + Sync>;
pub type WarmDecideFn =
    Arc<dyn Fn(CacheWarmingDecision) -> BoxFuture<'static, CacheWarmingAction> + Send + Sync>;
pub type WarmRecordUsageFn = Arc<dyn Fn(String, String, Usage, Option<String>) + Send + Sync>;

struct ActiveRun {
    id: u64,
    request: CacheWarmRequest,
    is_current: WarmIsCurrentFn,
    ttl_ms: f64,
    delay_ms: u64,
    refresh_deadline_at: u64,
    cancel: CancellationToken,
    phase: CacheWarmingPhase,
    next_warm_at: u64,
    refreshing: bool,
    extension_override: bool,
    task: Option<JoinHandle<()>>,
}

struct Inner {
    next_id: u64,
    run: Option<ActiveRun>,
    inactive: CacheWarmingStatus,
    stream: WarmStreamFn,
    get_mode: WarmModeFn,
    last_prompt_tokens: WarmPromptTokensFn,
    decide: WarmDecideFn,
    record_usage: WarmRecordUsageFn,
}

#[derive(Clone)]
pub struct CacheWarmer {
    inner: Arc<Mutex<Inner>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

impl CacheWarmer {
    pub fn new(
        stream: WarmStreamFn,
        get_mode: WarmModeFn,
        last_prompt_tokens: WarmPromptTokensFn,
        decide: WarmDecideFn,
        record_usage: WarmRecordUsageFn,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_id: 1,
                run: None,
                inactive: CacheWarmingStatus::default(),
                stream,
                get_mode,
                last_prompt_tokens,
                decide,
                record_usage,
            })),
        }
    }

    pub fn status(&self) -> CacheWarmingStatus {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if (inner.get_mode)() == CacheWarmingMode::Off {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("cache warming disabled".to_owned()),
                next_warm_at: None,
                decision: None,
                extension_override: false,
            };
        }
        let Some(run) = &inner.run else {
            return inner.inactive.clone();
        };
        if !(run.is_current)() {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("conversation context changed".to_owned()),
                next_warm_at: None,
                decision: None,
                extension_override: false,
            };
        }
        let decision =
            evaluate_cache_warming(&run.request.model, (inner.last_prompt_tokens)(), run.phase);
        if !decision.economics_available && !run.refreshing {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("cache economics unavailable".to_owned()),
                next_warm_at: None,
                decision: None,
                extension_override: false,
            };
        }
        CacheWarmingStatus {
            state: if run.refreshing {
                CacheWarmingState::Refreshing
            } else {
                CacheWarmingState::Scheduled
            },
            reason: None,
            next_warm_at: Some(run.next_warm_at),
            decision: Some(decision),
            extension_override: run.extension_override,
        }
    }

    pub fn start(&self, request: CacheWarmRequest, is_current: WarmIsCurrentFn) {
        self.stop_with("replaced", None, false);
        let mode = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (inner.get_mode)()
        };
        if mode == CacheWarmingMode::Off {
            self.stop_with("cache warming disabled", None, false);
            return;
        }
        let Some(ttl_ms) = get_prompt_cache_ttl_ms(&request.model, Some(&request.options)) else {
            self.stop_with("model has no prompt cache TTL", None, false);
            return;
        };
        if !is_cache_warming_replayable(&request.model, Some(&request.options)) {
            self.stop_with("thinking request cannot be replayed", None, false);
            return;
        }
        let Some(delay) = get_cache_warming_delay_ms(ttl_ms) else {
            self.stop_with("prompt cache TTL too short", None, false);
            return;
        };
        if !delay.is_finite() {
            self.stop_with("prompt cache TTL too short", None, false);
            return;
        }
        let delay_ms = delay as u64;
        let started_at = now_ms();
        let max_age_margin = ((ttl_ms - delay_ms as f64).floor().max(0.0) as u64) / 2;
        let refresh_deadline_at =
            started_at + CACHE_WARMING_MAX_AGE_MS.saturating_add(max_age_margin);

        let run_id = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let id = inner.next_id;
            inner.next_id += 1;
            inner.run = Some(ActiveRun {
                id,
                request,
                is_current,
                ttl_ms,
                delay_ms,
                refresh_deadline_at,
                cancel: CancellationToken::new(),
                phase: CacheWarmingPhase::Streaming,
                next_warm_at: started_at + delay_ms,
                refreshing: false,
                extension_override: false,
                task: None,
            });
            id
        };
        self.schedule(run_id);
    }

    pub fn on_agent_settled(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mode = (inner.get_mode)();
        let Some(run) = &mut inner.run else {
            return;
        };
        run.phase = CacheWarmingPhase::Idle;
        if mode == CacheWarmingMode::Streaming {
            drop(inner);
            self.stop_with("agent run settled", None, false);
            return;
        }
        let idle_deadline = now_ms()
            + CACHE_WARMING_MAX_IDLE_AGE_MS
            + ((run.ttl_ms as u64).saturating_sub(run.delay_ms)) / 2;
        run.refresh_deadline_at = run.refresh_deadline_at.min(idle_deadline);
    }

    pub fn on_mode_changed(&self) {
        let (run_id, stop_reason) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(run) = &inner.run else {
                return;
            };
            let mode = (inner.get_mode)();
            let reason = if mode == CacheWarmingMode::Off {
                Some("cache warming disabled")
            } else if mode == CacheWarmingMode::Streaming && run.phase == CacheWarmingPhase::Idle {
                Some("agent run settled")
            } else {
                None
            };
            (run.id, reason)
        };
        if let Some(reason) = stop_reason {
            self.stop_with(reason, None, false);
        } else {
            self.schedule(run_id);
        }
    }

    pub fn cancel(&self, reason: &str) {
        self.stop_with(reason, None, false);
    }

    fn stop_with(
        &self,
        reason: &str,
        decision: Option<CacheWarmingDecision>,
        extension_override: bool,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut run) = inner.run.take() {
            run.cancel.cancel();
            if let Some(task) = run.task.take() {
                task.abort();
            }
        }
        inner.inactive = CacheWarmingStatus {
            state: CacheWarmingState::Inactive,
            reason: Some(reason.to_owned()),
            next_warm_at: None,
            decision,
            extension_override,
        };
    }

    fn schedule(&self, run_id: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(run) = &mut inner.run else {
            return;
        };
        if run.id != run_id {
            return;
        }
        if let Some(task) = run.task.take() {
            task.abort();
        }
        let now = now_ms();
        let next_warm_at = now + run.delay_ms;
        if next_warm_at > run.refresh_deadline_at {
            drop(inner);
            self.stop_with("cache refresh deadline reached", None, false);
            return;
        }
        run.next_warm_at = next_warm_at;
        run.refreshing = false;
        let cancel = run.cancel.clone();
        let delay = Duration::from_millis(run.delay_ms);
        let warmer = self.clone();
        run.task = Some(handle.spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            warmer.execute_warm(run_id).await;
        }));
    }

    async fn execute_warm(&self, run_id: u64) {
        let (decision, decide_fn, stream_fn, req_model, req_ctx, req_opts, cancel, record_usage) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mode = (inner.get_mode)();
            let tokens = (inner.last_prompt_tokens)();
            let decide_outer = inner.decide.clone();
            let stream_outer = inner.stream.clone();
            let record_outer = inner.record_usage.clone();
            let Some(run) = &mut inner.run else {
                return;
            };
            if run.id != run_id {
                return;
            }
            if mode == CacheWarmingMode::Off {
                drop(inner);
                self.stop_with("cache warming disabled", None, false);
                return;
            }
            if mode == CacheWarmingMode::Streaming && run.phase == CacheWarmingPhase::Idle {
                drop(inner);
                self.stop_with("agent run settled", None, false);
                return;
            }
            if !(run.is_current)() {
                drop(inner);
                self.stop_with("conversation context changed", None, false);
                return;
            }
            if now_ms() > run.refresh_deadline_at {
                drop(inner);
                self.stop_with("cache refresh deadline missed", None, false);
                return;
            }
            run.refreshing = true;
            let decision = evaluate_cache_warming(&run.request.model, tokens, run.phase);
            (
                decision,
                decide_outer,
                stream_outer,
                run.request.model.clone(),
                run.request.context.clone(),
                get_cache_warming_request_options(run.request.options.clone(), run.cancel.clone()),
                run.cancel.clone(),
                record_outer,
            )
        };

        let action = decide_fn(decision).await;
        if cancel.is_cancelled() {
            return;
        }
        let extension_override = action != decision.action;
        if action == CacheWarmingAction::Stop {
            let reason = if extension_override {
                "stopped by extension"
            } else if decision.economics_available {
                "expected savings below threshold"
            } else {
                "cache economics unavailable"
            };
            self.stop_with(reason, Some(decision), extension_override);
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(run) = &mut inner.run
                && run.id == run_id
            {
                run.extension_override = extension_override;
            } else {
                return;
            }
        }
        if let Ok((response_model, usage, stop_reason)) =
            stream_fn(req_model.clone(), req_ctx, req_opts).await
            && !cancel.is_cancelled()
            && !stop_reason.failed()
        {
            let model_name = response_model.unwrap_or_else(|| req_model.id.clone());
            let note = extension_override.then(|| "extension override".to_owned());
            record_usage(req_model.provider.clone(), model_name, usage, note);
        }
        let still_active = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.run.as_ref().is_some_and(|r| r.id == run_id)
        };
        if still_active {
            self.schedule(run_id);
        }
    }
}

fn format_dollars(value: f64) -> String {
    if value < 0.0 {
        format!("-${:.3}", value.abs())
    } else {
        format!("${value:.3}")
    }
}

fn format_cache_warming_economics(decision: &CacheWarmingDecision) -> String {
    if !decision.economics_available {
        return "cache economics unavailable".to_owned();
    }
    let probability = (decision.continuation_probability * 100.0).round() as i64;
    let probability_text = if decision.phase == CacheWarmingPhase::Streaming {
        format!("{probability}% continuation probability while agent is running")
    } else {
        format!("{probability}% continuation probability")
    };
    let comparison = if decision.action == CacheWarmingAction::Warm {
        ">="
    } else {
        "<"
    };
    format!(
        "{probability_text}, expected savings {} {comparison} ${:.3}",
        format_dollars(decision.expected_savings),
        CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS
    )
}

fn format_cache_warming_decision_time(next_warm_at: Option<u64>, now: u64) -> String {
    let Some(next) = next_warm_at else {
        return "Decision now".to_owned();
    };
    if next <= now {
        return "Decision now".to_owned();
    }
    let mut remaining_seconds = (next - now).div_ceil(1000);
    let hours = remaining_seconds / 3600;
    remaining_seconds %= 3600;
    let minutes = remaining_seconds / 60;
    let seconds = remaining_seconds % 60;
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    format!("Decision in {}", parts.join(" "))
}

/// One-line status for `/session`.
pub fn format_cache_warming_status(status: &CacheWarmingStatus, now: u64) -> String {
    let Some(decision) = &status.decision else {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    };
    if status.state == CacheWarmingState::Inactive
        && !decision.economics_available
        && !status.extension_override
    {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let details = if status.extension_override {
        format!(
            "extension override, {}",
            format_cache_warming_economics(decision)
        )
    } else {
        let action = match decision.action {
            CacheWarmingAction::Warm => "warm",
            CacheWarmingAction::Stop => "stop",
        };
        format!("{} -> {action}", format_cache_warming_economics(decision))
    };
    match status.state {
        CacheWarmingState::Inactive => format!("Stopped ({details})"),
        CacheWarmingState::Refreshing => format!("Warming cache ({details})"),
        CacheWarmingState::Scheduled => format!(
            "{} ({details})",
            format_cache_warming_decision_time(status.next_warm_at, now)
        ),
    }
}

/// One-line transcript text for the total cost of one or more consecutive cache warms.
pub fn format_cache_warming_usage(total_cost: f64, note: Option<&str>) -> String {
    let note_text = note.map(|n| format!(" ({n})")).unwrap_or_default();
    let formatted = format!("{total_cost:.6}");
    let trimmed = if let Some(dot) = formatted.find('.') {
        let min_len = (dot + 4).min(formatted.len());
        let mut end = formatted.len();
        while end > min_len && formatted.as_bytes()[end - 1] == b'0' {
            end -= 1;
        }
        &formatted[..end]
    } else {
        formatted.as_str()
    };
    format!("Cache warmed{note_text}: ${trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_and_usage_like_upstream() {
        let inactive = CacheWarmingStatus::default();
        assert_eq!(
            format_cache_warming_status(&inactive, 1000),
            "Inactive (waiting for first request)"
        );
        let scheduled = CacheWarmingStatus {
            state: CacheWarmingState::Scheduled,
            reason: None,
            next_warm_at: Some(66_000),
            decision: Some(CacheWarmingDecision {
                phase: CacheWarmingPhase::Idle,
                warm_cost: 0.01,
                miss_cost: 0.5,
                continuation_probability: 0.15,
                expected_savings: 0.065,
                economics_available: true,
                action: CacheWarmingAction::Warm,
            }),
            extension_override: false,
        };
        assert_eq!(
            format_cache_warming_status(&scheduled, 1_000),
            "Decision in 1m 5s (15% continuation probability, expected savings $0.065 >= $0.050 -> warm)"
        );
        assert_eq!(
            format_cache_warming_usage(0.012300, Some("extension override")),
            "Cache warmed (extension override): $0.0123"
        );
        assert_eq!(
            format_cache_warming_usage(0.1, None),
            "Cache warmed: $0.100"
        );
    }
}
