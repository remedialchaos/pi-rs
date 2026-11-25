//! Cost sheet UI (upstream `CostPanel.tsx`).

use crate::cost::{
    AssistantCost, axis_cost, cache_bar_height, cost_bar_height, percent, precise_cost,
};
use leptos::prelude::*;

/// The cost sheet: summary, two shared-axis charts and a table of responses.
#[component]
pub fn CostPanel(costs: Vec<AssistantCost>, on_close: Callback<()>) -> impl IntoView {
    let max = crate::cost::maximum(&costs);
    let total = precise_cost(crate::cost::total(&costs));
    let count = costs.len();
    let rows = costs.clone();
    let chart_rows = costs.clone();
    view! {
        <div class="sheet cost-sheet" role="dialog" aria-label="Cost">
            <button type="button" class="sheet-head" on:click=move |_| on_close.run(()) aria-label="Close">
                "Cost"
            </button>
            <p class="muted small">
                "Viewed conversation · committed assistant messages. The session footer can also include subagents and tools."
            </p>
            {(count > 0).then(|| view! {
                <div class="cost-summary">
                    <span>{format!("{count} responses")}</span>
                    <strong>{total.clone()}</strong>
                </div>
            })}
            <figure class="cost-chart" aria-label="Assistant response costs in US dollars and cache hit rates">
                <figcaption>"Cost (USD)"</figcaption>
                <div class="cost-y-axis" aria-hidden="true">
                    <span>{axis_cost(max)}</span>
                    <span>{axis_cost(max / 2.0)}</span>
                    <span>"$0"</span>
                </div>
                <div class="cost-plot">
                    <div class="cost-bars">
                        <For
                            each=move || chart_rows.clone()
                            key=|entry| entry.entry_id.clone()
                            children={move |entry: AssistantCost| {
                                let height = cost_bar_height(entry.cost, max);
                                let cache = entry.cache_hit.map(cache_bar_height);
                                view! {
                                    <div class="cost-column" title=format!("{} · cache hit rate {}", precise_cost(entry.cost), percent(entry.cache_hit))>
                                        <span class="cost-bar-track"><span class="cost-bar" style=format!("height: {height}%")></span></span>
                                        {cache.map(|height| view! {
                                            <span class="cost-bar-track cache-bar-track">
                                                <span class="cost-bar" style=format!("height: {height}%")></span>
                                            </span>
                                        })}
                                    </div>
                                }
                            }}
                        />
                    </div>
                </div>
            </figure>
            <div class="cost-table-wrap">
                <table class="cost-table">
                    <thead><tr><th>"#"</th><th>"Model"</th><th>"Result"</th><th>"Cache"</th><th>"Cost"</th></tr></thead>
                    <tbody>
                        <For
                            each=move || rows.clone()
                            key=|entry| entry.entry_id.clone()
                            children={move |entry: AssistantCost| {
                                view! {
                                    <tr>
                                        <td>{entry.entry_id.clone()}</td>
                                        <td>{entry.model.clone()}</td>
                                        <td>{entry.stop_reason.clone()}</td>
                                        <td>{percent(entry.cache_hit)}</td>
                                        <td>{precise_cost(entry.cost)}</td>
                                    </tr>
                                }
                            }}
                        />
                    </tbody>
                </table>
            </div>
        </div>
    }
}
