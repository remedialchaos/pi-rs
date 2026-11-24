//! Cold-cache compaction helpers for the Familiar.

use pi_rs_durable::harness::ContextView;
use serde_json::Value;

/// Context size from which a cold Familiar compacts, unless the model's regular cutoff is lower.
pub const COLD_COMPACT_TOKENS: u64 = 150_000;

/// The context size from which a cold Familiar compacts: 150k, or the regular compaction cutoff when lower.
pub fn cold_threshold(context_window: u64, reserve_tokens: u64) -> u64 {
    COLD_COMPACT_TOKENS.min(context_window.saturating_sub(reserve_tokens))
}

/// When the prompt cache of an answer given at `answered_at` is expected to be gone.
pub fn cold_at(answered_at: u64, ttl_ms: u64, warming_ms: u64) -> u64 {
    answered_at
        .saturating_add(warming_ms)
        .saturating_add(ttl_ms)
}

/// The newest answer after the context's head (a compaction or reset), with the context size it reported;
/// `None` when there is none (e.g. right after a compaction).
pub fn last_answer(view: &ContextView) -> Option<(Value, u64)> {
    let after = view.head.as_ref().map(|head| head.id);
    for (entry, contributions) in view.entries.iter().zip(view.contributions.iter()).rev() {
        if after.is_some_and(|head_id| entry.id <= head_id) {
            return None;
        }
        for message in contributions.iter().rev() {
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && let Some(usage) = message.get("usage")
            {
                let tokens = context_tokens(usage);
                if tokens > 0 {
                    return Some((message.clone(), tokens));
                }
            }
        }
    }
    None
}

fn context_tokens(usage: &Value) -> u64 {
    let total = usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if total > 0 {
        return total;
    }
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    field("input") + field("output") + field("cacheRead") + field("cacheWrite")
}
