//! Built-in plugin side panels for Wave 1.6c:
//! `login`, `devices`, `discord`, `envs`, `ports`, `logs`, `telemetry`,
//! `subagents`, `chat-mode`, `familiar`, `prompt`, `history`, `scope-prompt`.

use crate::browser_plugins::{
    ChatModeView, DevicesView, DiscordView, EnvsView, FamiliarView, LoginView, LogsView, PortsView,
    PromptResourcesView, SubagentsView, TracesView,
};
use leptos::prelude::*;

#[component]
pub fn LoginPanel(
    view_state: LoginView,
    err: Option<String>,
    on_close: Callback<()>,
    on_start: Callback<(String, String)>,
    on_answer: Callback<(String, String, String)>,
    on_cancel: Callback<String>,
    on_dismiss: Callback<String>,
    on_logout: Callback<String>,
    on_refresh: Callback<String>,
) -> impl IntoView {
    let prompt_val = RwSignal::new(String::new());
    view! {
        <div class="sheet" role="dialog" aria-label="Provider Login" data-testid="panel-login">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Provider Login ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            {view_state.active.map(|attempt| {
                let id_ans = attempt.attempt_id.clone();
                let id_can = attempt.attempt_id.clone();
                let id_dis = attempt.attempt_id.clone();
                let prompt_id = attempt.prompt_id.clone();
                let prompt_lbl = attempt.prompt_label.clone().unwrap_or_else(|| "Enter response".into());
                view! {
                    <div class="login-attempt">
                        <p class="small">{format!("{} [{}]: {}", attempt.provider_id, attempt.status, attempt.message)}</p>
                        {prompt_id.map(|pid| view! {
                            <div class="panel-form">
                                <input
                                    type="text"
                                    placeholder=prompt_lbl
                                    prop:value=move || prompt_val.get()
                                    on:input=move |ev| prompt_val.set(event_target_value(&ev))
                                />
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| {
                                        on_answer.run((id_ans.clone(), pid.clone(), prompt_val.get_untracked()));
                                        prompt_val.set(String::new());
                                    }
                                >"Submit"</button>
                            </div>
                        })}
                        <div class="row-actions">
                            <button type="button" class="button small" on:click=move |_| on_cancel.run(id_can.clone())>"Cancel"</button>
                            <button type="button" class="button small" on:click=move |_| on_dismiss.run(id_dis.clone())>"Dismiss"</button>
                        </div>
                    </div>
                }
            })}
            <ul class="plugin-list">
                {view_state.providers.into_iter().map(|p| {
                    let pid_out = p.id.clone();
                    let pid_ref = p.id.clone();
                    let signed = p.signed_in.clone();
                    view! {
                        <li class="provider-row">
                            <div>
                                <strong>{p.name}</strong>
                                {signed.as_ref().map(|s| view! { <span class="muted small">{format!(" ({s})")}</span> })}
                            </div>
                            <div class="row-actions">
                                {p.methods.into_iter().map(|m| {
                                    let pid = p.id.clone();
                                    let auth = m.auth_type.clone();
                                    view! {
                                        <button type="button" class="button small" on:click=move |_| on_start.run((pid.clone(), auth.clone()))>
                                            {m.label}
                                        </button>
                                    }
                                }).collect_view()}
                                {signed.is_some().then(|| view! {
                                    <button type="button" class="button small" on:click=move |_| on_refresh.run(pid_ref.clone())>"Refresh"</button>
                                })}
                                {signed.is_some().then(|| view! {
                                    <button type="button" class="button small" on:click=move |_| on_logout.run(pid_out.clone())>"Logout"</button>
                                })}
                            </div>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn DevicesPanel(
    view_state: DevicesView,
    err: Option<String>,
    on_close: Callback<()>,
    on_mint_token: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="sheet" role="dialog" aria-label="Devices" data-testid="panel-devices">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Devices ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <p class="muted small">
                {if view_state.can_mint { "Mint a one-time pairing token for another browser or CLI." } else { "Device pairing is owner-only." }}
            </p>
            <button
                type="button"
                class="button small"
                disabled=!view_state.can_mint
                data-testid="devices-mint-btn"
                on:click=move |_| on_mint_token.run(())
            >"Mint Pairing Token"</button>
            {view_state.minted_token.map(|tok| view! {
                <pre class="device-token" data-testid="devices-token-output">{tok}</pre>
            })}
            {view_state.minted_url.map(|url| view! {
                <p class="small">{url}</p>
            })}
        </div>
    }
}

#[component]
pub fn DiscordPanel(
    view_state: DiscordView,
    active_session: Option<String>,
    err: Option<String>,
    on_close: Callback<()>,
    on_add_bot: Callback<String>,
    on_remove_bot: Callback<String>,
    on_link: Callback<(String, String, String, String)>,
    on_unlink: Callback<String>,
) -> impl IntoView {
    let token = RwSignal::new(String::new());
    let bot_id = RwSignal::new(String::new());
    let guild_id = RwSignal::new(String::new());
    let channel_id = RwSignal::new(String::new());

    view! {
        <div class="sheet" role="dialog" aria-label="Discord" data-testid="panel-discord">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Discord ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let t = token.get_untracked().trim().to_owned();
                    if !t.is_empty() {
                        on_add_bot.run(t);
                        token.set(String::new());
                    }
                }
            >
                <input
                    type="password"
                    placeholder="Discord bot token"
                    prop:value=move || token.get()
                    on:input=move |ev| token.set(event_target_value(&ev))
                />
                <button type="submit" class="button small">"Add Bot"</button>
            </form>
            <ul class="plugin-list">
                {view_state.bots.into_iter().map(|b| {
                    let bid = b.id.clone();
                    view! {
                        <li class="row-actions">
                            <span>{format!("{} [{}]", b.label, b.status)}</span>
                            <button type="button" class="button small" on:click=move |_| on_remove_bot.run(bid.clone())>"Remove"</button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {active_session.map(|sid| view! {
                <form
                    class="panel-form"
                    on:submit=move |ev| {
                        ev.prevent_default();
                        on_link.run((
                            sid.clone(),
                            bot_id.get_untracked(),
                            guild_id.get_untracked(),
                            channel_id.get_untracked(),
                        ));
                    }
                >
                    <input type="text" placeholder="Bot ID" prop:value=move || bot_id.get() on:input=move |ev| bot_id.set(event_target_value(&ev)) />
                    <input type="text" placeholder="Guild ID" prop:value=move || guild_id.get() on:input=move |ev| guild_id.set(event_target_value(&ev)) />
                    <input type="text" placeholder="Channel ID" prop:value=move || channel_id.get() on:input=move |ev| channel_id.set(event_target_value(&ev)) />
                    <button type="submit" class="button small">"Link Session"</button>
                </form>
            })}
            <ul class="plugin-list">
                {view_state.bindings.into_iter().map(|b| {
                    let sid = b.session.clone();
                    view! {
                        <li class="row-actions">
                            <span>{format!("{} -> {}:{}", b.session, b.guild, b.channel)}</span>
                            <button type="button" class="button small" on:click=move |_| on_unlink.run(sid.clone())>"Unlink"</button>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn EnvsPanel(
    view_state: EnvsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_save: Callback<(String, String)>,
    on_test: Callback<String>,
    on_delete: Callback<String>,
    on_generate_key: Callback<String>,
    on_sandbox_start: Callback<String>,
    on_sandbox_stop: Callback<String>,
) -> impl IntoView {
    let env_name = RwSignal::new(String::new());
    let env_host = RwSignal::new(String::new());

    view! {
        <div class="sheet" role="dialog" aria-label="Environments" data-testid="panel-envs">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Environments ×"</button>
            {err.or(view_state.failure).map(|e| view! { <p class="muted small">{format!("Status: {e}")}</p> })}
            {view_state.last_result.map(|res| view! { <p class="small">{res}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let name = env_name.get_untracked().trim().to_owned();
                    let host = env_host.get_untracked().trim().to_owned();
                    if !name.is_empty() && !host.is_empty() {
                        on_save.run((name, host));
                        env_name.set(String::new());
                        env_host.set(String::new());
                    }
                }
            >
                <input type="text" placeholder="Env name" prop:value=move || env_name.get() on:input=move |ev| env_name.set(event_target_value(&ev)) />
                <input type="text" placeholder="SSH host / sandbox" prop:value=move || env_host.get() on:input=move |ev| env_host.set(event_target_value(&ev)) />
                <button type="submit" class="button small">"Save Env"</button>
            </form>
            <ul class="plugin-list">
                {view_state.envs.into_iter().map(|e| {
                    let n_test = e.name.clone();
                    let n_key = e.name.clone();
                    let n_del = e.name.clone();
                    let n_start = e.name.clone();
                    let n_stop = e.name.clone();
                    view! {
                        <li class="env-row">
                            <span>{format!("{} ({}) {}", e.name, e.kind, e.status.as_deref().unwrap_or(""))}</span>
                            <div class="row-actions">
                                <button type="button" class="button small" on:click=move |_| on_test.run(n_test.clone())>"Test"</button>
                                <button type="button" class="button small" on:click=move |_| on_generate_key.run(n_key.clone())>"Key"</button>
                                <button type="button" class="button small" on:click=move |_| on_sandbox_start.run(n_start.clone())>"Start"</button>
                                <button type="button" class="button small" on:click=move |_| on_sandbox_stop.run(n_stop.clone())>"Stop"</button>
                                <button type="button" class="button small" on:click=move |_| on_delete.run(n_del.clone())>"Delete"</button>
                            </div>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn PortsPanel(
    view_state: PortsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_add: Callback<(u16, String, bool)>,
    on_set_public: Callback<(String, bool)>,
    on_remove: Callback<String>,
) -> impl IntoView {
    let port_input = RwSignal::new(String::new());
    let name_input = RwSignal::new(String::new());

    view! {
        <div class="sheet" role="dialog" aria-label="Ports" data-testid="panel-ports">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Ports ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    if let Ok(port) = port_input.get_untracked().trim().parse::<u16>() {
                        on_add.run((port, name_input.get_untracked().trim().to_owned(), false));
                        port_input.set(String::new());
                        name_input.set(String::new());
                    }
                }
            >
                <input type="number" placeholder="Port (e.g. 3000)" prop:value=move || port_input.get() on:input=move |ev| port_input.set(event_target_value(&ev)) />
                <input type="text" placeholder="Label" prop:value=move || name_input.get() on:input=move |ev| name_input.set(event_target_value(&ev)) />
                <button type="submit" class="button small">"Forward Port"</button>
            </form>
            <ul class="plugin-list">
                {view_state.forwards.into_iter().map(|f| {
                    let id_pub = f.id.clone();
                    let id_rm = f.id.clone();
                    let next_pub = !f.public;
                    view! {
                        <li class="port-row">
                            <span>{format!(":{} {}", f.port, f.name)}</span>
                            <div class="row-actions">
                                <button type="button" class="button small" on:click=move |_| on_set_public.run((id_pub.clone(), next_pub))>
                                    {if f.public { "Make Private" } else { "Make Public" }}
                                </button>
                                <button type="button" class="button small" on:click=move |_| on_remove.run(id_rm.clone())>"Remove"</button>
                            </div>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn LogsPanel(
    view_state: LogsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_select_source: Callback<String>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    view! {
        <div class="sheet" role="dialog" aria-label="Logs" data-testid="panel-logs">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Logs ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <div class="panel-form">
                <select
                    prop:value=view_state.source.clone()
                    on:change=move |ev| on_select_source.run(event_target_value(&ev))
                >
                    {view_state.sources.into_iter().map(|s| {
                        let val = s.clone();
                        view! {
                            <option value=val>{s}</option>
                        }
                    }).collect_view()}
                </select>
                <input
                    type="search"
                    placeholder="Filter logs..."
                    prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))
                />
            </div>
            <pre class="logs-output">
                {move || {
                    let q = filter.get().to_ascii_lowercase();
                    view_state
                        .lines
                        .iter()
                        .filter(|l| q.is_empty() || l.message.to_ascii_lowercase().contains(&q))
                        .map(|l| format!("[{}] {}", l.level, l.message))
                        .collect::<Vec<_>>()
                        .join("\n")
                }}
            </pre>
        </div>
    }
}

#[component]
pub fn TelemetryPanel(
    view_state: TracesView,
    err: Option<String>,
    on_close: Callback<()>,
    on_find_traces: Callback<String>,
    on_get_trace: Callback<String>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    view! {
        <div class="sheet" role="dialog" aria-label="Telemetry" data-testid="panel-telemetry">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Telemetry ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    on_find_traces.run(query.get_untracked());
                }
            >
                <input type="search" placeholder="Trace name filter" prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
                <button type="submit" class="button small">"Find Traces"</button>
            </form>
            <ul class="plugin-list">
                {view_state.traces.into_iter().map(|t| {
                    let tid = t.trace_id.clone();
                    view! {
                        <li>
                            <button type="button" on:click=move |_| on_get_trace.run(tid.clone())>
                                {format!("{} ({}ms · {})", t.name, t.duration_ms, t.status)}
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {view_state.selected_trace.map(|detail| view! {
                <pre class="trace-detail">{detail}</pre>
            })}
        </div>
    }
}

#[component]
pub fn SubagentsPanel(
    view_state: SubagentsView,
    err: Option<String>,
    on_close: Callback<()>,
    on_open_session: Callback<String>,
) -> impl IntoView {
    view! {
        <div class="sheet" role="dialog" aria-label="Subagents" data-testid="panel-subagents">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Subagents ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <ul class="plugin-list">
                {view_state.children.into_iter().map(|c| {
                    let sid = c.session_id.clone();
                    view! {
                        <li>
                            <button type="button" on:click=move |_| on_open_session.run(sid.clone())>
                                {format!("{} [{}]", c.title, c.status)}
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn ChatModePanel(
    view_state: ChatModeView,
    err: Option<String>,
    on_close: Callback<()>,
    on_set_enabled: Callback<bool>,
    on_rename: Callback<String>,
) -> impl IntoView {
    let agent_name = RwSignal::new(view_state.name.clone());
    let next_enabled = !view_state.enabled;
    view! {
        <div class="sheet" role="dialog" aria-label="Chat Mode" data-testid="panel-chat-mode">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Chat Mode ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <p class="small muted">{format!("Waiting messages: {}", view_state.waiting)}</p>
            <button
                type="button"
                class="button small"
                on:click=move |_| on_set_enabled.run(next_enabled)
            >
                {if view_state.enabled { "Disable Chat Mode" } else { "Enable Chat Mode" }}
            </button>
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let n = agent_name.get_untracked().trim().to_owned();
                    if !n.is_empty() {
                        on_rename.run(n);
                    }
                }
            >
                <input type="text" prop:value=move || agent_name.get() on:input=move |ev| agent_name.set(event_target_value(&ev)) />
                <button type="submit" class="button small">"Rename Agent"</button>
            </form>
        </div>
    }
}

#[component]
pub fn FamiliarPanel(
    view_state: FamiliarView,
    err: Option<String>,
    on_close: Callback<()>,
    on_open_familiar: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="sheet" role="dialog" aria-label="Familiar" data-testid="panel-familiar">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Familiar ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <p class="small muted">
                {view_state.session_id.map(|s| format!("Session: {s}")).unwrap_or_else(|| "No familiar session active".into())}
            </p>
            <button type="button" class="button small" on:click=move |_| on_open_familiar.run(())>"Open Familiar"</button>
        </div>
    }
}

#[component]
pub fn PromptPanel(
    view_state: PromptResourcesView,
    err: Option<String>,
    on_close: Callback<()>,
    on_expand: Callback<String>,
) -> impl IntoView {
    view! {
        <div class="sheet" role="dialog" aria-label="Prompt Resources" data-testid="panel-prompt">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Prompt Resources ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <p class="small muted">"Templates"</p>
            <ul class="plugin-list">
                {view_state.templates.into_iter().map(|t| {
                    let cmd = format!("/{}", t.name);
                    view! {
                        <li>
                            <button type="button" on:click=move |_| on_expand.run(cmd.clone())>
                                {format!("/{} — {}", t.name, t.description)}
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            <p class="small muted">"Skills"</p>
            <ul class="plugin-list">
                {view_state.skills.into_iter().map(|s| view! {
                    <li>{format!("{} — {}", s.name, s.description)}</li>
                }).collect_view()}
            </ul>
        </div>
    }
}

#[component]
pub fn HistoryPanel(
    entries: Vec<(String, String, String)>,
    err: Option<String>,
    on_close: Callback<()>,
    on_load_earlier: Callback<()>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    view! {
        <div class="sheet" role="dialog" aria-label="History" data-testid="panel-history">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"History ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <div class="panel-form">
                <input type="search" placeholder="Filter entries..." prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
                <button type="button" class="button small" on:click=move |_| on_load_earlier.run(())>"Load Earlier"</button>
            </div>
            <ul class="plugin-list">
                {move || {
                    let q = filter.get().to_ascii_lowercase();
                    entries
                        .iter()
                        .filter(|(_, _, text)| q.is_empty() || text.to_ascii_lowercase().contains(&q))
                        .map(|(id, role, text)| {
                            let preview: String = text.chars().take(80).collect();
                            view! { <li>{format!("#{id} [{role}] {preview}")}</li> }
                        })
                        .collect_view()
                }}
            </ul>
        </div>
    }
}

#[component]
pub fn ScopePromptPanel(
    err: Option<String>,
    on_close: Callback<()>,
    on_stage: Callback<(String, String)>,
) -> impl IntoView {
    let block = RwSignal::new(String::new());
    let note = RwSignal::new(String::new());
    view! {
        <div class="sheet" role="dialog" aria-label="Stage Scope Prompt" data-testid="panel-scope-prompt">
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>"Scope Prompt ×"</button>
            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
            <form
                class="panel-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    let b = block.get_untracked();
                    let n = note.get_untracked();
                    if !b.trim().is_empty() || !n.trim().is_empty() {
                        on_stage.run((b, n));
                        block.set(String::new());
                        note.set(String::new());
                    }
                }
            >
                <textarea rows="4" placeholder="Scope block..." prop:value=move || block.get() on:input=move |ev| block.set(event_target_value(&ev))></textarea>
                <input type="text" placeholder="Note" prop:value=move || note.get() on:input=move |ev| note.set(event_target_value(&ev)) />
                <button type="submit" class="button small" data-testid="scope-prompt-stage-btn">"Stage"</button>
            </form>
        </div>
    }
}
