use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::model::{Api, CacheRetention, Model, ModelCost};
use crate::request::{StreamOptions, provider_env_value};
use crate::types::Usage;

pub const CACHE_WARMING_MAX_AGE_MS: u64 = 60 * 60_000;
pub const CACHE_WARMING_MAX_IDLE_AGE_MS: u64 = 30 * 60_000;
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
pub const CACHE_WARMING_IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingAction {
    Warm,
    Stop,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingPhase {
    Streaming,
    Idle,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingDecision {
    pub phase: CacheWarmingPhase,
    pub warm_cost: f64,
    pub miss_cost: f64,
    pub continuation_probability: f64,
    pub expected_savings: f64,
    pub economics_available: bool,
    pub action: CacheWarmingAction,
}

pub fn get_cache_warming_delay_ms(ttl_ms: f64) -> Option<f64> {
    if ttl_ms <= 10_000.0 {
        None
    } else if ttl_ms.is_nan() {
        Some(f64::NAN)
    } else {
        Some((ttl_ms * 0.9).min(ttl_ms - 10_000.0).floor().max(1.0))
    }
}

pub fn get_prompt_cache_ttl_ms(model: &Model, options: Option<&StreamOptions>) -> Option<f64> {
    let retention = options.and_then(|o| o.cache_retention).unwrap_or_else(|| {
        let value = match options {
            Some(o) => provider_env_value("PI_CACHE_RETENTION", &o.request.env),
            None => std::env::var("PI_CACHE_RETENTION")
                .ok()
                .map(std::borrow::Cow::Owned),
        };
        if value.as_deref() == Some("long") {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    });
    let tiers = model.prompt_cache.as_ref()?;
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => tiers.short,
        CacheRetention::Long => tiers.long,
    }
    .map(|seconds| seconds * 1000.0)
}

pub fn is_cache_warming_replayable(model: &Model, options: Option<&StreamOptions>) -> bool {
    !options.is_some_and(|o| o.reasoning.is_some())
        || model.api != Api::AnthropicMessages
        || model.compat("forceAdaptiveThinking").as_bool() == Some(true)
}

pub fn get_cache_warming_request_options(
    mut options: StreamOptions,
    signal: CancellationToken,
) -> StreamOptions {
    options.max_tokens = Some(1);
    options.disable_continuation = true;
    options.request.max_retries = Some(0);
    options.request.signal = signal;
    options
}

pub fn evaluate_cache_warming(
    model: &Model,
    prompt_tokens: f64,
    phase: CacheWarmingPhase,
) -> CacheWarmingDecision {
    fn price(model: &ModelCost, input: f64, output: f64, cache_read: f64, cache_write: f64) -> f64 {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            ..Usage::default()
        }
        .calculate_cost(model)
        .total
    }
    let hit = price(&model.cost, 0.0, 0.0, prompt_tokens, 0.0);
    let miss = if model.cost.rates.cache_write > 0.0 {
        price(&model.cost, 0.0, 0.0, 0.0, prompt_tokens)
    } else {
        price(&model.cost, prompt_tokens, 0.0, 0.0, 0.0)
    };
    let warm_cost = price(&model.cost, 0.0, 1.0, prompt_tokens, 0.0);
    let extra = miss - hit;
    let miss_cost = if extra.is_nan() {
        extra
    } else {
        extra.max(0.0)
    };
    let continuation_probability = if phase == CacheWarmingPhase::Idle {
        CACHE_WARMING_IDLE_CONTINUATION_PROBABILITY
    } else {
        1.0
    };
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    CacheWarmingDecision {
        phase,
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available: prompt_tokens > 0.0 && (hit > 0.0 || miss > 0.0),
        action: if expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
            CacheWarmingAction::Warm
        } else {
            CacheWarmingAction::Stop
        },
    }
}
