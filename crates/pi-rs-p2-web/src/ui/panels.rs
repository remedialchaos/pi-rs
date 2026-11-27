//! Interactive side panels for Wave 1.6b (`Settings`, `Models`, `Mcp`, `Sharing`, `Tree`, `Artifacts`).

use crate::browser_plugins::{
    ArtifactsView, McpServersView, ModelsView, SettingsView, SharingView, TreeView,
};
use crate::markdown::{render_diff_html, render_html};
use leptos::prelude::*;

/// Interactive Settings panel (`p2.settings` `update` mutations).
#[component]
pub fn SettingsPanel(
    view_state: SettingsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_update: Callback<(String, String)>,
    on_unset: Callback<String>,
) -> impl IntoView {
    let new_key = RwSignal::new(String::new());
    let new_val = RwSignal::new(String::new());

    view! {
        <div class="sheet" role="dialog" aria-label="Settings" data-testid="panel-settings">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Settings ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Status: {e}")}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let k = new_key.get_untracked().trim().to_owned();
                    let v = new_val.get_untracked();
                    if !k.is_empty() {
                        on_update.run((k, v));
                        new_key.set(String::new());
                        new_val.set(String::new());
                    }
                }
            >
                <input
                    type="text"
                    placeholder="Setting key (e.g. detail)"
                    data-testid="settings-key-input"
                    prop:value=move || new_key.get()
                    on:input=move |ev| new_key.set(event_target_value(&ev))
                />
                <input
                    type="text"
                    placeholder="JSON or string value"
                    data-testid="settings-val-input"
                    prop:value=move || new_val.get()
                    on:input=move |ev| new_val.set(event_target_value(&ev))
                />
                <button type="submit" class="button small" data-testid="settings-save-btn">"Save"</button>
            </form>
            <dl class="settings-list">
                {view_state.entries.into_iter().map(|(k, v)| {
                    let key_edit = k.clone();
                    let key_del = k.clone();
                    let val_sig = RwSignal::new(v);
                    view! {
                        <div class="settings-row">
                            <dt>{k}</dt>
                            <dd class="settings-edit-row">
                                <input
                                    type="text"
                                    prop:value=move || val_sig.get()
                                    on:input=move |ev| val_sig.set(event_target_value(&ev))
                                />
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| on_update.run((key_edit.clone(), val_sig.get_untracked()))
                                >"Apply"</button>
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| on_unset.run(key_del.clone())
                                >"Reset"</button>
                            </dd>
                        </div>
                    }
                }).collect_view()}
            </dl>
        </div>
    }
}

/// Interactive Models panel (`setDefault`, per-session `p2.chat.configure`, `setEnabledModels`).
#[component]
pub fn ModelsPanel(
    view_state: ModelsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_set_default: Callback<(String, String, String)>,
    on_set_session_model: Callback<(String, String, String)>,
    on_set_enabled: Callback<Vec<String>>,
) -> impl IntoView {
    let default_thinking = view_state
        .default_thinking
        .clone()
        .unwrap_or_else(|| "medium".to_owned());
    let thinking = RwSignal::new(default_thinking);
    let enabled_set = RwSignal::new(view_state.enabled_models.clone());

    view! {
        <div class="sheet" role="dialog" aria-label="Models" data-testid="panel-models">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Models ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            {view_state.default_model_id.as_ref().map(|m| view! {
                <p class="muted small">{format!("Default: {m}")}</p>
            })}
            <label class="panel-inline-field">
                <span class="small muted">"Thinking:"</span>
                <select
                    data-testid="models-thinking-select"
                    prop:value=move || thinking.get()
                    on:change=move |ev| thinking.set(event_target_value(&ev))
                >
                    <option value="off">"off"</option>
                    <option value="minimal">"minimal"</option>
                    <option value="low">"low"</option>
                    <option value="medium">"medium"</option>
                    <option value="high">"high"</option>
                    <option value="xhigh">"xhigh"</option>
                </select>
            </label>
            <ul class="plugin-list">
                {view_state.models.into_iter().map(|m| {
                    let full_id = format!("{}/{}", m.provider, m.model_id);
                    let full_toggle = full_id.clone();
                    let prov_def = m.provider.clone();
                    let mid_def = m.model_id.clone();
                    let prov_sess = m.provider.clone();
                    let mid_sess = m.model_id.clone();
                    let is_enabled = move || {
                        let list = enabled_set.get();
                        list.is_empty() || list.iter().any(|p| p == &full_toggle || p == "*")
                    };
                    view! {
                        <li class="model-item">
                            <label class="model-toggle">
                                <input
                                    type="checkbox"
                                    prop:checked=is_enabled
                                    on:change=move |_| {
                                        let mut next = enabled_set.get_untracked();
                                        let id = format!("{}/{}", m.provider, m.model_id);
                                        if let Some(pos) = next.iter().position(|x| x == &id) {
                                            next.remove(pos);
                                        } else {
                                            next.push(id);
                                        }
                                        enabled_set.set(next.clone());
                                        on_set_enabled.run(next);
                                    }
                                />
                                <span>{full_id}</span>
                            </label>
                            <div class="model-actions">
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| on_set_default.run((prov_def.clone(), mid_def.clone(), thinking.get_untracked()))
                                >"Default"</button>
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| on_set_session_model.run((prov_sess.clone(), mid_sess.clone(), thinking.get_untracked()))
                                >"Use here"</button>
                            </div>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

/// Interactive MCP Servers panel (`signIn`, `paste`, `cancelSignIn`, `signOut`, `restart`).
#[component]
pub fn McpPanel(
    view_state: McpServersView,
    err: Option<String>,
    on_close: Callback<()>,
    on_restart: Callback<String>,
    on_sign_in: Callback<String>,
    on_sign_out: Callback<String>,
    on_paste_redirect: Callback<(String, String)>,
    on_cancel_sign_in: Callback<String>,
) -> impl IntoView {
    let redirect_url = RwSignal::new(String::new());

    view! {
        <div class="sheet" role="dialog" aria-label="MCP Servers" data-testid="panel-mcp">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"MCP Servers ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            {(!view_state.sign_ins.is_empty()).then(|| view! {
                <div class="mcp-sign-ins">
                    <p class="small muted">"Pending OAuth sign-ins:"</p>
                    <input
                        type="url"
                        placeholder="Paste redirect URL..."
                        data-testid="mcp-redirect-input"
                        prop:value=move || redirect_url.get()
                        on:input=move |ev| redirect_url.set(event_target_value(&ev))
                    />
                    {view_state.sign_ins.into_iter().map(|sid| {
                        let sid_paste = sid.clone();
                        let sid_cancel = sid.clone();
                        view! {
                            <div class="mcp-signin-row">
                                <span>{sid}</span>
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| {
                                        let url = redirect_url.get_untracked();
                                        if !url.trim().is_empty() {
                                            on_paste_redirect.run((sid_paste.clone(), url));
                                            redirect_url.set(String::new());
                                        }
                                    }
                                >"Submit URL"</button>
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| on_cancel_sign_in.run(sid_cancel.clone())
                                >"Cancel"</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            })}
            <ul class="plugin-list">
                {view_state.instances.into_iter().map(|inst| {
                    let id_restart = inst.id.clone();
                    let srv_in = inst.server.clone();
                    let srv_out = inst.server.clone();
                    view! {
                        <li class="mcp-row">
                            <span>{format!("{} [{}] ({} tools)", inst.server, inst.status, inst.tool_count)}</span>
                            <div class="mcp-actions">
                                <button type="button" class="button small" on:click=move |_| on_restart.run(id_restart.clone())>"Restart"</button>
                                <button type="button" class="button small" on:click=move |_| on_sign_in.run(srv_in.clone())>"Sign In"</button>
                                <button type="button" class="button small" on:click=move |_| on_sign_out.run(srv_out.clone())>"Sign Out"</button>
                            </div>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

/// Interactive Sharing panel (`link`, `share`, `remove`).
#[component]
pub fn SharingPanel(
    view_state: SharingView,
    err: Option<String>,
    on_close: Callback<()>,
    on_create_link: Callback<(String, bool)>,
    on_share_email: Callback<(String, String)>,
    on_remove_guest: Callback<String>,
) -> impl IntoView {
    let email = RwSignal::new(String::new());
    let mode = RwSignal::new("view".to_owned());

    view! {
        <div class="sheet" role="dialog" aria-label="Sharing" data-testid="panel-sharing">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Sharing ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <div class="sharing-actions">
                <button
                    type="button"
                    class="button small"
                    data-testid="share-link-view-btn"
                    on:click=move |_| on_create_link.run(("view".into(), false))
                >"Create View Link"</button>
                <button
                    type="button"
                    class="button small"
                    data-testid="share-link-full-btn"
                    on:click=move |_| on_create_link.run(("full".into(), false))
                >"Create Full Link"</button>
            </div>
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let addr = email.get_untracked().trim().to_owned();
                    if !addr.is_empty() {
                        on_share_email.run((addr, mode.get_untracked()));
                        email.set(String::new());
                    }
                }
            >
                <input
                    type="email"
                    placeholder="Invite by email"
                    data-testid="share-email-input"
                    prop:value=move || email.get()
                    on:input=move |ev| email.set(event_target_value(&ev))
                />
                <select prop:value=move || mode.get() on:change=move |ev| mode.set(event_target_value(&ev))>
                    <option value="view">"view"</option>
                    <option value="full">"full"</option>
                </select>
                <button type="submit" class="button small">"Share"</button>
            </form>
            <ul class="plugin-list">
                {view_state.links.into_iter().map(|l| view! {
                    <li>{format!("{}: {}", l.mode, l.url)}</li>
                }).collect_view()}
                {view_state.guests.into_iter().map(|g| {
                    let gid = g.id.clone();
                    view! {
                        <li class="guest-row">
                            <span>{format!("{} ({})", g.name, g.mode)}</span>
                            <button
                                type="button"
                                class="button small"
                                on:click=move |_| on_remove_guest.run(gid.clone())
                            >"Remove"</button>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

/// Interactive Session Tree panel (`select`, `navigate`, `branch`, `rename`).
#[component]
pub fn TreePanel(
    view_state: TreeView,
    err: Option<String>,
    on_close: Callback<()>,
    on_select_branch: Callback<u64>,
    on_navigate: Callback<u64>,
    on_branch: Callback<u64>,
    on_rename_branch: Callback<(u64, String)>,
) -> impl IntoView {
    let rename_label = RwSignal::new(String::new());
    let current = view_state.current;

    view! {
        <div class="sheet" role="dialog" aria-label="Session Tree" data-testid="panel-tree">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Session Tree ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <div class="tree-rename-row">
                <input
                    type="text"
                    placeholder="Rename active branch..."
                    prop:value=move || rename_label.get()
                    on:input=move |ev| rename_label.set(event_target_value(&ev))
                />
                {current.map(|conv| view! {
                    <button
                        type="button"
                        class="button small"
                        on:click=move |_| {
                            let label = rename_label.get_untracked().trim().to_owned();
                            if !label.is_empty() {
                                on_rename_branch.run((conv, label));
                                rename_label.set(String::new());
                            }
                        }
                    >"Rename"</button>
                })}
            </div>
            <ul class="plugin-list">
                {view_state.branches.into_iter().map(|b| {
                    let conv = b.conversation;
                    let is_current = current == Some(conv);
                    let label = b.label.unwrap_or_else(|| format!("Branch {conv}"));
                    view! {
                        <li class="tree-branch-row" data-current=is_current.to_string()>
                            <button type="button" on:click=move |_| on_select_branch.run(conv)>
                                {if is_current { format!("● {label}") } else { label }}
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {(!view_state.nodes.is_empty()).then(|| view! {
                <div class="tree-nodes">
                    <p class="small muted">"Entries"</p>
                    <ul class="plugin-list">
                        {view_state.nodes.into_iter().map(|n| {
                            let entry = n.entry;
                            let preview: String = n.text.chars().take(60).collect();
                            view! {
                                <li class="tree-node-row">
                                    <span class="small">{format!("#{} [{}] {}", entry, n.role, preview)}</span>
                                    <button type="button" class="button small" on:click=move |_| on_navigate.run(entry)>"Jump"</button>
                                    <button type="button" class="button small" on:click=move |_| on_branch.run(entry)>"Branch"</button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </div>
            })}
        </div>
    }
}

/// Interactive Artifacts panel with sandboxed `/artifact-frame` iframe, markdown view, diff toggle, and delete.
#[component]
pub fn ArtifactsPanel(
    view_state: ArtifactsView,
    session_id: Option<String>,
    err: Option<String>,
    on_close: Callback<()>,
    on_read: Callback<String>,
    on_remove: Callback<String>,
) -> impl IntoView {
    let show_diff = RwSignal::new(false);
    let selected_kind = view_state
        .selected_id
        .as_deref()
        .and_then(|id| view_state.items.iter().find(|i| i.id == id))
        .map(|i| i.kind.clone());
    let selected_version = view_state
        .selected_id
        .as_deref()
        .and_then(|id| view_state.items.iter().find(|i| i.id == id))
        .map(|i| i.version)
        .unwrap_or(1);

    view! {
        <div class="sheet" role="dialog" aria-label="Artifacts" data-testid="panel-artifacts">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Artifacts ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <ul class="plugin-list">
                {view_state.items.into_iter().map(|item| {
                    let id_read = item.id.clone();
                    let id_del = item.id.clone();
                    view! {
                        <li class="artifact-row">
                            <button type="button" on:click=move |_| on_read.run(id_read.clone())>
                                {format!("{} ({} v{})", item.title, item.kind, item.version)}
                            </button>
                            <button type="button" class="button small" aria-label="Delete artifact" on:click=move |_| on_remove.run(id_del.clone())>"×"</button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {view_state.selected_id.clone().map(|aid| {
                let kind = selected_kind.clone().unwrap_or_else(|| "md".to_owned());
                let is_frame = matches!(kind.as_str(), "html" | "svg" | "react");
                let frame_src = session_id.as_deref().map(|sid| {
                    format!("/artifact-frame?session={sid}&id={aid}&version={selected_version}")
                });
                let content = view_state.selected_content.clone().unwrap_or_default();
                view! {
                    <div class="artifact-viewer" data-testid="artifact-viewer">
                        <div class="artifact-viewer-toolbar">
                            <button type="button" class="button small" on:click=move |_| show_diff.update(|d| *d = !*d)>
                                {move || if show_diff.get() { "Preview" } else { "Diff / Source" }}
                            </button>
                        </div>
                        {move || {
                            if show_diff.get() {
                                let diff_html = render_diff_html("", &content);
                                view! { <div class="artifact-diff" inner_html=diff_html></div> }.into_any()
                            } else if is_frame && let Some(src) = frame_src.clone() {
                                view! {
                                    <iframe
                                        class="artifact-iframe"
                                        data-testid="artifact-iframe"
                                        sandbox="allow-scripts allow-modals allow-pointer-lock"
                                        src=src
                                    ></iframe>
                                }.into_any()
                            } else if kind == "md" {
                                let html = render_html(&content);
                                view! { <div class="elanor-markdown" inner_html=html></div> }.into_any()
                            } else {
                                view! { <pre class="artifact-preview">{content.clone()}</pre> }.into_any()
                            }
                        }}
                    </div>
                }
            })}
        </div>
    }
}
