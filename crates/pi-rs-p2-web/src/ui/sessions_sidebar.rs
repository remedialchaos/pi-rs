//! Rich sessions UI: `/new` session screen, `/list/:page` rollup list screen,
//! search, projects, rollups, and session actions (pin, rename, archive, clone, fork).

use crate::browser_plugins::{ProjectsView, RollupView, SearchHitView};
use leptos::prelude::*;

/// Minimal session metadata shared with `SessionsSidebar`, `/new`, and `/list/:page`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub main: u64,
    pub pinned: bool,
    pub archived: bool,
    pub badge: Option<String>,
}

/// Returns the next visible session in sidebar order, wrapping around from the last to the first.
pub fn next_cycle_session<'a>(
    shown: &'a [SessionSummary],
    active_id: Option<&str>,
) -> Option<&'a SessionSummary> {
    if shown.is_empty() {
        return None;
    }
    let idx = active_id
        .and_then(|id| shown.iter().position(|row| row.id == id))
        .map_or(0, |at| (at + 1) % shown.len());
    shown.get(idx)
}

/// `/new` screen allowing custom cwd, env, model, and title before creating a session.
#[component]
pub fn NewSessionView(
    default_cwd: Signal<String>,
    default_env: Signal<Option<String>>,
    can_create: Signal<bool>,
    on_create_with: Callback<(String, Option<String>, Option<String>)>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let cwd = RwSignal::new(String::new());
    let env_name = RwSignal::new(String::new());
    let title = RwSignal::new(String::new());

    view! {
        <section class="new-session-screen" data-testid="new-session-screen">
            <h2>"New Session"</h2>
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let chosen_cwd = {
                        let val = cwd.get_untracked().trim().to_owned();
                        if val.is_empty() { default_cwd.get_untracked() } else { val }
                    };
                    let chosen_env = {
                        let val = env_name.get_untracked().trim().to_owned();
                        if val.is_empty() { default_env.get_untracked() } else { Some(val) }
                    };
                    let chosen_title = {
                        let val = title.get_untracked().trim().to_owned();
                        (!val.is_empty()).then_some(val)
                    };
                    on_create_with.run((chosen_cwd, chosen_env, chosen_title));
                }
            >
                <label>
                    <span class="small muted">"Working directory"</span>
                    <input
                        type="text"
                        placeholder=move || default_cwd.get()
                        prop:value=move || cwd.get()
                        on:input=move |ev| cwd.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    <span class="small muted">"Environment (optional)"</span>
                    <input
                        type="text"
                        placeholder=move || default_env.get().unwrap_or_else(|| "local".into())
                        prop:value=move || env_name.get()
                        on:input=move |ev| env_name.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    <span class="small muted">"Title (optional)"</span>
                    <input
                        type="text"
                        placeholder="Session title"
                        prop:value=move || title.get()
                        on:input=move |ev| title.set(event_target_value(&ev))
                    />
                </label>
                <div class="row-actions">
                    <button type="submit" class="button" disabled=move || !can_create.get() data-testid="new-session-submit">"Create"</button>
                    <button type="button" class="button" on:click=move |_| on_cancel.run(())>"Cancel"</button>
                </div>
            </form>
        </section>
    }
}

/// `/list/:page` rollup/filter list view in the main area.
#[component]
pub fn ListPageView(
    page: Signal<String>,
    sessions: Signal<Vec<SessionSummary>>,
    rollups: Signal<Vec<RollupView>>,
    on_open_session: Callback<(String, u64)>,
    on_back: Callback<()>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    view! {
        <section class="list-page" data-testid="list-page">
            <header class="list-page-head">
                <button type="button" class="button small" on:click=move |_| on_back.run(())>"Back"</button>
                <h2>{move || format!("List: {}", page.get())}</h2>
                <input
                    type="search"
                    placeholder="Filter list..."
                    prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))
                />
            </header>
            <ul class="plugin-list">
                {move || {
                    let key = page.get();
                    let q = filter.get().to_ascii_lowercase();
                    let all = sessions.get();
                    let rollup_ids = rollups
                        .get()
                        .into_iter()
                        .find(|r| r.key == key)
                        .map(|r| r.session_ids);
                    all.into_iter()
                        .filter(|s| {
                            let in_page = match key.as_str() {
                                "pinned" => s.pinned && !s.archived,
                                "archived" => s.archived,
                                _ => rollup_ids.as_ref().is_none_or(|ids| ids.contains(&s.id)),
                            };
                            in_page && (q.is_empty() || s.title.to_ascii_lowercase().contains(&q))
                        })
                        .map(|s| {
                            let id = s.id.clone();
                            let main = s.main;
                            view! {
                                <li>
                                    <button type="button" on:click=move |_| on_open_session.run((id.clone(), main))>
                                        {s.title}
                                    </button>
                                </li>
                            }
                        })
                        .collect_view()
                }}
            </ul>
        </section>
    }
}

/// Rich sidebar session browser: search, projects, rollups, and per-session actions.
#[component]
pub fn RichSessionsControls(
    projects: Signal<ProjectsView>,
    rollups: Signal<Vec<RollupView>>,
    search_hits: Signal<Vec<SearchHitView>>,
    show_archived: RwSignal<bool>,
    on_search: Callback<(String, bool)>,
    on_open_list: Callback<String>,
    on_open_session: Callback<(String, u64)>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    view! {
        <div class="rich-sessions-controls" data-testid="rich-sessions-controls">
            <form
                class="sidebar-search-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let q = query.get_untracked();
                    if !q.trim().is_empty() {
                        on_search.run((q, show_archived.get_untracked()));
                    }
                }
            >
                <input
                    type="search"
                    placeholder="Search sessions..."
                    data-testid="sidebar-search-input"
                    prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                />
                <label class="small muted">
                    <input
                        type="checkbox"
                        prop:checked=move || show_archived.get()
                        on:change=move |_| show_archived.update(|v| *v = !*v)
                    />
                    " Archived"
                </label>
            </form>
            {move || {
                let hits = search_hits.get();
                (!hits.is_empty()).then(|| view! {
                    <ul class="sidebar-search-hits">
                        {hits.into_iter().map(|h| {
                            let sid = h.session_id.clone();
                            view! {
                                <li>
                                    <button type="button" on:click=move |_| on_open_session.run((sid.clone(), 1))>
                                        {format!("{} — {}", h.title, h.snippet)}
                                    </button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                })
            }}
            <div class="sidebar-rollups">
                <button type="button" class="button small" on:click=move |_| on_open_list.run("pinned".into())>"Pinned"</button>
                <button type="button" class="button small" on:click=move |_| on_open_list.run("archived".into())>"Archived"</button>
                {move || rollups.get().into_iter().map(|r| {
                    let key = r.key.clone();
                    view! {
                        <button type="button" class="button small" on:click=move |_| on_open_list.run(key.clone())>
                            {r.name}
                        </button>
                    }
                }).collect_view()}
            </div>
            {move || {
                let p = projects.get();
                (!p.projects.is_empty()).then(|| view! {
                    <div class="sidebar-projects small muted">
                        {p.projects.into_iter().map(|proj| view! {
                            <span class="project-pill">{proj.name}</span>
                        }).collect_view()}
                    </div>
                })
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            title: id.into(),
            main: 1,
            pinned: false,
            archived: false,
            badge: None,
        }
    }

    #[test]
    fn next_cycle_session_wraps_and_handles_missing_active() {
        assert!(next_cycle_session(&[], Some("a")).is_none());
        let list = vec![sess("a"), sess("b"), sess("c")];
        assert_eq!(
            next_cycle_session(&list, Some("a")).map(|s| s.id.as_str()),
            Some("b")
        );
        assert_eq!(
            next_cycle_session(&list, Some("c")).map(|s| s.id.as_str()),
            Some("a")
        );
        assert_eq!(
            next_cycle_session(&list, Some("missing")).map(|s| s.id.as_str()),
            Some("a")
        );
    }
}
