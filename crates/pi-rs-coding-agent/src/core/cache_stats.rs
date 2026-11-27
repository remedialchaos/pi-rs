//! Prompt cache miss detection and cumulative waste calculation.
//!
//! Port of `packages/coding-agent/src/core/cache-stats.ts`.

use chrono::DateTime;
use pi_rs_ai::{AssistantMessage, Usage};
use serde::{Deserialize, Serialize};

use super::agent_session::{ModelRegistry, SessionEntry};

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
pub const NOISE_FLOOR_TOKENS: f64 = 1024.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheMiss {
    pub missed_tokens: f64,
    pub missed_cost: f64,
    pub idle_ms: u64,
    pub model_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWasteTotals {
    pub missed_tokens: f64,
    pub missed_cost: f64,
    pub miss_count: usize,
}

pub trait ModelPriceSource {
    fn cache_read_cost_per_million(&self, provider: &str, model_id: &str) -> Option<f64>;
}

impl<T: ModelRegistry + ?Sized> ModelPriceSource for T {
    fn cache_read_cost_per_million(&self, provider: &str, model_id: &str) -> Option<f64> {
        self.get_model(provider, model_id)
            .map(|model| model.cost.rates.cache_read)
    }
}

pub struct RegistryPriceAdapter<'a>(pub &'a dyn ModelRegistry);

impl ModelPriceSource for RegistryPriceAdapter<'_> {
    fn cache_read_cost_per_million(&self, provider: &str, model_id: &str) -> Option<f64> {
        self.0
            .get_model(provider, model_id)
            .map(|model| model.cost.rates.cache_read)
    }
}

#[derive(Clone, Debug)]
struct PreviousRequest {
    prompt_tokens: f64,
    model_key: String,
    timestamp_ms: u64,
    reported_cache: bool,
}

fn detect_miss_fields(
    prev: Option<&PreviousRequest>,
    provider: &str,
    model: &str,
    usage: &Usage,
    timestamp_ms: u64,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let prev = prev?;
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    if prompt_tokens <= 0.0 || (usage.cache_read + usage.cache_write == 0.0 && !prev.reported_cache)
    {
        return None;
    }
    let missed_tokens = prev.prompt_tokens.min(prompt_tokens) - usage.cache_read;
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0.0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0.0 {
        usage.cost.cache_read / usage.cache_read
    } else {
        models
            .cache_read_cost_per_million(provider, model)
            .unwrap_or(0.0)
            / 1_000_000.0
    };
    let extra_per_token = (paid_per_token - read_per_token).max(0.0);
    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens * extra_per_token,
        idle_ms: timestamp_ms.saturating_sub(prev.timestamp_ms),
        model_changed: format!("{provider}/{model}") != prev.model_key,
    })
}

fn as_previous_request(
    provider: &str,
    model: &str,
    usage: &Usage,
    timestamp_ms: u64,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    if prompt_tokens <= 0.0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{provider}/{model}"),
        timestamp_ms,
        reported_cache: reported_cache || (usage.cache_read + usage.cache_write > 0.0),
    })
}

fn scan(
    entries: &[SessionEntry],
    models: &dyn ModelPriceSource,
) -> (
    Option<PreviousRequest>,
    CacheWasteTotals,
    Vec<(String, CacheMiss)>,
) {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();
    let mut misses = Vec::new();

    for entry in entries {
        match entry.kind.as_str() {
            "compaction" | "branch_summary" => {
                prev = None;
            }
            "usage" if entry.data["kind"].as_str() == Some("cache_warm") => {
                if let Ok(usage) = serde_json::from_value::<Usage>(entry.data["usage"].clone()) {
                    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
                    if prompt_tokens > 0.0 {
                        let provider = entry.data["provider"].as_str().unwrap_or_default();
                        let model = entry.data["model"].as_str().unwrap_or_default();
                        let ts = entry.data["timestamp"]
                            .as_str()
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
                            .unwrap_or(0);
                        prev = Some(PreviousRequest {
                            prompt_tokens,
                            model_key: format!("{provider}/{model}"),
                            timestamp_ms: ts,
                            reported_cache: true,
                        });
                    }
                }
            }
            "message" if entry.data["message"]["role"].as_str() == Some("assistant") => {
                if let Ok(message) =
                    serde_json::from_value::<AssistantMessage>(entry.data["message"].clone())
                {
                    if let Some(miss) = detect_miss_fields(
                        prev.as_ref(),
                        &message.provider,
                        &message.model,
                        &message.usage,
                        message.timestamp,
                        models,
                    ) {
                        totals.missed_tokens += miss.missed_tokens;
                        totals.missed_cost += miss.missed_cost;
                        totals.miss_count += 1;
                        misses.push((entry.id.clone(), miss));
                    }
                    if let Some(next_prev) = as_previous_request(
                        &message.provider,
                        &message.model,
                        &message.usage,
                        message.timestamp,
                        prev.as_ref().is_some_and(|p| p.reported_cache),
                    ) {
                        prev = Some(next_prev);
                    }
                }
            }
            _ => {}
        }
    }

    (prev, totals, misses)
}

pub fn compute_cache_waste(
    entries: &[SessionEntry],
    models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, models).1
}

pub fn collect_cache_misses(
    entries: &[SessionEntry],
    models: &dyn ModelPriceSource,
) -> Vec<(String, CacheMiss)> {
    scan(entries, models).2
}

pub fn detect_cache_miss(
    entries: &[SessionEntry],
    message: &AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let (prev, _, _) = scan(entries, models);
    detect_miss_fields(
        prev.as_ref(),
        &message.provider,
        &message.model,
        &message.usage,
        message.timestamp,
        models,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_ai::{AssistantRole, Cost, StopReason};
    use serde_json::json;

    struct StaticPrice(f64);
    impl ModelPriceSource for StaticPrice {
        fn cache_read_cost_per_million(&self, _provider: &str, _model_id: &str) -> Option<f64> {
            Some(self.0)
        }
    }

    #[test]
    fn detects_miss_above_noise_floor_and_resets_on_compaction() {
        let entries = vec![SessionEntry {
            id: "1".into(),
            kind: "message".into(),
            data: json!({
                "message": {
                    "role": "assistant",
                    "content": [],
                    "api": "anthropic-messages",
                    "provider": "anthropic",
                    "model": "claude-sonnet",
                    "usage": {
                        "input": 100.0,
                        "output": 20.0,
                        "cacheRead": 5000.0,
                        "cacheWrite": 0.0,
                        "totalTokens": 5120.0,
                        "cost": { "input": 0.0003, "output": 0.0003, "cacheRead": 0.0015, "cacheWrite": 0.0, "total": 0.0021 }
                    },
                    "stopReason": "stop",
                    "timestamp": 1000
                }
            }),
        }];
        let mut msg = AssistantMessage::pending(&pi_rs_ai::Model {
            base: pi_rs_ai::BaseModel {
                id: "claude-sonnet".into(),
                name: "Claude Sonnet".into(),
                api: pi_rs_ai::Api::from("anthropic-messages"),
                provider: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                input: vec![pi_rs_ai::ModelInputModality::Text],
                input_limits: None,
                cost: pi_rs_ai::ModelCost::default(),
                headers: None,
            },
            kind: None,
            reasoning: true,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 8_192,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            compat: None,
        });
        msg.role = AssistantRole::Assistant;
        msg.provider = "anthropic".into();
        msg.model = "claude-sonnet".into();
        msg.stop_reason = StopReason::Stop;
        msg.timestamp = 400_000;
        msg.usage = Usage {
            input: 4000.0,
            output: 50.0,
            cache_read: 500.0,
            cache_write: 0.0,
            total_tokens: 4550.0,
            cost: Cost {
                input: 0.012,
                output: 0.0005,
                cache_read: 0.00015,
                cache_write: 0.0,
                total: 0.01265,
                total_from_provider: None,
            },
            ..Usage::default()
        };

        let miss = detect_cache_miss(&entries, &msg, &StaticPrice(0.3)).expect("miss");
        assert_eq!(miss.missed_tokens, 4000.0);
        assert!(!miss.model_changed);
        assert_eq!(miss.idle_ms, 399_000);
        assert!(miss.missed_cost > 0.0);

        let mut compacted = entries.clone();
        compacted.push(SessionEntry {
            id: "2".into(),
            kind: "compaction".into(),
            data: json!({}),
        });
        assert!(detect_cache_miss(&compacted, &msg, &StaticPrice(0.3)).is_none());
    }
}
