//! Session usage footer: cost, cache hit rate, context bar, and viewer count (upstream `SessionStats.tsx`).

use crate::browser_plugins::SessionMetricsView;
use crate::cost::precise_cost;
use leptos::prelude::*;

/// Displays live session metrics (`p2.session-metrics`) with clickable Cost & Sharing triggers.
#[component]
pub fn SessionStats(
    metrics: Signal<SessionMetricsView>,
    on_cost: Callback<()>,
    on_clients: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="session-usage" data-testid="session-stats">
            <div class="session-stats">
                {move || {
                    let m = metrics.get();
                    let chr = m
                        .cache_hit_rate
                        .map(|r| format!("chr {}%", (r * 100.0).round() as u64))
                        .unwrap_or_else(|| "chr —".to_owned());
                    let cost_text = precise_cost(m.total_cost);
                    let context_pct = match (m.context_used, m.context_limit) {
                        (Some(used), Some(limit)) if limit > 0 => {
                            Some(((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0).round() as u64)
                        }
                        _ => None,
                    };
                    view! {
                        <button
                            type="button"
                            class="stat-button"
                            data-testid="stats-cost-btn"
                            title="Show cost details"
                            on:click=move |_| on_cost.run(())
                        >
                            {cost_text}
                        </button>
                        <span class="usage" title="Cache hit rate">{chr}</span>
                        {context_pct.map(|pct| view! {
                            <span class="context-usage" title="Context window usage">
                                {format!("ctx {pct}%")}
                            </span>
                        })}
                        <button
                            type="button"
                            class="stat-button"
                            data-testid="stats-clients-btn"
                            title="Connected viewers"
                            on:click=move |_| on_clients.run(())
                        >
                            {format!("{} viewers", m.clients)}
                        </button>
                    }
                }}
            </div>
        </div>
    }
}
