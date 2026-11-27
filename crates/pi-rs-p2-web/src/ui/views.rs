//! Multi-pane Workbench / split views panel (`p2.views`, upstream `apps/p2/plugins/views/web.tsx`).

use crate::browser_plugins::{ArrangementNode, ViewsLayoutView};
use leptos::prelude::*;

fn render_arrangement(
    node: ArrangementNode,
    on_select: Callback<(String, Option<String>)>,
    on_close_view: Callback<String>,
) -> AnyView {
    match node {
        ArrangementNode::Leaf { tabs, active } => view! {
            <div class="view-leaf">
                <div class="view-tab-strip">
                    {tabs.into_iter().map(|tid| {
                        let is_active = active.as_deref() == Some(tid.as_str());
                        let tid_sel = tid.clone();
                        let tid_cls = tid.clone();
                        view! {
                            <span class="view-tab" data-active=is_active.to_string()>
                                <button type="button" on:click=move |_| on_select.run((tid_sel.clone(), None))>{tid}</button>
                                <button type="button" aria-label="Close view" on:click=move |_| on_close_view.run(tid_cls.clone())>"×"</button>
                            </span>
                        }
                    }).collect_view()}
                </div>
            </div>
        }.into_any(),
        ArrangementNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_view = render_arrangement(*first, on_select, on_close_view);
            let second_view = render_arrangement(*second, on_select, on_close_view);
            view! {
                <div
                    class="view-split"
                    data-direction=direction
                    style=format!("--split-ratio: {ratio}%")
                >
                    <div class="view-split-first">{first_view}</div>
                    <div class="view-split-second">{second_view}</div>
                </div>
            }.into_any()
        }
    }
}

/// Interactive `p2.views` panel showing open views, split arrangement, and annotations.
#[component]
pub fn ViewsPanel(
    view_state: ViewsLayoutView,
    err: Option<String>,
    on_close: Callback<()>,
    on_open_view: Callback<()>,
    on_select_view: Callback<(String, Option<String>)>,
    on_close_view: Callback<String>,
    on_unannotate: Callback<Vec<String>>,
) -> impl IntoView {
    view! {
        <div class="sheet" role="dialog" aria-label="Views" data-testid="panel-views">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Views ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <div class="row-actions">
                <button type="button" class="button small" on:click=move |_| on_open_view.run(())>"Open Workbench View"</button>
            </div>
            {view_state.arrangement.map(|arr| render_arrangement(arr, on_select_view, on_close_view))}
            <ul class="plugin-list">
                {view_state.views.into_iter().map(|v| {
                    let id_sel = v.id.clone();
                    let id_cls = v.id.clone();
                    let target = v.target.clone();
                    view! {
                        <li class="row-actions">
                            <button type="button" on:click=move |_| on_select_view.run((id_sel.clone(), target.clone()))>
                                {format!("{} ({})", if v.title.is_empty() { &v.id } else { &v.title }, v.kind)}
                            </button>
                            <button type="button" class="button small" on:click=move |_| on_close_view.run(id_cls.clone())>"Close"</button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {(!view_state.annotations.is_empty()).then(|| view! {
                <div class="view-annotations">
                    <p class="small muted">"Annotations"</p>
                    <ul class="plugin-list">
                        {view_state.annotations.into_iter().map(|(aid, text)| {
                            let id_del = aid.clone();
                            view! {
                                <li class="row-actions">
                                    <span class="small">{text}</span>
                                    <button type="button" class="button small" on:click=move |_| on_unannotate.run(vec![id_del.clone()])>"Clear"</button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </div>
            })}
        </div>
    }
}
