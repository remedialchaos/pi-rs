//! Usage aggregation and per-model cost breakdown helpers.
//!
//! Port of `packages/coding-agent/src/core/usage-totals.ts`.

use indexmap::IndexMap;
use pi_rs_ai::{Cost, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agent_session::SessionEntry as SessionApiEntry;
use super::session_manager::SessionEntry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost: f64,
}

pub fn create_usage_totals() -> UsageTotals {
    UsageTotals::default()
}

pub fn add_usage_to_totals(totals: &mut UsageTotals, usage: &Usage) {
    totals.input += usage.input;
    totals.output += usage.output;
    totals.cache_read += usage.cache_read;
    totals.cache_write += usage.cache_write;
    totals.cost += usage.cost.total;
}

/// Sum of two usages, keeping the optional token splits when either side reports them.
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let cache_write_1h = match (first.cache_write_1h, second.cache_write_1h) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    let reasoning = match (first.reasoning, second.reasoning) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    let total_from_provider = (first.cost.total_from_provider == Some(true)
        && second.cost.total_from_provider == Some(true))
    .then_some(true)
    .or(Some(false));

    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h,
        reasoning,
        total_tokens: first.total_tokens + second.total_tokens,
        cost: Cost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
            total_from_provider,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: f64,
}

fn add_value_to_totals(totals: &mut UsageTotals, usage: &Value) {
    if let Ok(parsed) = serde_json::from_value::<Usage>(usage.clone()) {
        add_usage_to_totals(totals, &parsed);
        return;
    }
    totals.input += usage["input"].as_f64().unwrap_or(0.0);
    totals.output += usage["output"].as_f64().unwrap_or(0.0);
    totals.cache_read += usage["cacheRead"].as_f64().unwrap_or(0.0);
    totals.cache_write += usage["cacheWrite"].as_f64().unwrap_or(0.0);
    totals.cost += usage["cost"]["total"].as_f64().unwrap_or(0.0);
}

fn finalize_breakdown(
    totals_by_key: IndexMap<String, UsageTotals>,
) -> Vec<UsageCostBreakdownEntry> {
    let mut result = totals_by_key
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            key,
            cost: totals.cost,
            tokens: totals.input + totals.output + totals.cache_read + totals.cache_write,
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0.0)
        .collect::<Vec<_>>();
    result.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    result
}

/// Group model-attributed usage by model and all other usage into `"Tools/summaries"`.
pub fn get_usage_cost_breakdown(entries: &[SessionEntry]) -> Vec<UsageCostBreakdownEntry> {
    let mut totals_by_key: IndexMap<String, UsageTotals> = IndexMap::new();
    for entry in entries {
        let extracted: Option<(String, &Value)> = match entry {
            SessionEntry::Message { message, .. } => match message["role"].as_str() {
                Some("assistant") => {
                    let provider = message["provider"].as_str().unwrap_or_default();
                    let model = message["responseModel"]
                        .as_str()
                        .or_else(|| message["model"].as_str())
                        .unwrap_or_default();
                    message
                        .get("usage")
                        .map(|usage| (format!("{provider}/{model}"), usage))
                }
                Some("toolResult") => message
                    .get("usage")
                    .map(|usage| ("Tools/summaries".to_owned(), usage)),
                _ => None,
            },
            SessionEntry::Usage {
                provider,
                model,
                usage,
                ..
            } => Some((format!("{provider}/{model}"), usage)),
            SessionEntry::BranchSummary { usage, .. } | SessionEntry::Compaction { usage, .. } => {
                usage
                    .as_ref()
                    .map(|usage| ("Tools/summaries".to_owned(), usage))
            }
            _ => None,
        };
        if let Some((key, usage)) = extracted {
            let totals = totals_by_key.entry(key).or_default();
            add_value_to_totals(totals, usage);
        }
    }
    finalize_breakdown(totals_by_key)
}

/// Same as [`get_usage_cost_breakdown`] over serialized `agent_session::SessionEntry` records.
pub fn get_usage_cost_breakdown_from_entries(
    entries: &[SessionApiEntry],
) -> Vec<UsageCostBreakdownEntry> {
    let mut totals_by_key: IndexMap<String, UsageTotals> = IndexMap::new();
    for entry in entries {
        let data = &entry.data;
        let extracted: Option<(String, &Value)> = match entry.kind.as_str() {
            "message" => {
                let message = &data["message"];
                match message["role"].as_str() {
                    Some("assistant") => {
                        let provider = message["provider"].as_str().unwrap_or_default();
                        let model = message["responseModel"]
                            .as_str()
                            .or_else(|| message["model"].as_str())
                            .unwrap_or_default();
                        message
                            .get("usage")
                            .filter(|u| !u.is_null())
                            .map(|usage| (format!("{provider}/{model}"), usage))
                    }
                    Some("toolResult") => message
                        .get("usage")
                        .filter(|u| !u.is_null())
                        .map(|usage| ("Tools/summaries".to_owned(), usage)),
                    _ => None,
                }
            }
            "usage" => {
                let provider = data["provider"].as_str().unwrap_or_default();
                let model = data["model"].as_str().unwrap_or_default();
                data.get("usage")
                    .filter(|u| !u.is_null())
                    .map(|usage| (format!("{provider}/{model}"), usage))
            }
            "branch_summary" | "compaction" => data
                .get("usage")
                .filter(|u| !u.is_null())
                .map(|usage| ("Tools/summaries".to_owned(), usage)),
            _ => None,
        };
        if let Some((key, usage)) = extracted {
            let totals = totals_by_key.entry(key).or_default();
            add_value_to_totals(totals, usage);
        }
    }
    finalize_breakdown(totals_by_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn combine_usage_preserves_optional_splits_and_provider_flag() {
        let a = Usage {
            input: 10.0,
            cache_write_1h: Some(5.0),
            cost: Cost {
                total: 1.0,
                total_from_provider: Some(true),
                ..Cost::default()
            },
            ..Usage::default()
        };

        let mut b = Usage {
            input: 20.0,
            reasoning: Some(3.0),
            cost: Cost {
                total: 2.0,
                total_from_provider: Some(true),
                ..Cost::default()
            },
            ..Usage::default()
        };

        let sum = combine_usage(&a, &b);
        assert_eq!(sum.input, 30.0);
        assert_eq!(sum.cache_write_1h, Some(5.0));
        assert_eq!(sum.reasoning, Some(3.0));
        assert_eq!(sum.cost.total, 3.0);
        assert_eq!(sum.cost.total_from_provider, Some(true));

        b.cost.total_from_provider = Some(false);
        assert_eq!(combine_usage(&a, &b).cost.total_from_provider, Some(false));
    }

    #[test]
    fn cost_breakdown_groups_tools_and_sorts_descending() {
        let entries = vec![
            SessionApiEntry {
                id: "1".into(),
                kind: "message".into(),
                data: json!({
                    "message": {
                        "role": "assistant",
                        "provider": "anthropic",
                        "model": "claude-sonnet",
                        "usage": { "input": 100, "output": 50, "cacheRead": 0, "cacheWrite": 0, "cost": { "total": 0.2 } }
                    }
                }),
            },
            SessionApiEntry {
                id: "2".into(),
                kind: "message".into(),
                data: json!({
                    "message": {
                        "role": "toolResult",
                        "usage": { "input": 10, "output": 10, "cacheRead": 0, "cacheWrite": 0, "cost": { "total": 0.5 } }
                    }
                }),
            },
            SessionApiEntry {
                id: "3".into(),
                kind: "compaction".into(),
                data: json!({
                    "usage": { "input": 5, "output": 5, "cacheRead": 0, "cacheWrite": 0, "cost": { "total": 0.1 } }
                }),
            },
        ];
        let breakdown = get_usage_cost_breakdown_from_entries(&entries);
        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[0].key, "Tools/summaries");
        assert!((breakdown[0].cost - 0.6).abs() < 1e-9);
        assert_eq!(breakdown[0].tokens, 30.0);
        assert_eq!(breakdown[1].key, "anthropic/claude-sonnet");
    }
}
