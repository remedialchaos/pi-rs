//! Owner-only live notice lane (upstream `OwnerNotices.tsx`).

use crate::browser_plugins::LogNoticeView;
use crate::state::navigation::is_dev_success_result;
use leptos::prelude::*;
use std::collections::HashSet;

/// Displays owner-only server/worker/dev notices below the header with Dismiss & Show logs actions.
#[component]
pub fn OwnerNotices(
    notices: Signal<Vec<LogNoticeView>>,
    dismissed: RwSignal<HashSet<String>>,
    on_dismiss: Callback<String>,
    on_show_logs: Callback<String>,
) -> impl IntoView {
    let visible = move || {
        let hidden = dismissed.get();
        notices
            .get()
            .into_iter()
            .filter(|n| !hidden.contains(&n.id))
            .collect::<Vec<_>>()
    };

    view! {
        {move || {
            let items = visible();
            (!items.is_empty()).then(|| view! {
                <aside class="owner-notices" aria-label="Owner notices" data-testid="owner-notices">
                    {items.into_iter().map(|notice| {
                        let tone = if is_dev_success_result(&notice.message) {
                            "success"
                        } else {
                            "error"
                        };
                        let id_for_dismiss = notice.id.clone();
                        let src_for_logs = notice.source.clone();
                        view! {
                            <section class=format!("owner-notice {tone}") aria-label="Owner notification">
                                <div class="owner-notice-message" role="alert">
                                    <span class="owner-notice-summary">
                                        {format!("{}: {}", notice.source, notice.message)}
                                    </span>
                                </div>
                                <div class="owner-notice-actions">
                                    <button
                                        type="button"
                                        class="button"
                                        on:click=move |_| on_show_logs.run(src_for_logs.clone())
                                    >"Show logs"</button>
                                    <button
                                        type="button"
                                        class="button"
                                        aria-label="Dismiss owner notice"
                                        on:click=move |_| on_dismiss.run(id_for_dismiss.clone())
                                    >"Dismiss"</button>
                                </div>
                            </section>
                        }
                    }).collect_view()}
                </aside>
            })
        }}
    }
}
