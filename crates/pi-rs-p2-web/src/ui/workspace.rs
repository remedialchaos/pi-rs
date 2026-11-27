//! `MainPane::Workspace` view: Files / Changes / Search / Comments tabs, syntax-highlighted
//! file viewer, unified diff viewer, and wrap toggle (upstream `apps/p2/plugins/workspace/Workspace.tsx`).

use crate::browser_plugins::WorkspaceView;
use crate::markdown::{highlight_code, render_diff_html};
use crate::state::navigation::WorkspaceTab;
use leptos::prelude::*;

/// Renders the workspace main pane (`data-testid="workspace-pane"`).
#[component]
pub fn WorkspacePane(
    state: Signal<WorkspaceView>,
    active_tab: RwSignal<WorkspaceTab>,
    wrap_lines: RwSignal<bool>,
    on_back_to_chat: Callback<()>,
    on_open_terminal: Callback<()>,
    on_select_file: Callback<String>,
    on_toggle_wrap: Callback<bool>,
    on_refresh_changes: Callback<()>,
) -> impl IntoView {
    let search_q = RwSignal::new(String::new());

    view! {
        <div class="workspace-pane" data-testid="workspace-pane" data-wrap=move || wrap_lines.get().to_string()>
            <header class="workspace-toolbar">
                <button type="button" class="button small" data-testid="pane-btn-chat" on:click=move |_| on_back_to_chat.run(())>"Back to Chat"</button>
                <button type="button" class="button small" on:click=move |_| on_open_terminal.run(())>"Terminal"</button>
                <div class="workspace-tabs" role="tablist">
                    <button type="button" role="tab" on:click=move |_| active_tab.set(WorkspaceTab::Files)>"Files"</button>
                    <button type="button" role="tab" on:click=move |_| {
                        active_tab.set(WorkspaceTab::Changes);
                        on_refresh_changes.run(());
                    }>"Changes"</button>
                    <button type="button" role="tab" on:click=move |_| active_tab.set(WorkspaceTab::Search)>"Search"</button>
                    <button type="button" role="tab" on:click=move |_| active_tab.set(WorkspaceTab::Comments)>"Comments"</button>
                </div>
                <button
                    type="button"
                    class="button small"
                    on:click=move |_| {
                        let next = !wrap_lines.get_untracked();
                        wrap_lines.set(next);
                        on_toggle_wrap.run(next);
                    }
                >
                    {move || if wrap_lines.get() { "No Wrap" } else { "Wrap" }}
                </button>
            </header>
            <div class="workspace-body">
                {move || {
                    let ws = state.get();
                    match active_tab.get() {
                        WorkspaceTab::Changes => view! {
                            <ul class="plugin-list" data-testid="workspace-changes">
                                {ws.changes.into_iter().map(|c| {
                                    let p = c.path.clone();
                                    view! {
                                        <li>
                                            <button type="button" on:click=move |_| on_select_file.run(p.clone())>
                                                {format!("[{}] {}", c.status, c.path)}
                                            </button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        }.into_any(),
                        WorkspaceTab::Search => view! {
                            <div class="workspace-search">
                                <input
                                    type="search"
                                    placeholder="Filter workspace files..."
                                    prop:value=move || search_q.get()
                                    on:input=move |ev| search_q.set(event_target_value(&ev))
                                />
                                <ul class="plugin-list">
                                    {move || {
                                        let q = search_q.get().to_ascii_lowercase();
                                        ws.files
                                            .iter()
                                            .filter(|(p, _)| q.is_empty() || p.to_ascii_lowercase().contains(&q))
                                            .map(|(p, is_dir)| {
                                                let path = p.clone();
                                                let label = if *is_dir { format!("{p}/") } else { p.clone() };
                                                view! {
                                                    <li>
                                                        <button type="button" on:click=move |_| on_select_file.run(path.clone())>{label}</button>
                                                    </li>
                                                }
                                            })
                                            .collect_view()
                                    }}
                                </ul>
                            </div>
                        }.into_any(),
                        WorkspaceTab::Files | WorkspaceTab::Comments | WorkspaceTab::Terminal => view! {
                            <ul class="plugin-list" data-testid="workspace-files">
                                {ws.files.into_iter().map(|(p, is_dir)| {
                                    let path = p.clone();
                                    let label = if is_dir { format!("{p}/") } else { p.clone() };
                                    view! {
                                        <li>
                                            <button type="button" on:click=move |_| on_select_file.run(path.clone())>{label}</button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        }.into_any(),
                    }
                }}
                {move || {
                    let ws = state.get();
                    ws.selected_path.map(|path| {
                        let ext = path.rsplit('.').next();
                        let content = ws.selected_content.unwrap_or_default();
                        if let Some(before) = ws.selected_before {
                            let diff_html = render_diff_html(&before, &content);
                            view! {
                                <div class="workspace-preview" data-testid="workspace-diff">
                                    <div class="small muted">{path}</div>
                                    <div inner_html=diff_html></div>
                                </div>
                            }.into_any()
                        } else {
                            let code_html = highlight_code(&content, ext);
                            view! {
                                <div class="workspace-preview" data-testid="workspace-file-preview">
                                    <div class="small muted">{path}</div>
                                    <pre class="code-block"><code inner_html=code_html></code></pre>
                                </div>
                            }.into_any()
                        }
                    })
                }}
            </div>
        </div>
    }
}
