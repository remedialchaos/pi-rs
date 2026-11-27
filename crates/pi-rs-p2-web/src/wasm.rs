//! Leptos browser host for the p2 Chord contracts.
#![cfg(target_arch = "wasm32")]

mod storage;
mod terminal_ws;
mod voice_ws;
mod wire;

use crate::activity::model::default_tool_activities;
use crate::browser_plugins::extra_indexes;
use crate::browser_plugins::{
    ArtifactsView, ChatModeView, ComposerAttachments, ContractSpec, DRAFT_STORAGE_KEY, DevicesView,
    DiscordView, EnvsView, FamiliarView, HoldTracker, LogNoticeView, LoginView, LogsView,
    McpServersView, ModelsView, P2_ACCESS, P2_ARTIFACTS, P2_CHAT, P2_CHAT_COSTS, P2_CHAT_MODE,
    P2_DEVICES, P2_DISCORD, P2_ENVS, P2_FAMILIAR, P2_FILE_INDEX, P2_LOG_NOTICES, P2_LOGS, P2_MCP,
    P2_MODELS, P2_PORTS, P2_PROJECTS, P2_PROMPT_RESOURCES, P2_PROVIDER_LOGIN, P2_SCOPE_STAGE,
    P2_SEARCH, P2_SENT_FILES, P2_SESSION_FILES, P2_SESSION_METRICS, P2_SESSION_STATUS, P2_SESSIONS,
    P2_SETTINGS, P2_SHARING, P2_SIDEBAR_ROLLUPS, P2_SUBAGENTS, P2_TERMINALS, P2_TRACES, P2_TREE,
    P2_VIEWS, P2_VOICE_MODE, P2_VOICE_SERVICE, P2_WORKSPACE, PLUGIN_RELOAD_EVENT,
    PluginRegistryState, PortsView, ProjectedMessage, ProjectedRole, ProjectsView,
    PromptResourcesView, RELOAD_STATE_STORAGE_KEY, ReloadSnapshot, ReloadTrigger, RollupView,
    SearchHitView, SessionMetricsView, SessionStatusView, SettingsView, SharingView, SidePanel,
    SubagentsView, TerminalsView, TracesView, TreeView, ViewsLayoutView, VoiceView, WorkspaceView,
    decode_artifact_content, decode_artifacts_view, decode_chat_mode_view, decode_costs_view,
    decode_devices_view, decode_discord_view, decode_envs_view, decode_familiar_view,
    decode_log_notices_view, decode_login_view, decode_logs_view, decode_mcp_view,
    decode_models_view, decode_plugin_manifest, decode_ports_view, decode_projects_view,
    decode_prompt_resources_view, decode_reload_snapshot, decode_rollups_view,
    decode_search_results, decode_session_metrics_view, decode_session_status_view,
    decode_settings_view, decode_sharing_view, decode_subagents_view, decode_terminals_list,
    decode_traces_result, decode_tree_view, decode_views_state, decode_voice_view,
    decode_workspace_changes, detect_asset_revision_change, detect_close_reload, draft_key_for,
    encode_release_frame, encode_reload_snapshot, format_draft_storage, hold_for_contract, indexes,
    project_chat_transcript, restore_draft_for_conversation,
};
use crate::chord_bridge::{
    HOST_MENU_EVENT, HOST_THEME_EVENT, HostAnswer, HostMenuEvent, HtmlThemeAttributes,
    PI_HOST_HANDLER, ShellBridgeMessage, THEME_STORAGE_KEY, ThemeBridgeState, ThemePreference,
    format_document_title, is_native_host,
};
use crate::command_menu::keys::{Modifiers, ShortcutAction, resolve_shortcut};
use crate::command_menu::recents::{KEY as RECENTS_STORAGE_KEY, Recents};
use crate::cost::AssistantCost;
use crate::gestures::drag_size::read_size;
use crate::state::attachments::{
    MAX_ATTACHMENTS_PER_MESSAGE, UPLOAD_CHUNK_BYTES, UploadChunk, decode_begin_upload_result,
    decode_finish_upload_result, encode_append_upload_call, encode_begin_upload_call,
    encode_cancel_upload_call, encode_finish_upload_call, rewrite_attachment_reference,
    split_upload_chunks,
};
use crate::state::conversation_cache::{
    MAX_CONVERSATIONS, MAX_PERSISTED_STORE_BYTES, PersistedTranscriptStore,
    STORAGE_KEY_TRANSCRIPTS, projected_from_cached, reconcile_live_snapshot,
};
use crate::state::file_completion::{
    Popover, apply_file_completion, at_items, extract_at_prefix, out_of_scope, parse_path_prefix,
};
use crate::state::navigation::{
    LAST_CONVERSATION_STORAGE_KEY, MAINS_STORAGE_KEY, NAV_STORAGE_KEY, NavigationState, Route,
    SCROLL_STORAGE_KEY, SESSION_PANELS_STORAGE_KEY, SIDEBAR_COLLAPSED_KEY, SessionPanelsStore,
    WorkspaceTab, decode_last_conversation, dismissed_notice_key, encode_last_conversation,
    path_route, route_path,
};
use crate::ui::{
    app::{AppLayout, MainPane},
    chat::{Chat, ChatMessage, MessageRole},
    command_menu::CommandMenu,
    composer::{Composer, SelectedUploadFile},
    cost_ui::CostPanel,
    dynamic_plugin::DynamicPluginView,
    notices::OwnerNotices,
    panels::{ArtifactsPanel, McpPanel, ModelsPanel, SettingsPanel, SharingPanel, TreePanel},
    plugin_panels::{
        ChatModePanel, DevicesPanel, DiscordPanel, EnvsPanel, FamiliarPanel, HistoryPanel,
        LoginPanel, LogsPanel, PortsPanel, PromptPanel, ScopePromptPanel, SubagentsPanel,
        TelemetryPanel,
    },
    session_stats::SessionStats,
    sessions_sidebar::{ListPageView, NewSessionView, RichSessionsControls, SessionSummary},
    terminal::{Terminal, Voice},
    views::ViewsPanel,
    workspace::WorkspacePane,
};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::{mount::mount_to, prelude::*};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use storage::{
    idb_delete_session, idb_load_all, idb_put_conversation, local_get, local_remove, local_set,
    session_get, session_remove, session_set,
};
use terminal_ws::{TerminalRuntime, TerminalWsAction, spawn_terminal_channel};
use voice_ws::{VoiceRuntime, VoiceWsAction, play_voice_sample, start_voice_session};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlElement, KeyboardEvent, MediaQueryList, Url};
use wire::{Reader, Result};

const SIDEBAR_WIDTH_KEY: &str = "pi-sidebar-width";
const PANEL_WIDTH_KEY: &str = "pi-panel-width";
const WORKSPACE_WRAP_KEY: &str = "pi-workspace-wrap";

#[derive(Clone, PartialEq)]
struct Session {
    id: String,
    title: String,
    main: u64,
    pinned: bool,
    archived: bool,
}

#[derive(Clone)]
enum Command {
    Open(String, u64),
    Navigate(Route, bool),
    Send(String),
    Stop,
    Create,
    CreateWith {
        cwd: String,
        env: Option<String>,
        title: Option<String>,
    },
    OpenPanel(SidePanel),
    OpenCustomPanel(String),
    OpenPane(MainPane),
    // Wave 1.6b interactive mutations
    UpdateSetting(String, String),
    UnsetSetting(String),
    SetDefaultModel(String, String, String),
    SetSessionModel(String, String, String),
    SetEnabledModels(Vec<String>),
    McpRestart(String),
    McpSignIn(String),
    McpSignOut(String),
    McpPasteRedirect(String, String),
    McpCancelSignIn(String),
    ShareCreateLink(String, bool),
    ShareEmail(String, String),
    ShareRemoveGuest(String),
    RefreshTree,
    SelectBranch(u64),
    TreeNavigate(u64),
    TreeBranch(u64),
    TreeRename(u64, String),
    ReadArtifact(String),
    RemoveArtifact(String),
    RefreshCosts,
    // Terminal & Voice
    OpenTerminal,
    TerminalAttach(String),
    TerminalClose(String),
    TerminalResize(u16, u16),
    TerminalAck(u64),
    TerminalRequestChannel,
    ToggleVoice,
    ChooseVoice(String, String, f64),
    // Wave 1.6c plugin commands
    LoginStart(String, String),
    LoginAnswer(String, String, String),
    LoginCancel(String),
    LoginDismiss(String),
    LoginLogout(String),
    LoginRefresh(String),
    MintDeviceToken,
    DiscordAddBot(String),
    DiscordRemoveBot(String),
    DiscordLink(String, String, String, String),
    DiscordUnlink(String),
    EnvSave(String, String),
    EnvTest(String),
    EnvDelete(String),
    EnvGenerateKey(String),
    EnvSandboxStart(String),
    EnvSandboxStop(String),
    PortAdd(u16, String, bool),
    PortSetPublic(String, bool),
    PortRemove(String),
    SelectLogSource(String),
    DismissNotice(String),
    FindTraces(String),
    GetTrace(String),
    QueryWorkspace,
    CompleteFiles(String),
    ListWorkspaceDir(String),
    ReadWorkspaceFile(String),
    ViewsOpen,
    ViewsSelect(String, Option<String>),
    ViewsClose(String),
    ViewsUnannotate(Vec<String>),
    StageScopePrompt(String, String),
    ChatModeSet(bool),
    ChatModeRename(String),
    OpenFamiliar,
    ExpandPrompt(String),
    LoadEarlierEntries,
    SearchSessions(String, bool),
    SessionPin(String, bool),
    SessionRename(String, String),
    SessionArchive(String, bool),
    SessionClone(String),
    UploadFile {
        key: String,
        name: String,
        bytes: Vec<u8>,
    },
    CancelUpload(String),
}

#[derive(Clone)]
struct Submission {
    destination: (String, u64),
    text: String,
    id: String,
}

#[derive(Clone)]
enum UploadRpc {
    Begin { key: String },
    Append { key: String, next_index: usize },
    Finish { key: String },
    Cancel,
}

struct ActiveUpload {
    hold_id: u64,
    total_bytes: u64,
    chunks: Vec<UploadChunk>,
    upload_token: Option<String>,
    cancelled: bool,
}

#[derive(Clone, Copy)]
struct Ui {
    sessions: RwSignal<Vec<Session>>,
    messages: RwSignal<Vec<ChatMessage>>,
    selected: RwSignal<Option<(String, u64)>>,
    nav: StoredValue<NavigationState>,
    route_page: RwSignal<Option<String>>,
    route_new: RwSignal<bool>,
    session_panels: StoredValue<SessionPanelsStore>,
    status: RwSignal<String>,
    write: RwSignal<bool>,
    create: RwSignal<bool>,
    default_cwd: RwSignal<String>,
    default_env: RwSignal<Option<String>>,
    draft: RwSignal<String>,
    guest: RwSignal<bool>,
    last_submission: StoredValue<Option<Submission>>,
    busy: RwSignal<bool>,
    streaming: RwSignal<bool>,
    commands: StoredValue<Option<UnboundedSender<Command>>>,
    pane: RwSignal<MainPane>,
    panel: RwSignal<SidePanel>,
    custom_panel: RwSignal<Option<String>>,
    command_menu_open: RwSignal<bool>,
    command_query: RwSignal<String>,
    command_recents: RwSignal<Vec<String>>,
    completion: RwSignal<Option<Popover>>,
    sidebar_collapsed: RwSignal<bool>,
    sidebar_width: RwSignal<u32>,
    panel_width: RwSignal<u32>,
    workspace_tab: RwSignal<WorkspaceTab>,
    workspace_wrap: RwSignal<bool>,
    models: RwSignal<ModelsView>,
    settings: RwSignal<SettingsView>,
    mcp: RwSignal<McpServersView>,
    sharing: RwSignal<SharingView>,
    session_status: RwSignal<SessionStatusView>,
    costs: RwSignal<Vec<AssistantCost>>,
    artifacts: RwSignal<ArtifactsView>,
    tree: RwSignal<TreeView>,
    terminals: RwSignal<TerminalsView>,
    terminal_rt: TerminalRuntime,
    voice: RwSignal<VoiceView>,
    voice_rt: VoiceRuntime,
    login: RwSignal<LoginView>,
    devices: RwSignal<DevicesView>,
    discord: RwSignal<DiscordView>,
    envs: RwSignal<EnvsView>,
    ports: RwSignal<PortsView>,
    logs: RwSignal<LogsView>,
    log_notices: RwSignal<Vec<LogNoticeView>>,
    dismissed_notices: RwSignal<HashSet<String>>,
    traces: RwSignal<TracesView>,
    workspace: RwSignal<WorkspaceView>,
    views_state: RwSignal<ViewsLayoutView>,
    subagents: RwSignal<SubagentsView>,
    chat_mode: RwSignal<ChatModeView>,
    familiar: RwSignal<FamiliarView>,
    prompt_res: RwSignal<PromptResourcesView>,
    projects: RwSignal<ProjectsView>,
    rollups: RwSignal<Vec<RollupView>>,
    search_hits: RwSignal<Vec<SearchHitView>>,
    session_metrics: RwSignal<SessionMetricsView>,
    attachments: RwSignal<ComposerAttachments>,
    plugin_errors: RwSignal<HashMap<String, String>>,
    transcript_cache: StoredValue<PersistedTranscriptStore>,
    plugin_registry: RwSignal<PluginRegistryState>,
    asset_revision: StoredValue<String>,
    theme: StoredValue<ThemeBridgeState>,
    theme_label: RwSignal<&'static str>,
}

impl Ui {
    fn command(self, cmd: Command) {
        if let Some(tx) = self.commands.get_value() {
            let _ = tx.unbounded_send(cmd);
        }
    }
}

fn persist_transcript_cache(store: &PersistedTranscriptStore, updated_conv: Option<(String, u64)>) {
    let json = store.to_bounded_json(MAX_PERSISTED_STORE_BYTES);
    if !local_set(STORAGE_KEY_TRANSCRIPTS, &json) {
        let smaller = store.to_bounded_json(MAX_PERSISTED_STORE_BYTES / 2);
        let _ = local_set(STORAGE_KEY_TRANSCRIPTS, &smaller);
    }
    if let Some((sid, conv)) = updated_conv
        && let Some(entry) = store.get(&sid, conv).cloned()
    {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = idb_put_conversation(entry).await;
        });
    }
}

fn chat_messages_from_projected(rows: Vec<ProjectedMessage>) -> Vec<ChatMessage> {
    rows.into_iter()
        .map(|row| ChatMessage {
            id: row.id,
            role: match row.role {
                ProjectedRole::User => MessageRole::User,
                ProjectedRole::Assistant => MessageRole::Assistant,
                ProjectedRole::Tool => MessageRole::Tool,
            },
            text: row.text,
            streaming: row.streaming,
            activities: row.activities,
        })
        .collect()
}

fn hydrate_messages_from_cache(ui: Ui, session_id: &str, conversation: u64) {
    let cached = ui
        .transcript_cache
        .with_value(|store| store.get(session_id, conversation).cloned());
    if let Some(conv) = cached {
        let projected = projected_from_cached(&conv.messages);
        ui.messages.set(chat_messages_from_projected(projected));
    } else {
        ui.messages.set(vec![]);
    }
}

fn sync_browser_history(ui: Ui, route: Route, push: bool) {
    let path = route_path(&route);
    let idx = ui.nav.try_update_value(|nav| {
        if push {
            nav.push(route.clone());
        } else {
            nav.replace(route.clone());
        }
        let _ = session_set(NAV_STORAGE_KEY, &nav.encode_nav());
        let _ = session_set(SCROLL_STORAGE_KEY, &nav.encode_scroll());
        nav.doc
    });
    if let (Some(idx), Ok(history)) = (idx, window().history()) {
        let state = JsValue::from_f64(idx as f64);
        if push {
            let _ = history.push_state_with_url(&state, "", Some(&path));
        } else {
            let _ = history.replace_state_with_url(&state, "", Some(&path));
        }
    }
    ui.session_panels.try_update_value(|sp| {
        sp.remember(&route);
        let _ = local_set(SESSION_PANELS_STORAGE_KEY, &sp.to_json());
    });
    if let Some(sid) = route.session.as_deref() {
        let now = js_sys::Date::now().max(0.0) as u64;
        let _ = local_set(
            LAST_CONVERSATION_STORAGE_KEY,
            &encode_last_conversation(sid, route.conversation, now),
        );
    }
}

fn save_reload_state(ui: Ui) {
    let selected = ui.selected.get_untracked();
    let draft = ui.draft.get_untracked();
    if let Some((ref sid, conv)) = selected {
        if draft.is_empty() {
            session_remove(DRAFT_STORAGE_KEY);
        } else {
            let _ = session_set(DRAFT_STORAGE_KEY, &format_draft_storage(sid, conv, &draft));
        }
    }
    let panel_id = if let Some(custom) = ui.custom_panel.get_untracked() {
        Some(custom)
    } else {
        let p = ui.panel.get_untracked();
        p.is_open().then(|| p.id().to_owned())
    };
    let snap = ReloadSnapshot {
        session_id: selected.as_ref().map(|(s, _)| s.clone()),
        conversation: selected.as_ref().map(|(_, c)| *c),
        panel_id,
        draft_key: selected.as_ref().map(|(s, c)| draft_key_for(s, *c)),
        draft_text: draft,
        asset_revision: ui.asset_revision.get_value(),
        plugin_revision: Some(ui.plugin_registry.with_untracked(|r| r.revision.clone())),
    };
    let _ = local_set(RELOAD_STATE_STORAGE_KEY, &encode_reload_snapshot(&snap));
}

fn webkit_handler(name: &str) -> Option<JsValue> {
    let webkit = js_sys::Reflect::get(&window(), &"webkit".into()).ok()?;
    if webkit.is_undefined() || webkit.is_null() {
        return None;
    }
    let handlers = js_sys::Reflect::get(&webkit, &"messageHandlers".into()).ok()?;
    if handlers.is_undefined() || handlers.is_null() {
        return None;
    }
    let handler = js_sys::Reflect::get(&handlers, &JsValue::from_str(name)).ok()?;
    if handler.is_undefined() || handler.is_null() {
        None
    } else {
        Some(handler)
    }
}

fn post_shell_message(msg: &ShellBridgeMessage) -> Option<js_sys::Promise> {
    let handler = webkit_handler(msg.handler_name())?;
    let post = js_sys::Reflect::get(&handler, &"postMessage".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let payload = js_sys::JSON::parse(&msg.to_json()).ok()?;
    let result = post.call1(&handler, &payload).ok()?;
    result.dyn_into::<js_sys::Promise>().ok()
}

fn apply_html_theme(attrs: HtmlThemeAttributes) {
    if let Some(el) = document().document_element() {
        let _ = el.set_attribute("data-theme", attrs.data_theme);
        let _ = el.set_attribute("data-color-scheme", attrs.data_color_scheme);
    }
}

fn sync_theme_dom_and_shell(ui: Ui) {
    let (attrs, label, pref, resolved) = ui.theme.with_value(|state| {
        (
            state.html_attributes(),
            state.preference().label(),
            state.preference(),
            state.resolved(),
        )
    });
    apply_html_theme(attrs);
    ui.theme_label.set(label);
    let _ = post_shell_message(&ShellBridgeMessage::Appearance {
        preference: pref,
        resolved,
    });
}

fn send_pi_host_theme_request(ui: Ui, turn: u64, msg: crate::chord_bridge::PiHostMessage) {
    let is_set_reply = matches!(msg, crate::chord_bridge::PiHostMessage::SetTheme(_));
    let Some(promise) = post_shell_message(&ShellBridgeMessage::Host(msg)) else {
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        let Ok(val) = wasm_bindgen_futures::JsFuture::from(promise).await else {
            return;
        };
        let text = js_sys::JSON::stringify(&val)
            .ok()
            .and_then(|s| s.as_string());
        let answer = text.as_deref().and_then(HostAnswer::from_json);
        let changed = ui
            .theme
            .try_update_value(|state| state.handle_host_answer(turn, is_set_reply, answer))
            .unwrap_or(false);
        if changed {
            sync_theme_dom_and_shell(ui);
        }
    });
}

fn cycle_theme(ui: Ui) {
    let next = ui.theme.with_value(|state| state.preference().cycle());
    let native = is_native_host(webkit_handler(PI_HOST_HANDLER).is_some());
    if !native {
        if next == ThemePreference::System {
            local_remove(THEME_STORAGE_KEY);
        } else {
            let _ = local_set(THEME_STORAGE_KEY, next.as_str());
        }
    }
    let req = ui
        .theme
        .try_update_value(|state| state.user_select(next))
        .flatten();
    sync_theme_dom_and_shell(ui);
    if let Some((turn, msg)) = req {
        send_pi_host_theme_request(ui, turn, msg);
    }
}

fn endpoint(root: &HtmlElement) -> Result<(String, bool, String)> {
    let loc = window().location();
    if let Some(raw) = root.get_attribute("data-pi-bootstrap") {
        let data: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let url = Url::new_with_base(
            data["chordUrl"]
                .as_str()
                .ok_or("Radius gave this page no Chord URL")?,
            &loc.href().map_err(js_error)?,
        )
        .map_err(js_error)?;
        url.search_params().set("client", "cloud");
        return Ok((url.href(), true, String::new()));
    }
    let existing_base = js_sys::Reflect::get(&window(), &"chordPage".into())
        .ok()
        .and_then(|page| js_sys::Reflect::get(&page, &"base".into()).ok())
        .and_then(|base| base.as_string());
    let base = if let Some(base) = existing_base {
        base
    } else if let Some(base) = document()
        .document_element()
        .and_then(|html| html.get_attribute("data-web-version"))
    {
        let page = js_sys::Object::new();
        js_sys::Reflect::set(&page, &"base".into(), &JsValue::from_str(&base)).map_err(js_error)?;
        js_sys::Reflect::set(&window(), &"chordPage".into(), &page).map_err(js_error)?;
        base
    } else {
        String::new()
    };
    let scheme = if loc.protocol().map_err(js_error)? == "https:" {
        "wss"
    } else {
        "ws"
    };
    Ok((
        format!(
            "{scheme}://{}/chord?web={}",
            loc.host().map_err(js_error)?,
            js_sys::encode_uri_component(&base)
        ),
        false,
        base,
    ))
}

fn js_error(v: JsValue) -> String {
    format!("{v:?}")
}

fn uuid() -> Result<String> {
    Ok(window().crypto().map_err(js_error)?.random_uuid())
}

fn transcript(view: &Value) -> Vec<ProjectedMessage> {
    project_chat_transcript(view, &default_tool_activities())
}

struct Link {
    ui: Ui,
    catalog_member: u64,
    chat: u64,
    next_hold: u64,
    hold_contracts: HashMap<u64, &'static str>,
    contract_holds: HashMap<&'static str, u64>,
    tracker: HoldTracker,
    members: HashMap<(u64, u64), crate::chord_bridge::replica::Replica>,
    next_call: u64,
    pending_primary: Option<(u64, Command)>,
    pending_plugin: HashMap<u64, Command>,
    pending_uploads: HashMap<u64, UploadRpc>,
    active_uploads: HashMap<String, ActiveUpload>,
    greeted: bool,
}

impl Link {
    fn hold_contract(
        &mut self,
        spec: &'static ContractSpec,
        required: bool,
        session: Option<&str>,
        conversation: Option<u64>,
        out: &mut Vec<Vec<u8>>,
    ) -> u64 {
        if let Some(&existing) = self.contract_holds.get(spec.name) {
            return existing;
        }
        let id = self.next_hold;
        self.next_hold += 1;
        self.hold_contracts.insert(id, spec.name);
        self.contract_holds.insert(spec.name, id);
        self.tracker.register(id, spec.name, required);
        out.push(hold_for_contract(
            id,
            spec,
            session,
            conversation,
            Some("web"),
        ));
        id
    }

    fn release_contract(&mut self, name: &'static str, out: &mut Vec<Vec<u8>>) {
        if let Some(id) = self.contract_holds.remove(name) {
            self.hold_contracts.remove(&id);
            self.tracker.unregister(id);
            self.members.retain(|(h, _), _| *h != id);
            out.push(encode_release_frame(id));
        }
    }

    fn release_session_plugins(&mut self, out: &mut Vec<Vec<u8>>) {
        for name in [
            P2_CHAT_COSTS.name,
            P2_ARTIFACTS.name,
            P2_TREE.name,
            P2_TERMINALS.name,
            P2_VOICE_MODE.name,
            P2_SHARING.name,
            P2_SESSION_FILES.name,
            P2_WORKSPACE.name,
            P2_VIEWS.name,
            P2_SCOPE_STAGE.name,
            P2_CHAT_MODE.name,
            P2_PROMPT_RESOURCES.name,
            P2_FILE_INDEX.name,
            P2_SESSION_METRICS.name,
            P2_SENT_FILES.name,
        ] {
            self.release_contract(name, out);
        }
        self.active_uploads.clear();
        self.pending_uploads.clear();
    }

    fn send_plugin_call(
        &mut self,
        hold_id: u64,
        method: u64,
        args: Value,
        cmd: Command,
        out: &mut Vec<Vec<u8>>,
    ) {
        self.next_call += 1;
        out.push(wire::call(self.next_call, hold_id, method, args));
        self.pending_plugin.insert(self.next_call, cmd);
    }

    fn ensure_plugin_hold_for_panel(&mut self, panel: SidePanel, out: &mut Vec<Vec<u8>>) {
        let sid = self.ui.selected.get_untracked().map(|(s, _)| s);
        let conv = self.ui.selected.get_untracked().map(|(_, c)| c);
        match panel {
            SidePanel::Cost => {
                if let (Some(sid), Some(conv)) = (sid, conv) {
                    let h = self.hold_contract(&P2_CHAT_COSTS, false, Some(&sid), Some(conv), out);
                    self.send_plugin_call(
                        h,
                        indexes::chat_costs::METHOD_COSTS,
                        json!(null),
                        Command::RefreshCosts,
                        out,
                    );
                }
            }
            SidePanel::Artifacts => {
                if let Some(sid) = sid {
                    self.hold_contract(&P2_ARTIFACTS, false, Some(&sid), None, out);
                }
            }
            SidePanel::Tree => {
                if let Some(sid) = sid {
                    let h = self.hold_contract(&P2_TREE, false, Some(&sid), None, out);
                    self.send_plugin_call(
                        h,
                        indexes::tree::METHOD_TREE,
                        json!(null),
                        Command::RefreshTree,
                        out,
                    );
                }
            }
            SidePanel::Voice => {
                if let (Some(sid), Some(conv)) = (sid, conv) {
                    self.hold_contract(&P2_VOICE_MODE, false, Some(&sid), Some(conv), out);
                }
            }
            SidePanel::Sharing => {
                if let Some(sid) = sid {
                    self.hold_contract(&P2_SHARING, false, Some(&sid), None, out);
                }
            }
            SidePanel::Login => {
                self.hold_contract(&P2_PROVIDER_LOGIN, false, None, None, out);
            }
            SidePanel::Devices => {
                self.hold_contract(&P2_DEVICES, false, None, None, out);
            }
            SidePanel::Discord => {
                self.hold_contract(&P2_DISCORD, false, None, None, out);
            }
            SidePanel::Envs => {
                self.hold_contract(&P2_ENVS, false, None, None, out);
            }
            SidePanel::Ports => {
                self.hold_contract(&P2_PORTS, false, None, None, out);
            }
            SidePanel::Logs => {
                self.hold_contract(&P2_LOGS, false, None, None, out);
            }
            SidePanel::Telemetry => {
                let h = self.hold_contract(&P2_TRACES, false, None, None, out);
                self.send_plugin_call(
                    h,
                    extra_indexes::traces::METHOD_FIND_TRACES,
                    json!({ "limit": 25 }),
                    Command::FindTraces(String::new()),
                    out,
                );
            }
            SidePanel::Subagents => {
                self.hold_contract(&P2_SUBAGENTS, false, None, None, out);
            }
            SidePanel::ChatMode => {
                if let (Some(sid), Some(conv)) = (sid, conv) {
                    self.hold_contract(&P2_CHAT_MODE, false, Some(&sid), Some(conv), out);
                }
            }
            SidePanel::Familiar => {
                self.hold_contract(&P2_FAMILIAR, false, None, None, out);
            }
            SidePanel::Prompt => {
                if let (Some(sid), Some(conv)) = (sid, conv) {
                    self.hold_contract(&P2_PROMPT_RESOURCES, false, Some(&sid), Some(conv), out);
                }
            }
            SidePanel::ScopePrompt => {
                if let Some(sid) = sid {
                    self.hold_contract(&P2_SCOPE_STAGE, false, Some(&sid), None, out);
                }
            }
            SidePanel::Views => {
                if let Some(sid) = sid {
                    self.hold_contract(&P2_VIEWS, false, Some(&sid), None, out);
                }
            }
            SidePanel::None
            | SidePanel::Models
            | SidePanel::Settings
            | SidePanel::Mcp
            | SidePanel::History => {}
        }
    }

    fn select(&mut self, id: String, conversation: u64, out: &mut Vec<Vec<u8>>) {
        if self.chat >= 3 && self.ui.selected.get_untracked() == Some((id.clone(), conversation)) {
            return;
        }
        if self.chat >= 3 && self.ui.selected.get_untracked().is_some() {
            out.push(encode_release_frame(self.chat));
            self.hold_contracts.remove(&self.chat);
            self.contract_holds.remove(P2_CHAT.name);
            self.tracker.unregister(self.chat);
        }
        self.release_session_plugins(out);
        self.members.retain(|(h, _), _| {
            self.hold_contracts
                .get(h)
                .is_some_and(|name| *name != P2_CHAT.name)
        });
        let hold_id = self.next_hold;
        self.next_hold += 1;
        self.chat = hold_id;
        self.hold_contracts.insert(hold_id, P2_CHAT.name);
        self.contract_holds.insert(P2_CHAT.name, hold_id);
        self.tracker.register(hold_id, P2_CHAT.name, true);
        self.ui.write.set(false);
        hydrate_messages_from_cache(self.ui, &id, conversation);
        self.ui.costs.set(vec![]);
        self.ui.artifacts.set(ArtifactsView::default());
        self.ui.tree.set(TreeView::default());
        self.ui.workspace.set(WorkspaceView::default());
        out.push(wire::hold(hold_id, Some((&id, conversation))));
        self.ui.selected.set(Some((id.clone(), conversation)));
        self.hold_contract(&P2_SESSION_METRICS, false, Some(&id), None, out);
        self.hold_contract(&P2_FILE_INDEX, false, Some(&id), Some(conversation), out);

        let mut next_route = Route::session_conversation(&id, conversation);
        let current_panel = self.ui.panel.get_untracked();
        if current_panel.is_open() {
            next_route.panel = Some(current_panel.id().to_owned());
            self.ensure_plugin_hold_for_panel(current_panel, out);
        }
        if !self.ui.route_new.get_untracked() && self.ui.route_page.get_untracked().is_none() {
            sync_browser_history(self.ui, next_route, true);
        }
        if self.ui.pane.get_untracked() == MainPane::Terminal {
            let h = self.hold_contract(&P2_TERMINALS, false, Some(&id), None, out);
            self.send_plugin_call(
                h,
                indexes::terminals::METHOD_CHANNEL,
                json!({}),
                Command::TerminalRequestChannel,
                out,
            );
            self.send_plugin_call(
                h,
                indexes::terminals::METHOD_LIST,
                json!({}),
                Command::OpenTerminal,
                out,
            );
        } else if self.ui.pane.get_untracked() == MainPane::Workspace {
            self.hold_contract(&P2_WORKSPACE, false, Some(&id), Some(conversation), out);
        }
    }

    fn state_val(&self, contract: &str, member: u64) -> Option<&Value> {
        let h = *self.contract_holds.get(contract)?;
        self.members.get(&(h, member)).map(|r| &r.value)
    }

    fn publish(&mut self, h: u64, m: u64, out: &mut Vec<Vec<u8>>) {
        let Some(&contract) = self.hold_contracts.get(&h) else {
            return;
        };
        let value = &self.members[&(h, m)].value;
        match contract {
            "p2.sessions" if m == self.catalog_member => {
                let rows: Vec<_> = value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| {
                        Some(Session {
                            id: s["id"].as_str()?.into(),
                            title: s["title"].as_str()?.into(),
                            main: s["main"].as_u64().unwrap_or(1),
                            pinned: s["pinned"].as_bool().unwrap_or(false),
                            archived: !s["archivedAt"].is_null(),
                        })
                    })
                    .collect();
                let active_rows: Vec<_> = rows.iter().filter(|s| !s.archived).cloned().collect();
                let pick = self
                    .ui
                    .selected
                    .get_untracked()
                    .filter(|(id, _)| rows.iter().any(|s| s.id == *id))
                    .or_else(|| active_rows.first().map(|s| (s.id.clone(), s.main)));
                self.ui.sessions.set(rows);
                if let Some((id, c)) = pick
                    && (self.chat == 0 || self.ui.selected.get_untracked() != Some((id.clone(), c)))
                {
                    self.select(id, c, out);
                } else if active_rows.is_empty()
                    && self.ui.selected.get_untracked().is_some()
                    && self.chat >= 3
                {
                    out.push(encode_release_frame(self.chat));
                    self.release_session_plugins(out);
                    self.ui.selected.set(None);
                    self.ui.messages.set(vec![]);
                    self.ui.write.set(false);
                }
            }
            "p2.sessions" if m == indexes::sessions::STATE_OWNER_DEFAULT_CWD => {
                if let Some(cwd) = value.as_str() {
                    self.ui.default_cwd.set(cwd.to_owned());
                }
            }
            "p2.sessions" if m == indexes::sessions::STATE_OWNER_DEFAULT_ENV => {
                self.ui.default_env.set(value.as_str().map(str::to_owned));
            }
            "p2.chat" if h == self.chat && m == indexes::chat::STATE_VIEW => {
                let live_rows = transcript(value);
                let is_streaming = value["docs"]["pi.live"]["generation"]["message"]["role"]
                    .as_str()
                    .is_some();
                self.ui.streaming.set(is_streaming);
                let reconciled = if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let preserve_prefix =
                        value["before"].is_u64() || value["more"].as_bool() == Some(true);
                    let cached_rows = self.ui.transcript_cache.with_value(|store| {
                        store
                            .get(&sid, conv)
                            .map(|c| c.messages.clone())
                            .unwrap_or_default()
                    });
                    let merged = reconcile_live_snapshot(&cached_rows, &live_rows, preserve_prefix);
                    let now = js_sys::Date::now().max(0.0) as u64;
                    let snapshot = self.ui.transcript_cache.try_update_value(|store| {
                        store.put(
                            &sid,
                            conv,
                            &merged,
                            now,
                            MAX_CONVERSATIONS,
                            MAX_PERSISTED_STORE_BYTES,
                        );
                        store.clone()
                    });
                    if let Some(store) = snapshot {
                        persist_transcript_cache(&store, Some((sid, conv)));
                    }
                    merged
                } else {
                    live_rows
                };
                self.ui
                    .messages
                    .set(chat_messages_from_projected(reconciled));
            }
            "p2.models" => {
                let view = decode_models_view(
                    self.state_val(P2_MODELS.name, indexes::models::STATE_MODELS),
                    self.state_val(P2_MODELS.name, indexes::models::STATE_DEFAULT_MODEL),
                    self.state_val(P2_MODELS.name, indexes::models::STATE_ENABLED_MODELS),
                    self.state_val(P2_MODELS.name, indexes::models::STATE_ERRORS),
                );
                self.ui.models.set(view);
            }
            "p2.settings" => {
                let view = decode_settings_view(
                    self.state_val(P2_SETTINGS.name, indexes::settings::STATE_SETTINGS),
                    self.state_val(P2_SETTINGS.name, indexes::settings::STATE_ERROR),
                );
                self.ui.settings.set(view);
            }
            "p2.mcp" => {
                let view = decode_mcp_view(
                    self.state_val(P2_MCP.name, indexes::mcp::STATE_INSTANCES),
                    self.state_val(P2_MCP.name, indexes::mcp::STATE_ERRORS),
                    self.state_val(P2_MCP.name, indexes::mcp::STATE_SIGN_INS),
                );
                self.ui.mcp.set(view);
            }
            "p2.sharing" => {
                let view = decode_sharing_view(
                    self.state_val(P2_SHARING.name, indexes::sharing::STATE_ENV),
                    self.state_val(P2_SHARING.name, indexes::sharing::STATE_LINKS),
                    self.state_val(P2_SHARING.name, indexes::sharing::STATE_GUESTS),
                    self.state_val(P2_SHARING.name, indexes::sharing::STATE_PEOPLE_URL),
                );
                self.ui.sharing.set(view);
            }
            "p2.session-status" => {
                let view = decode_session_status_view(self.state_val(
                    P2_SESSION_STATUS.name,
                    indexes::session_status::STATE_STATUSES,
                ));
                self.ui.session_status.set(view);
            }
            "p2.artifacts" if m == indexes::artifacts::STATE_ARTIFACTS => {
                let items = decode_artifacts_view(Some(value));
                self.ui.artifacts.update(|a| a.items = items);
            }
            "p2.voice-mode" | "p2.voice-service" => {
                let view = decode_voice_view(
                    self.state_val(P2_VOICE_MODE.name, indexes::voice_mode::STATE_ENABLED),
                    self.state_val(P2_VOICE_MODE.name, indexes::voice_mode::STATE_VOICE),
                    self.state_val(P2_VOICE_SERVICE.name, indexes::voice_service::STATE_VOICES),
                    self.state_val(P2_VOICE_SERVICE.name, indexes::voice_service::STATE_FAILURE),
                );
                self.ui.voice_rt.state.set(view.state);
                self.ui.voice.set(view);
            }
            "p2.provider-login" => {
                let view = decode_login_view(
                    self.state_val(
                        P2_PROVIDER_LOGIN.name,
                        extra_indexes::provider_login::STATE_PROVIDERS,
                    ),
                    self.state_val(
                        P2_PROVIDER_LOGIN.name,
                        extra_indexes::provider_login::STATE_LOGIN,
                    ),
                );
                self.ui.login.set(view);
            }
            "p2.devices" => {
                let mut view = decode_devices_view(
                    self.state_val(P2_DEVICES.name, extra_indexes::devices::STATE_CAN_MINT),
                );
                view.minted_token = self.ui.devices.get_untracked().minted_token;
                view.minted_url = self.ui.devices.get_untracked().minted_url;
                self.ui.devices.set(view);
            }
            "p2.discord" => {
                let view = decode_discord_view(
                    self.state_val(P2_DISCORD.name, extra_indexes::discord::STATE_BOTS),
                    self.state_val(P2_DISCORD.name, extra_indexes::discord::STATE_BINDINGS),
                );
                self.ui.discord.set(view);
            }
            "p2.envs" => {
                let view = decode_envs_view(
                    self.state_val(P2_ENVS.name, extra_indexes::envs::STATE_ENVS),
                    self.state_val(P2_ENVS.name, extra_indexes::envs::STATE_FAILURE),
                );
                self.ui.envs.set(view);
            }
            "p2.ports" => {
                let view = decode_ports_view(
                    self.state_val(P2_PORTS.name, extra_indexes::ports::STATE_FORWARDS),
                    self.state_val(P2_PORTS.name, extra_indexes::ports::STATE_RADIUS_ORIGIN),
                );
                self.ui.ports.set(view);
            }
            "p2.logs" => {
                let current_src = self.ui.logs.get_untracked().source;
                let src = if current_src.is_empty() {
                    "server".to_owned()
                } else {
                    current_src
                };
                let view = decode_logs_view(
                    &src,
                    self.state_val(P2_LOGS.name, extra_indexes::logs::STATE_SOURCES),
                    self.state_val(P2_LOGS.name, extra_indexes::logs::STATE_LINES),
                );
                self.ui.logs.set(view);
            }
            "p2.log-notices" => {
                let notices = decode_log_notices_view(self.state_val(
                    P2_LOG_NOTICES.name,
                    extra_indexes::log_notices::STATE_NOTICES,
                ));
                self.ui.log_notices.set(notices);
            }
            "p2.workspace" => {
                let changes = decode_workspace_changes(
                    self.state_val(P2_WORKSPACE.name, extra_indexes::workspace::STATE_CHANGES),
                );
                self.ui.workspace.update(|ws| ws.changes = changes);
            }
            "p2.views" => {
                let view = decode_views_state(
                    self.state_val(P2_VIEWS.name, extra_indexes::views::STATE_VIEWS),
                    self.state_val(P2_VIEWS.name, extra_indexes::views::STATE_ARRANGEMENT),
                    self.state_val(P2_VIEWS.name, extra_indexes::views::STATE_INLINE),
                    self.state_val(P2_VIEWS.name, extra_indexes::views::STATE_ANNOTATIONS),
                );
                self.ui.views_state.set(view);
            }
            "p2.subagents" => {
                let view = decode_subagents_view(
                    self.state_val(P2_SUBAGENTS.name, extra_indexes::subagents::STATE_CHILDREN),
                );
                self.ui.subagents.set(view);
            }
            "p2.chat-mode" => {
                let view = decode_chat_mode_view(
                    self.state_val(P2_CHAT_MODE.name, extra_indexes::chat_mode::STATE_ENABLED),
                    self.state_val(P2_CHAT_MODE.name, extra_indexes::chat_mode::STATE_NAME),
                    self.state_val(P2_CHAT_MODE.name, extra_indexes::chat_mode::STATE_WAITING),
                );
                self.ui.chat_mode.set(view);
            }
            "p2.familiar" => {
                let view = decode_familiar_view(
                    self.state_val(P2_FAMILIAR.name, extra_indexes::familiar::STATE_SESSION),
                );
                self.ui.familiar.set(view);
            }
            "p2.prompt-resources" => {
                let view = decode_prompt_resources_view(
                    self.state_val(
                        P2_PROMPT_RESOURCES.name,
                        extra_indexes::prompt_resources::STATE_SKILLS,
                    ),
                    self.state_val(
                        P2_PROMPT_RESOURCES.name,
                        extra_indexes::prompt_resources::STATE_TEMPLATES,
                    ),
                );
                self.ui.prompt_res.set(view);
            }
            "p2.projects" => {
                let view = decode_projects_view(
                    self.state_val(P2_PROJECTS.name, extra_indexes::projects::STATE_PROJECTS),
                    self.state_val(P2_PROJECTS.name, extra_indexes::projects::STATE_SESSIONS),
                );
                self.ui.projects.set(view);
            }
            "p2.sidebar-rollups" => {
                let rollups = decode_rollups_view(self.state_val(
                    P2_SIDEBAR_ROLLUPS.name,
                    extra_indexes::sidebar_rollups::STATE_ROLLUPS,
                ));
                self.ui.rollups.set(rollups);
            }
            "p2.session-metrics" => {
                let view = decode_session_metrics_view(
                    self.state_val(
                        P2_SESSION_METRICS.name,
                        extra_indexes::session_metrics::STATE_CLIENTS,
                    ),
                    self.state_val(
                        P2_SESSION_METRICS.name,
                        extra_indexes::session_metrics::STATE_USAGE,
                    ),
                );
                self.ui.session_metrics.set(view);
            }
            _ => {}
        }
    }

    fn handle_upload_reply(
        &mut self,
        call_id: u64,
        ok: bool,
        value: &Value,
        out: &mut Vec<Vec<u8>>,
    ) -> bool {
        let Some(rpc) = self.pending_uploads.remove(&call_id) else {
            return false;
        };
        if !ok {
            let reason = value.to_string();
            match rpc {
                UploadRpc::Begin { key }
                | UploadRpc::Append { key, .. }
                | UploadRpc::Finish { key } => {
                    if let Some(active) = self.active_uploads.remove(&key)
                        && let Some(token) = active.upload_token
                    {
                        self.next_call += 1;
                        out.push(encode_cancel_upload_call(
                            self.next_call,
                            active.hold_id,
                            &token,
                        ));
                        self.pending_uploads
                            .insert(self.next_call, UploadRpc::Cancel);
                    }
                    self.ui
                        .attachments
                        .update(|att| att.mark_failed(&key, reason));
                }
                UploadRpc::Cancel => {}
            }
            return true;
        }

        match rpc {
            UploadRpc::Begin { key } => match decode_begin_upload_result(value) {
                Ok((_, upload_token)) => {
                    let Some(active) = self.active_uploads.get_mut(&key) else {
                        return true;
                    };
                    if active.cancelled {
                        let hold_id = active.hold_id;
                        self.active_uploads.remove(&key);
                        self.next_call += 1;
                        out.push(encode_cancel_upload_call(
                            self.next_call,
                            hold_id,
                            &upload_token,
                        ));
                        self.pending_uploads
                            .insert(self.next_call, UploadRpc::Cancel);
                        return true;
                    }
                    active.upload_token = Some(upload_token.clone());
                    let hold_id = active.hold_id;
                    let total = active.total_bytes;
                    if let Some(chunk) = active.chunks.first() {
                        self.ui.attachments.update(|att| {
                            att.mark_uploading(&key, upload_token.clone(), 0, total);
                        });
                        self.next_call += 1;
                        out.push(encode_append_upload_call(
                            self.next_call,
                            hold_id,
                            &upload_token,
                            chunk.offset,
                            &chunk.bytes,
                        ));
                        self.pending_uploads
                            .insert(self.next_call, UploadRpc::Append { key, next_index: 1 });
                    } else {
                        self.ui.attachments.update(|att| att.mark_finishing(&key));
                        self.next_call += 1;
                        out.push(encode_finish_upload_call(
                            self.next_call,
                            hold_id,
                            &upload_token,
                        ));
                        self.pending_uploads
                            .insert(self.next_call, UploadRpc::Finish { key });
                    }
                }
                Err(err) => {
                    self.active_uploads.remove(&key);
                    self.ui.attachments.update(|att| att.mark_failed(&key, err));
                }
            },
            UploadRpc::Append { key, next_index } => {
                let Some(active) = self.active_uploads.get(&key) else {
                    return true;
                };
                let Some(upload_token) = active.upload_token.clone() else {
                    return true;
                };
                let hold_id = active.hold_id;
                if active.cancelled {
                    self.active_uploads.remove(&key);
                    self.next_call += 1;
                    out.push(encode_cancel_upload_call(
                        self.next_call,
                        hold_id,
                        &upload_token,
                    ));
                    self.pending_uploads
                        .insert(self.next_call, UploadRpc::Cancel);
                    return true;
                }
                let sent = active
                    .chunks
                    .get(next_index.saturating_sub(1))
                    .map_or(0, |c| c.offset + c.bytes.len() as u64);
                let total = active.total_bytes;
                if let Some(chunk) = active.chunks.get(next_index) {
                    self.ui.attachments.update(|att| {
                        att.mark_uploading(&key, upload_token.clone(), sent, total);
                    });
                    self.next_call += 1;
                    out.push(encode_append_upload_call(
                        self.next_call,
                        hold_id,
                        &upload_token,
                        chunk.offset,
                        &chunk.bytes,
                    ));
                    self.pending_uploads.insert(
                        self.next_call,
                        UploadRpc::Append {
                            key,
                            next_index: next_index + 1,
                        },
                    );
                } else {
                    self.ui.attachments.update(|att| att.mark_finishing(&key));
                    self.next_call += 1;
                    out.push(encode_finish_upload_call(
                        self.next_call,
                        hold_id,
                        &upload_token,
                    ));
                    self.pending_uploads
                        .insert(self.next_call, UploadRpc::Finish { key });
                }
            }
            UploadRpc::Finish { key } => {
                self.active_uploads.remove(&key);
                match decode_finish_upload_result(value) {
                    Ok(uploaded) => {
                        let new_id = uploaded.id.clone();
                        self.ui.draft.update(|d| {
                            *d = rewrite_attachment_reference(d, &key, &new_id);
                        });
                        self.ui
                            .attachments
                            .update(|att| att.mark_uploaded(&key, uploaded));
                    }
                    Err(err) => {
                        self.ui.attachments.update(|att| att.mark_failed(&key, err));
                    }
                }
            }
            UploadRpc::Cancel => {}
        }
        true
    }

    fn frame(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut r = Reader(bytes);
        let kind = r.take(1)?[0];
        if !self.greeted && kind != 0 {
            return Err("expected Chord hello".into());
        }
        let mut out = vec![];
        match kind {
            0 => {
                if self.greeted || r.number()? != 1 {
                    return Err("incompatible Chord hello".into());
                }
                r.take(16)?;
                r.number()?;
                self.greeted = true;
                self.ui.status.set("Connected".into());
                self.hold_contracts.insert(1, P2_ACCESS.name);
                self.contract_holds.insert(P2_ACCESS.name, 1);
                self.tracker.register(1, P2_ACCESS.name, true);
                self.hold_contracts.insert(2, P2_SESSIONS.name);
                self.contract_holds.insert(P2_SESSIONS.name, 2);
                self.tracker.register(2, P2_SESSIONS.name, true);
                self.next_hold = 3;
                out.extend([wire::hold(1, None), wire::hold(2, None)]);
                for spec in [
                    &P2_SESSION_STATUS,
                    &P2_VOICE_SERVICE,
                    &P2_MODELS,
                    &P2_SETTINGS,
                    &P2_MCP,
                    &P2_LOG_NOTICES,
                    &P2_PROJECTS,
                    &P2_SIDEBAR_ROLLUPS,
                    &P2_SEARCH,
                ] {
                    self.hold_contract(spec, false, None, None, &mut out);
                }
            }
            5 | 6 => {
                for _ in 0..r.number()? {
                    let h = r.number()?;
                    if kind == 5 {
                        let variant = r.number()?;
                        self.members.retain(|(id, _), _| *id != h);
                        self.tracker.mark_live(h);
                        if let Some(&contract) = self.hold_contracts.get(&h) {
                            self.ui.plugin_errors.update(|errs| {
                                errs.remove(contract);
                            });
                            if contract == P2_ACCESS.name {
                                self.ui.guest.set(variant == 0);
                                self.ui.status.set("Connected".into());
                            } else if contract == P2_SESSIONS.name {
                                self.catalog_member = if variant == indexes::sessions::VARIANT_OWNER
                                {
                                    indexes::sessions::STATE_OWNER_SESSIONS
                                } else {
                                    indexes::sessions::STATE_GUEST_SESSIONS
                                };
                                self.ui
                                    .create
                                    .set(variant == indexes::sessions::VARIANT_OWNER);
                                self.ui.status.set("Connected".into());
                            } else if h == self.chat {
                                self.ui.write.set(
                                    variant == indexes::chat::VARIANT_PARTICIPANT
                                        && self.ui.selected.get_untracked().is_some(),
                                );
                                self.ui.status.set("Connected".into());
                            }
                        }
                    }
                    for _ in 0..r.number()? {
                        let m = r.number()?;
                        let v = r.part()?;
                        if self.hold_contracts.contains_key(&h)
                            && (h != self.chat || self.ui.selected.get_untracked().is_some())
                        {
                            if kind == 5 {
                                self.members.insert(
                                    (h, m),
                                    crate::chord_bridge::replica::Replica::snapshot(v),
                                );
                            } else {
                                self.members.entry((h, m)).or_default().apply(v)?;
                            }
                            self.publish(h, m, &mut out);
                        }
                    }
                    let flags = r.number()?;
                    for _ in 0..if kind == 5 { flags } else { flags / 2 } {
                        let m = r.number()?;
                        let e = r.json()?;
                        self.members.remove(&(h, m));
                        if matches!(
                            self.hold_contracts.get(&h).copied(),
                            Some("p2.access" | "p2.sessions" | "p2.chat")
                        ) {
                            self.ui.status.set(e.to_string());
                        }
                    }
                    if kind == 6 && flags % 2 == 1 {
                        for _ in 0..r.number()? {
                            r.number()?;
                        }
                    }
                }
                out.push(vec![13]);
            }
            8 | 9 => {
                let id = r.number()?;
                let value = if kind == 9 {
                    r.json()?
                } else {
                    match r.number()? {
                        0 => Value::Null,
                        1 => r.part()?,
                        _ => return Err("unexpected Chat result segments".into()),
                    }
                };
                if self.handle_upload_reply(id, kind == 8, &value, &mut out) {
                    // handled upload call
                } else if self
                    .pending_primary
                    .as_ref()
                    .is_some_and(|(call, _)| *call == id)
                    && let Some((_, cmd)) = self.pending_primary.take()
                {
                    self.ui.busy.set(false);
                    match cmd {
                        Command::Send(text) => {
                            self.ui.last_submission.set_value(None);
                            if kind == 8 && self.ui.draft.get_untracked() == text {
                                self.ui.draft.set(String::new());
                                self.ui.attachments.update(ComposerAttachments::clear);
                                session_remove(DRAFT_STORAGE_KEY);
                            }
                        }
                        Command::Create | Command::CreateWith { .. } if kind == 8 => {
                            self.ui.route_new.set(false);
                            self.ui.route_page.set(None);
                            if let Some(id) = value["id"].as_str() {
                                self.select(
                                    id.into(),
                                    value["main"].as_u64().unwrap_or(1),
                                    &mut out,
                                );
                            }
                        }
                        _ => {}
                    }
                    self.ui.status.set(if kind == 9 {
                        value.to_string()
                    } else {
                        "Connected".into()
                    });
                } else if let Some(cmd) = self.pending_plugin.remove(&id) {
                    self.handle_plugin_reply(cmd, kind == 8, &value, &mut out);
                }
            }
            7 | 10 | 12 => {
                let h = r.number()?;
                if kind == 7 {
                    r.take(1)?;
                }
                let reason = if kind == 12 {
                    "Loading".into()
                } else {
                    r.json()?.to_string()
                };
                if kind == 7 {
                    r.number()?;
                }
                if let Some(core_err) = self.tracker.mark_failed(h, reason.clone()) {
                    if h == self.chat {
                        self.ui.write.set(false);
                    }
                    self.ui.status.set(core_err);
                } else if let Some(&contract) = self.hold_contracts.get(&h) {
                    self.ui.plugin_errors.update(|errs| {
                        errs.insert(contract.to_owned(), reason);
                    });
                }
            }
            15 => out.push(vec![16]),
            16 => {}
            _ => return Err(format!("unexpected Chord frame {kind}")),
        }
        if !r.0.is_empty() {
            return Err("trailing Chord data".into());
        }
        Ok(out)
    }

    fn handle_plugin_reply(
        &mut self,
        cmd: Command,
        ok: bool,
        value: &Value,
        out: &mut Vec<Vec<u8>>,
    ) {
        if !ok {
            if let Some(contract) = match cmd {
                Command::RefreshCosts => Some(P2_CHAT_COSTS.name),
                Command::ReadArtifact(_) | Command::RemoveArtifact(_) => Some(P2_ARTIFACTS.name),
                Command::RefreshTree
                | Command::SelectBranch(_)
                | Command::TreeNavigate(_)
                | Command::TreeBranch(_)
                | Command::TreeRename(_, _) => Some(P2_TREE.name),
                Command::OpenTerminal | Command::TerminalRequestChannel => Some(P2_TERMINALS.name),
                Command::ToggleVoice | Command::ChooseVoice(_, _, _) => Some(P2_VOICE_MODE.name),
                Command::UpdateSetting(_, _) | Command::UnsetSetting(_) => Some(P2_SETTINGS.name),
                Command::SetDefaultModel(_, _, _) | Command::SetEnabledModels(_) => {
                    Some(P2_MODELS.name)
                }
                Command::McpRestart(_)
                | Command::McpSignIn(_)
                | Command::McpSignOut(_)
                | Command::McpPasteRedirect(_, _)
                | Command::McpCancelSignIn(_) => Some(P2_MCP.name),
                Command::ShareCreateLink(_, _)
                | Command::ShareEmail(_, _)
                | Command::ShareRemoveGuest(_) => Some(P2_SHARING.name),
                _ => None,
            } {
                self.ui.plugin_errors.update(|errs| {
                    errs.insert(contract.to_owned(), value.to_string());
                });
            }
            return;
        }
        match cmd {
            Command::RefreshCosts => {
                self.ui.costs.set(decode_costs_view(value));
            }
            Command::ReadArtifact(_) => {
                if let Some((aid, content)) = decode_artifact_content(value) {
                    self.ui.artifacts.update(|a| {
                        a.selected_id = Some(aid);
                        a.selected_content = Some(content);
                    });
                }
            }
            Command::RefreshTree
            | Command::SelectBranch(_)
            | Command::TreeNavigate(_)
            | Command::TreeBranch(_)
            | Command::TreeRename(_, _) => {
                if value.is_object() {
                    self.ui.tree.set(decode_tree_view(value));
                }
            }
            Command::TerminalRequestChannel => {
                if let Some(tok) = value["token"].as_str() {
                    let cmd_tx = self.ui.commands.get_value();
                    spawn_terminal_channel(self.ui.terminal_rt, tok.to_owned(), move |n| {
                        if let Some(ref tx) = cmd_tx {
                            let _ = tx.unbounded_send(Command::TerminalAck(n));
                        }
                    });
                }
            }
            Command::OpenTerminal => {
                let list = decode_terminals_list(value);
                self.ui.terminals.update(|t| {
                    t.available = list.clone();
                    t.status = crate::ui::terminal::TerminalStatus::Live;
                });
                if let Some(first) = list.first().cloned()
                    && self.ui.terminal_rt.active_id.get_untracked().is_none()
                    && let Some(&h) = self.contract_holds.get(P2_TERMINALS.name)
                {
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_ATTACH,
                        json!({ "terminalId": first }),
                        Command::TerminalAttach(first),
                        out,
                    );
                }
            }
            Command::MintDeviceToken => {
                self.ui.devices.update(|d| {
                    d.minted_token = value["token"].as_str().map(str::to_owned);
                    d.minted_url = value["url"].as_str().map(str::to_owned);
                });
            }
            Command::FindTraces(_) => {
                let list = decode_traces_result(value);
                self.ui.traces.update(|t| t.traces = list);
            }
            Command::GetTrace(_) => {
                let formatted = serde_json::to_string_pretty(value).unwrap_or_default();
                self.ui
                    .traces
                    .update(|t| t.selected_trace = Some(formatted));
            }
            Command::ListWorkspaceDir(_) => {
                let entries: Vec<(String, bool)> = value["entries"]
                    .as_array()
                    .or_else(|| value.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        Some((
                            e["path"]
                                .as_str()
                                .or_else(|| e["name"].as_str())?
                                .to_owned(),
                            e["isDir"].as_bool().unwrap_or(false),
                        ))
                    })
                    .collect();
                self.ui.workspace.update(|ws| ws.files = entries);
            }
            Command::CompleteFiles(prefix) => {
                let (_, _, is_quoted) = parse_path_prefix(&prefix);
                let results: Vec<(String, bool)> = value["items"]
                    .as_array()
                    .or_else(|| value.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        Some((
                            e["path"].as_str()?.to_owned(),
                            e["isDir"].as_bool().unwrap_or(false),
                        ))
                    })
                    .collect();
                let items = at_items(&results, is_quoted);
                if items.is_empty() {
                    self.ui.completion.set(None);
                } else {
                    self.ui.completion.set(Some(Popover {
                        items,
                        prefix,
                        selected: 0,
                        hint: Some("Tab or Enter to insert".into()),
                    }));
                }
            }
            Command::ReadWorkspaceFile(path) => {
                let content = value["content"]
                    .as_str()
                    .or_else(|| value.as_str())
                    .unwrap_or("")
                    .to_owned();
                self.ui.workspace.update(|ws| {
                    ws.selected_path = Some(path);
                    ws.selected_content = Some(content);
                    ws.selected_before = None;
                });
            }
            Command::ExpandPrompt(_) => {
                if let Some(expanded) = value["text"].as_str().or_else(|| value.as_str()) {
                    self.ui.draft.set(expanded.to_owned());
                }
            }
            Command::SearchSessions(_, _) => {
                self.ui.search_hits.set(decode_search_results(value));
            }
            Command::EnvGenerateKey(_) | Command::EnvTest(_) => {
                self.ui
                    .envs
                    .update(|e| e.last_result = Some(value.to_string()));
            }
            _ => {}
        }
    }

    fn command(&mut self, cmd: Command) -> Result<Vec<Vec<u8>>> {
        let mut out = vec![];
        match cmd {
            Command::Open(id, c) => {
                if self.pending_primary.is_none() {
                    self.ui.route_new.set(false);
                    self.ui.route_page.set(None);
                    self.select(id, c, &mut out);
                }
                return Ok(out);
            }
            Command::Navigate(route, push) => {
                self.ui.route_new.set(route.new_session);
                self.ui.route_page.set(route.page.clone());
                if let Some(panel_id) = route.panel.as_deref() {
                    if let Some(p) = SidePanel::from_id(panel_id) {
                        self.ui.custom_panel.set(None);
                        self.ui.panel.set(p);
                        self.ensure_plugin_hold_for_panel(p, &mut out);
                    } else {
                        self.ui.panel.set(SidePanel::None);
                        self.ui.custom_panel.set(Some(panel_id.to_owned()));
                    }
                }
                if let Some(sid) = route.session.clone() {
                    self.select(sid, route.conversation, &mut out);
                } else {
                    sync_browser_history(self.ui, route, push);
                }
                return Ok(out);
            }
            Command::OpenPanel(panel) => {
                self.ui.custom_panel.set(None);
                self.ui.panel.set(panel);
                if panel.is_open() {
                    self.ensure_plugin_hold_for_panel(panel, &mut out);
                }
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let mut r = Route::session_conversation(sid, conv);
                    r.panel = panel.is_open().then(|| panel.id().to_owned());
                    sync_browser_history(self.ui, r, false);
                }
                return Ok(out);
            }
            Command::OpenCustomPanel(id) => {
                self.ui.panel.set(SidePanel::None);
                self.ui.custom_panel.set(Some(id));
                return Ok(out);
            }
            Command::OpenPane(pane) => {
                self.ui.pane.set(pane);
                if pane == MainPane::Terminal
                    && let Some((sid, _)) = self.ui.selected.get_untracked()
                {
                    let h = self.hold_contract(&P2_TERMINALS, false, Some(&sid), None, &mut out);
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_CHANNEL,
                        json!({}),
                        Command::TerminalRequestChannel,
                        &mut out,
                    );
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_LIST,
                        json!({}),
                        Command::OpenTerminal,
                        &mut out,
                    );
                } else if pane == MainPane::Workspace
                    && let Some((sid, conv)) = self.ui.selected.get_untracked()
                {
                    self.hold_contract(&P2_WORKSPACE, false, Some(&sid), Some(conv), &mut out);
                    if let Some(&fh) = self.contract_holds.get(P2_FILE_INDEX.name) {
                        self.send_plugin_call(
                            fh,
                            extra_indexes::file_index::METHOD_LIST,
                            json!({ "path": "" }),
                            Command::ListWorkspaceDir(String::new()),
                            &mut out,
                        );
                    }
                }
                return Ok(out);
            }
            Command::Stop => {
                if self.chat >= 3 {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        self.chat,
                        indexes::chat::METHOD_STOP,
                        json!({}),
                    ));
                    self.ui.streaming.set(false);
                }
                return Ok(out);
            }
            Command::UpdateSetting(key, raw_val) => {
                if let Some(&h) = self.contract_holds.get(P2_SETTINGS.name) {
                    let parsed = serde_json::from_str::<Value>(&raw_val)
                        .unwrap_or(Value::String(raw_val.clone()));
                    let mut set_map = serde_json::Map::new();
                    set_map.insert(key.clone(), parsed);
                    self.send_plugin_call(
                        h,
                        indexes::settings::METHOD_UPDATE,
                        json!({ "set": Value::Object(set_map), "unset": [] }),
                        Command::UpdateSetting(key, raw_val),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::UnsetSetting(key) => {
                if let Some(&h) = self.contract_holds.get(P2_SETTINGS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::settings::METHOD_UPDATE,
                        json!({ "set": {}, "unset": [key.clone()] }),
                        Command::UnsetSetting(key),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SetDefaultModel(provider, model_id, thinking_level) => {
                if let Some(&h) = self.contract_holds.get(P2_MODELS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::models::METHOD_SET_DEFAULT,
                        json!({
                            "provider": provider,
                            "modelId": model_id,
                            "thinkingLevel": thinking_level
                        }),
                        Command::SetDefaultModel(provider, model_id, thinking_level),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SetSessionModel(provider, model_id, thinking_level) => {
                if self.chat >= 3 {
                    self.send_plugin_call(
                        self.chat,
                        indexes::chat::METHOD_CONFIGURE,
                        json!({
                            "provider": provider,
                            "modelId": model_id,
                            "thinkingLevel": thinking_level
                        }),
                        Command::SetSessionModel(provider, model_id, thinking_level),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SetEnabledModels(patterns) => {
                if let Some(&h) = self.contract_holds.get(P2_MODELS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::models::METHOD_SET_ENABLED_MODELS,
                        json!({ "patterns": patterns }),
                        Command::SetEnabledModels(patterns),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::McpRestart(instance) => {
                if let Some(&h) = self.contract_holds.get(P2_MCP.name) {
                    self.send_plugin_call(
                        h,
                        indexes::mcp::METHOD_RESTART,
                        json!({ "instance": instance }),
                        Command::McpRestart(instance),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::McpSignIn(server) => {
                if let Some(&h) = self.contract_holds.get(P2_MCP.name) {
                    self.send_plugin_call(
                        h,
                        indexes::mcp::METHOD_SIGN_IN,
                        json!({ "server": server }),
                        Command::McpSignIn(server),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::McpSignOut(server) => {
                if let Some(&h) = self.contract_holds.get(P2_MCP.name) {
                    self.send_plugin_call(
                        h,
                        indexes::mcp::METHOD_SIGN_OUT,
                        json!({ "server": server }),
                        Command::McpSignOut(server),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::McpPasteRedirect(id, redirect_url) => {
                if let Some(&h) = self.contract_holds.get(P2_MCP.name) {
                    self.send_plugin_call(
                        h,
                        indexes::mcp::METHOD_PASTE,
                        json!({ "id": id, "redirectUrl": redirect_url }),
                        Command::McpPasteRedirect(id, redirect_url),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::McpCancelSignIn(id) => {
                if let Some(&h) = self.contract_holds.get(P2_MCP.name) {
                    self.send_plugin_call(
                        h,
                        indexes::mcp::METHOD_CANCEL_SIGN_IN,
                        json!({ "id": id }),
                        Command::McpCancelSignIn(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ShareCreateLink(mode, reset) => {
                if let Some(&h) = self.contract_holds.get(P2_SHARING.name) {
                    self.send_plugin_call(
                        h,
                        indexes::sharing::METHOD_LINK,
                        json!({ "mode": mode, "reset": reset, "remoteConfirmed": true }),
                        Command::ShareCreateLink(mode, reset),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ShareEmail(email, mode) => {
                if let Some(&h) = self.contract_holds.get(P2_SHARING.name) {
                    self.send_plugin_call(
                        h,
                        indexes::sharing::METHOD_SHARE,
                        json!({ "email": email, "mode": mode, "remoteConfirmed": true }),
                        Command::ShareEmail(email, mode),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ShareRemoveGuest(guest_id) => {
                if let Some(&h) = self.contract_holds.get(P2_SHARING.name) {
                    self.send_plugin_call(
                        h,
                        indexes::sharing::METHOD_REMOVE,
                        json!({ "guestId": guest_id }),
                        Command::ShareRemoveGuest(guest_id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ReadArtifact(aid) => {
                if let Some(&h) = self.contract_holds.get(P2_ARTIFACTS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::artifacts::METHOD_READ,
                        json!({ "id": aid }),
                        Command::ReadArtifact(aid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::RemoveArtifact(aid) => {
                if let Some(&h) = self.contract_holds.get(P2_ARTIFACTS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::artifacts::METHOD_REMOVE,
                        json!({ "id": aid }),
                        Command::RemoveArtifact(aid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::RefreshCosts => {
                if let Some(&h) = self.contract_holds.get(P2_CHAT_COSTS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::chat_costs::METHOD_COSTS,
                        json!(null),
                        Command::RefreshCosts,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::RefreshTree => {
                if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                    self.send_plugin_call(
                        h,
                        indexes::tree::METHOD_TREE,
                        json!(null),
                        Command::RefreshTree,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SelectBranch(conv) => {
                if let Some((sid, _)) = self.ui.selected.get_untracked() {
                    if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                        self.send_plugin_call(
                            h,
                            indexes::tree::METHOD_SELECT,
                            json!({ "conversation": conv }),
                            Command::SelectBranch(conv),
                            &mut out,
                        );
                    }
                    self.select(sid, conv, &mut out);
                }
                return Ok(out);
            }
            Command::TreeNavigate(entry) => {
                if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                    self.send_plugin_call(
                        h,
                        indexes::tree::METHOD_NAVIGATE,
                        json!({ "entry": entry }),
                        Command::TreeNavigate(entry),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TreeBranch(entry) => {
                if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                    self.send_plugin_call(
                        h,
                        indexes::tree::METHOD_BRANCH,
                        json!({ "entry": entry }),
                        Command::TreeBranch(entry),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TreeRename(conv, label) => {
                if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                    self.send_plugin_call(
                        h,
                        indexes::tree::METHOD_RENAME,
                        json!({ "conversation": conv, "label": label }),
                        Command::TreeRename(conv, label),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::OpenTerminal => {
                self.ui.pane.set(MainPane::Terminal);
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let h = self.hold_contract(&P2_TERMINALS, false, Some(&sid), None, &mut out);
                    let cols = self.ui.terminal_rt.cols.get_untracked();
                    let rows = self.ui.terminal_rt.rows.get_untracked();
                    if self.ui.terminal_rt.tx.get_value().is_none() {
                        self.send_plugin_call(
                            h,
                            indexes::terminals::METHOD_CHANNEL,
                            json!({}),
                            Command::TerminalRequestChannel,
                            &mut out,
                        );
                    }
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_OPEN,
                        json!({ "conversation": conv, "cols": cols, "rows": rows }),
                        Command::OpenTerminal,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TerminalRequestChannel => {
                if let Some(&h) = self.contract_holds.get(P2_TERMINALS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_CHANNEL,
                        json!({}),
                        Command::TerminalRequestChannel,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TerminalAttach(tid) => {
                if let Some(&h) = self.contract_holds.get(P2_TERMINALS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_ATTACH,
                        json!({ "terminalId": tid }),
                        Command::TerminalAttach(tid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TerminalClose(tid) => {
                if let Some(&h) = self.contract_holds.get(P2_TERMINALS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_CLOSE,
                        json!({ "terminalId": tid, "force": true }),
                        Command::TerminalClose(tid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TerminalResize(cols, rows) => {
                if let Some(&h) = self.contract_holds.get(P2_TERMINALS.name) {
                    self.send_plugin_call(
                        h,
                        indexes::terminals::METHOD_RESIZE,
                        json!({ "cols": cols, "rows": rows }),
                        Command::TerminalResize(cols, rows),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::TerminalAck(bytes) => {
                if let Some(&h) = self.contract_holds.get(P2_TERMINALS.name) {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::terminals::METHOD_ACK,
                        json!({ "bytes": bytes }),
                    ));
                }
                return Ok(out);
            }
            Command::ToggleVoice => {
                let next_enabled = !self.ui.voice.get_untracked().enabled;
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let h =
                        self.hold_contract(&P2_VOICE_MODE, false, Some(&sid), Some(conv), &mut out);
                    self.send_plugin_call(
                        h,
                        indexes::voice_mode::METHOD_SET,
                        json!({ "enabled": next_enabled }),
                        Command::ToggleVoice,
                        &mut out,
                    );
                    if next_enabled {
                        start_voice_session(self.ui.voice_rt, sid, conv);
                    } else {
                        self.ui.voice_rt.stop();
                    }
                }
                return Ok(out);
            }
            Command::ChooseVoice(engine, voice, speed) => {
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let h =
                        self.hold_contract(&P2_VOICE_MODE, false, Some(&sid), Some(conv), &mut out);
                    self.send_plugin_call(
                        h,
                        indexes::voice_mode::METHOD_CHOOSE,
                        json!({ "engine": engine, "voice": voice, "speed": speed }),
                        Command::ChooseVoice(engine, voice, speed),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginStart(provider_id, auth_type) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_START,
                        json!({ "providerId": provider_id, "authType": auth_type }),
                        Command::LoginStart(provider_id, auth_type),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginAnswer(attempt_id, prompt_id, value) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_ANSWER,
                        json!({ "attemptId": attempt_id, "promptId": prompt_id, "value": value }),
                        Command::LoginAnswer(attempt_id, prompt_id, value),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginCancel(id) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_CANCEL,
                        json!({ "attemptId": id }),
                        Command::LoginCancel(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginDismiss(id) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_DISMISS,
                        json!({ "attemptId": id }),
                        Command::LoginDismiss(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginLogout(pid) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_LOGOUT,
                        json!({ "providerId": pid }),
                        Command::LoginLogout(pid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoginRefresh(pid) => {
                if let Some(&h) = self.contract_holds.get(P2_PROVIDER_LOGIN.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::provider_login::METHOD_REFRESH,
                        json!({ "providerId": pid }),
                        Command::LoginRefresh(pid),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::MintDeviceToken => {
                if let Some(&h) = self.contract_holds.get(P2_DEVICES.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::devices::METHOD_TOKEN,
                        json!({}),
                        Command::MintDeviceToken,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::DiscordAddBot(token) => {
                if let Some(&h) = self.contract_holds.get(P2_DISCORD.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::discord::METHOD_ADD_BOT,
                        json!({ "token": token }),
                        Command::DiscordAddBot(token),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::DiscordRemoveBot(bot) => {
                if let Some(&h) = self.contract_holds.get(P2_DISCORD.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::discord::METHOD_REMOVE_BOT,
                        json!({ "bot": bot }),
                        Command::DiscordRemoveBot(bot),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::DiscordLink(session, bot, guild, channel) => {
                if let Some(&h) = self.contract_holds.get(P2_DISCORD.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::discord::METHOD_LINK,
                        json!({
                            "session": session,
                            "bot": bot,
                            "guild": guild,
                            "channel": channel,
                            "lock": false,
                            "sandbox": false,
                            "wake": "mentions"
                        }),
                        Command::DiscordLink(session, bot, guild, channel),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::DiscordUnlink(session) => {
                if let Some(&h) = self.contract_holds.get(P2_DISCORD.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::discord::METHOD_UNLINK,
                        json!({ "session": session }),
                        Command::DiscordUnlink(session),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvSave(name, host) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_SAVE,
                        json!({ "name": name, "config": { "host": host } }),
                        Command::EnvSave(name, host),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvTest(name) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_TEST,
                        json!({ "name": name }),
                        Command::EnvTest(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvDelete(name) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_DELETE,
                        json!({ "name": name }),
                        Command::EnvDelete(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvGenerateKey(name) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_GENERATE_KEY,
                        json!({ "name": name }),
                        Command::EnvGenerateKey(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvSandboxStart(name) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_SANDBOX_START,
                        json!({ "name": name }),
                        Command::EnvSandboxStart(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::EnvSandboxStop(name) => {
                if let Some(&h) = self.contract_holds.get(P2_ENVS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::envs::METHOD_SANDBOX_STOP,
                        json!({ "name": name }),
                        Command::EnvSandboxStop(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::PortAdd(port, name, public) => {
                if let Some(&h) = self.contract_holds.get(P2_PORTS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::ports::METHOD_ADD,
                        json!({ "port": port, "name": name, "public": public }),
                        Command::PortAdd(port, name, public),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::PortSetPublic(id, public) => {
                if let Some(&h) = self.contract_holds.get(P2_PORTS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::ports::METHOD_SET_PUBLIC,
                        json!({ "id": id, "public": public }),
                        Command::PortSetPublic(id, public),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::PortRemove(id) => {
                if let Some(&h) = self.contract_holds.get(P2_PORTS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::ports::METHOD_REMOVE,
                        json!({ "id": id }),
                        Command::PortRemove(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SelectLogSource(src) => {
                self.release_contract(P2_LOGS.name, &mut out);
                let id = self.next_hold;
                self.next_hold += 1;
                self.hold_contracts.insert(id, P2_LOGS.name);
                self.contract_holds.insert(P2_LOGS.name, id);
                self.tracker.register(id, P2_LOGS.name, false);
                out.push(hold_for_contract(id, &P2_LOGS, None, None, Some(&src)));
                self.ui.logs.update(|l| l.source = src);
                return Ok(out);
            }
            Command::DismissNotice(id) => {
                let _ = local_set(&dismissed_notice_key(&id), "1");
                self.ui.dismissed_notices.update(|s| {
                    s.insert(id.clone());
                });
                if let Some(&h) = self.contract_holds.get(P2_LOG_NOTICES.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::log_notices::METHOD_DISMISS,
                        json!({ "id": id }),
                        Command::DismissNotice(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::FindTraces(name) => {
                if let Some(&h) = self.contract_holds.get(P2_TRACES.name) {
                    let args = if name.trim().is_empty() {
                        json!({ "limit": 25 })
                    } else {
                        json!({ "name": name, "limit": 25 })
                    };
                    self.send_plugin_call(
                        h,
                        extra_indexes::traces::METHOD_FIND_TRACES,
                        args,
                        Command::FindTraces(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::GetTrace(trace_id) => {
                if let Some(&h) = self.contract_holds.get(P2_TRACES.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::traces::METHOD_GET_TRACE,
                        json!({ "traceId": trace_id }),
                        Command::GetTrace(trace_id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::QueryWorkspace => {
                if let Some(&h) = self.contract_holds.get(P2_WORKSPACE.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::workspace::METHOD_QUERY,
                        json!({}),
                        Command::QueryWorkspace,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::CompleteFiles(prefix) => {
                let (raw, _, _) = parse_path_prefix(&prefix);
                if !out_of_scope(&raw)
                    && let Some(&h) = self.contract_holds.get(P2_FILE_INDEX.name)
                {
                    self.send_plugin_call(
                        h,
                        extra_indexes::file_index::METHOD_COMPLETE,
                        json!({ "query": raw, "limit": 12 }),
                        Command::CompleteFiles(prefix),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ListWorkspaceDir(path) => {
                if let Some(&h) = self.contract_holds.get(P2_FILE_INDEX.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::file_index::METHOD_LIST,
                        json!({ "path": path }),
                        Command::ListWorkspaceDir(path),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ReadWorkspaceFile(path) => {
                if let Some((sid, _)) = self.ui.selected.get_untracked() {
                    let h =
                        self.hold_contract(&P2_SESSION_FILES, false, Some(&sid), None, &mut out);
                    self.send_plugin_call(
                        h,
                        indexes::session_files::METHOD_READ,
                        json!({ "path": path }),
                        Command::ReadWorkspaceFile(path),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ViewsOpen => {
                if let Some(&h) = self.contract_holds.get(P2_VIEWS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::views::METHOD_OPEN,
                        json!({}),
                        Command::ViewsOpen,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ViewsSelect(id, selection) => {
                if let Some(&h) = self.contract_holds.get(P2_VIEWS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::views::METHOD_SELECT,
                        json!({ "id": id, "selection": selection }),
                        Command::ViewsSelect(id, selection),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ViewsClose(id) => {
                if let Some(&h) = self.contract_holds.get(P2_VIEWS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::views::METHOD_CLOSE,
                        json!({ "id": id }),
                        Command::ViewsClose(id),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ViewsUnannotate(ids) => {
                if let Some(&h) = self.contract_holds.get(P2_VIEWS.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::views::METHOD_UNANNOTATE,
                        json!({ "ids": ids }),
                        Command::ViewsUnannotate(ids),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::StageScopePrompt(block, note) => {
                if let Some(&h) = self.contract_holds.get(P2_SCOPE_STAGE.name) {
                    let req_id = uuid().unwrap_or_else(|_| "req-1".into());
                    self.send_plugin_call(
                        h,
                        extra_indexes::scope_stage::METHOD_STAGE,
                        json!({ "block": block, "note": note, "requestId": req_id }),
                        Command::StageScopePrompt(block, note),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ChatModeSet(enabled) => {
                if let Some(&h) = self.contract_holds.get(P2_CHAT_MODE.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::chat_mode::METHOD_SET,
                        json!({ "enabled": enabled }),
                        Command::ChatModeSet(enabled),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ChatModeRename(name) => {
                if let Some(&h) = self.contract_holds.get(P2_CHAT_MODE.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::chat_mode::METHOD_RENAME,
                        json!({ "name": name }),
                        Command::ChatModeRename(name),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::OpenFamiliar => {
                if let Some(&h) = self.contract_holds.get(P2_FAMILIAR.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::familiar::METHOD_OPEN,
                        json!({}),
                        Command::OpenFamiliar,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::ExpandPrompt(text) => {
                if let Some(&h) = self.contract_holds.get(P2_PROMPT_RESOURCES.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::prompt_resources::METHOD_EXPAND,
                        json!({ "text": text }),
                        Command::ExpandPrompt(text),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::LoadEarlierEntries => {
                if self.chat >= 3 {
                    self.send_plugin_call(
                        self.chat,
                        indexes::chat::METHOD_ENTRIES,
                        json!({ "limit": 50 }),
                        Command::LoadEarlierEntries,
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SearchSessions(query, archived) => {
                if let Some(&h) = self.contract_holds.get(P2_SEARCH.name) {
                    self.send_plugin_call(
                        h,
                        extra_indexes::search::METHOD_SEARCH,
                        json!({ "query": query, "archived": archived }),
                        Command::SearchSessions(query, archived),
                        &mut out,
                    );
                }
                return Ok(out);
            }
            Command::SessionPin(id, pinned) => {
                self.send_plugin_call(
                    2,
                    indexes::sessions::METHOD_PIN,
                    json!({ "id": id, "pinned": pinned }),
                    Command::SessionPin(id, pinned),
                    &mut out,
                );
                return Ok(out);
            }
            Command::SessionRename(id, title) => {
                self.send_plugin_call(
                    2,
                    indexes::sessions::METHOD_RENAME,
                    json!({ "id": id, "title": title }),
                    Command::SessionRename(id, title),
                    &mut out,
                );
                return Ok(out);
            }
            Command::SessionArchive(id, archived) => {
                let method = if archived {
                    indexes::sessions::METHOD_RESTORE
                } else {
                    indexes::sessions::METHOD_ARCHIVE
                };
                self.send_plugin_call(
                    2,
                    method,
                    json!({ "id": id }),
                    Command::SessionArchive(id, archived),
                    &mut out,
                );
                return Ok(out);
            }
            Command::SessionClone(id) => {
                self.send_plugin_call(
                    2,
                    indexes::sessions::METHOD_CLONE,
                    json!({ "id": id }),
                    Command::SessionClone(id),
                    &mut out,
                );
                return Ok(out);
            }
            Command::UploadFile { key, name, bytes } => {
                let Some((sid, _)) = self.ui.selected.get_untracked() else {
                    self.ui
                        .attachments
                        .update(|att| att.mark_failed(&key, "No active session".into()));
                    return Ok(out);
                };
                let hold_id =
                    self.hold_contract(&P2_SESSION_FILES, false, Some(&sid), None, &mut out);
                let total_bytes = bytes.len() as u64;
                let chunks = split_upload_chunks(&bytes, UPLOAD_CHUNK_BYTES);
                self.active_uploads.insert(
                    key.clone(),
                    ActiveUpload {
                        hold_id,
                        total_bytes,
                        chunks,
                        upload_token: None,
                        cancelled: false,
                    },
                );
                self.ui.attachments.update(|att| att.mark_beginning(&key));
                self.next_call += 1;
                out.push(encode_begin_upload_call(
                    self.next_call,
                    hold_id,
                    &name,
                    "web",
                ));
                self.pending_uploads
                    .insert(self.next_call, UploadRpc::Begin { key });
                return Ok(out);
            }
            Command::CancelUpload(key) => {
                let token = self
                    .ui
                    .attachments
                    .with_untracked(|att| att.upload_token_for(&key).map(str::to_owned));
                let hold_id = self.active_uploads.get(&key).map(|a| a.hold_id);
                if let Some(active) = self.active_uploads.get_mut(&key) {
                    active.cancelled = true;
                }
                if let (Some(h), Some(tok)) = (hold_id, token) {
                    self.active_uploads.remove(&key);
                    self.next_call += 1;
                    out.push(encode_cancel_upload_call(self.next_call, h, &tok));
                    self.pending_uploads
                        .insert(self.next_call, UploadRpc::Cancel);
                }
                self.ui.attachments.update(|att| att.remove(&key));
                return Ok(out);
            }
            Command::Send(_) | Command::Create | Command::CreateWith { .. } => {}
        }

        if self.pending_primary.is_some() {
            return Ok(out);
        }
        let (hold, method, args, cmd_to_store) = match cmd {
            Command::Send(text) if self.ui.write.get_untracked() => {
                let destination = self
                    .ui
                    .selected
                    .get_untracked()
                    .ok_or("no selected conversation")?;
                let previous = self.ui.last_submission.get_value();
                let id = match previous {
                    Some(prior) if prior.destination == destination && prior.text == text => {
                        prior.id
                    }
                    _ => uuid()?,
                };
                let assembled = self
                    .ui
                    .attachments
                    .with_untracked(|a| a.assemble_send_payload(&text, &id, "steer"));
                let payload = match assembled {
                    Ok(p) => p,
                    Err(err) => {
                        let msg = err.to_string();
                        self.ui
                            .attachments
                            .update(|a| a.last_error = Some(msg.clone()));
                        self.ui.status.set(msg);
                        return Ok(out);
                    }
                };
                self.ui.last_submission.set_value(Some(Submission {
                    destination,
                    text: payload.text.clone(),
                    id,
                }));
                if payload.text != text {
                    self.ui.draft.set(payload.text.clone());
                }
                (
                    self.chat,
                    indexes::chat::METHOD_SEND,
                    payload.wire_args,
                    Command::Send(payload.text),
                )
            }
            Command::Create if self.ui.create.get_untracked() => {
                let cwd = self
                    .members
                    .get(&(2, indexes::sessions::STATE_OWNER_DEFAULT_CWD))
                    .and_then(|m| m.value.as_str())
                    .ok_or("missing default working directory")?;
                (
                    2,
                    indexes::sessions::METHOD_CREATE,
                    json!({
                        "cwd": cwd,
                        "env": self.members.get(&(2, indexes::sessions::STATE_OWNER_DEFAULT_ENV)).map(|m| &m.value)
                    }),
                    Command::Create,
                )
            }
            Command::CreateWith { cwd, env, title } if self.ui.create.get_untracked() => {
                let mut obj = serde_json::Map::new();
                obj.insert("cwd".into(), json!(cwd));
                if let Some(e) = env {
                    obj.insert("env".into(), json!(e));
                }
                if let Some(t) = title {
                    obj.insert("title".into(), json!(t));
                }
                (
                    2,
                    indexes::sessions::METHOD_CREATE,
                    Value::Object(obj),
                    Command::Create,
                )
            }
            _ => return Ok(out),
        };
        self.next_call += 1;
        out.push(wire::call(self.next_call, hold, method, args));
        self.ui.busy.set(true);
        self.pending_primary = Some((self.next_call, cmd_to_store));
        Ok(out)
    }
}

async fn connected(url: &str, ui: Ui, mut commands: UnboundedReceiver<Command>) -> Result<()> {
    let mut socket = WebSocket::open(url).map_err(|e| e.to_string())?;
    let mut hello = vec![0, 1];
    let mut incarnation = [0; 16];
    window()
        .crypto()
        .map_err(js_error)?
        .get_random_values_with_u8_array(&mut incarnation)
        .map_err(js_error)?;
    hello.extend(incarnation);
    hello.push(0);
    socket
        .send(Message::Bytes(hello))
        .await
        .map_err(|e| e.to_string())?;
    let mut link = Link {
        ui,
        catalog_member: indexes::sessions::STATE_OWNER_SESSIONS,
        chat: 0,
        next_hold: 3,
        hold_contracts: HashMap::new(),
        contract_holds: HashMap::new(),
        tracker: HoldTracker::default(),
        next_call: 0,
        pending_primary: None,
        pending_plugin: HashMap::new(),
        pending_uploads: HashMap::new(),
        active_uploads: HashMap::new(),
        greeted: false,
        members: HashMap::new(),
    };
    loop {
        let out = futures_util::select! {
            msg = socket.next().fuse() => match msg {
                Some(Ok(Message::Bytes(bytes))) => link.frame(&bytes)?,
                Some(Err(gloo_net::websocket::WebSocketError::ConnectionClose(event))) => {
                    if !matches!(detect_close_reload(event.code, &event.reason), ReloadTrigger::None) {
                        save_reload_state(ui);
                        window().location().reload().map_err(js_error)?;
                        return Ok(());
                    }
                    return Err(format!("Chord closed ({}): {}", event.code, event.reason));
                }
                Some(Err(error)) => return Err(error.to_string()),
                _ => return Err("Chord disconnected".into()),
            },
            cmd = commands.next().fuse() => match cmd {
                Some(cmd) => link.command(cmd)?,
                None => return Ok(()),
            }
        };
        for frame in out {
            socket
                .send(Message::Bytes(frame))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
}

async fn run(root: HtmlElement, ui: Ui) -> Result<()> {
    let (url, cloud, base) = endpoint(&root)?;
    ui.asset_revision.set_value(base);
    if !cloud
        && js_sys::Reflect::has(&window().navigator(), &"serviceWorker".into()).unwrap_or(false)
    {
        let registration = window().navigator().service_worker().register("/sw.js");
        wasm_bindgen_futures::spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(registration).await;
        });
    }
    let mut failures = 0u32;
    loop {
        let (tx, rx) = unbounded();
        ui.commands.set_value(Some(tx));
        let start = js_sys::Date::now();
        let ended = connected(&url, ui, rx).await;
        ui.commands.set_value(None);
        ui.write.set(false);
        ui.create.set(false);
        ui.busy.set(false);
        ui.streaming.set(false);
        ui.status
            .set(ended.err().unwrap_or_else(|| "Disconnected".into()));
        if ui.guest.get_untracked() {
            let check_path = ui
                .selected
                .get_untracked()
                .map(|(sid, _)| format!("/s/{sid}"))
                .unwrap_or_else(|| "/".to_owned());
            if let Ok(r) = gloo_net::http::Request::get(&check_path)
                .method(gloo_net::http::Method::HEAD)
                .cache(web_sys::RequestCache::NoStore)
                .send()
                .await
                && matches!(r.status(), 401 | 403)
            {
                if let Some((sid, _)) = ui.selected.get_untracked() {
                    let updated = ui.transcript_cache.try_update_value(|store| {
                        store.remove_session(&sid);
                        store.clone()
                    });
                    if let Some(store) = updated {
                        persist_transcript_cache(&store, None);
                    }
                    let sid_del = sid.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = idb_delete_session(sid_del).await;
                    });
                }
                ui.sessions.set(vec![]);
                ui.messages.set(vec![]);
                ui.selected.set(None);
                return Err("Share ended".into());
            }
        }
        if !cloud
            && let loaded_base = ui.asset_revision.get_value()
            && !loaded_base.is_empty()
            && let Ok(resp) = gloo_net::http::Request::get("/_chord/web/base")
                .cache(web_sys::RequestCache::NoStore)
                .send()
                .await
            && resp.ok()
            && let Ok(remote_base) = resp.text().await
            && !matches!(
                detect_asset_revision_change(&loaded_base, &remote_base),
                ReloadTrigger::None
            )
        {
            save_reload_state(ui);
            window().location().reload().map_err(js_error)?;
            return Ok(());
        }
        if js_sys::Date::now() - start >= 10000.0 {
            failures = 0
        }
        gloo_timers::future::TimeoutFuture::new(crate::chord_bridge::backoff(failures) as u32)
            .await;
        failures = failures.saturating_add(1);
    }
}

fn render_side_panel(ui: Ui) -> AnyView {
    let close = Callback::new(move |_| ui.command(Command::OpenPanel(SidePanel::None)));
    view! {
        <div class="plugin-side-panel" data-testid="side-panel">
            {move || {
                if let Some(custom_id) = ui.custom_panel.get() {
                    let desc = ui
                        .plugin_registry
                        .with(|reg| reg.plugins.get(&custom_id).cloned())
                        .unwrap_or_else(|| crate::browser_plugins::PluginDescriptor {
                            id: custom_id.clone(),
                            title: custom_id.clone(),
                            revision: String::new(),
                            contract: None,
                            panel: Some(custom_id.clone()),
                            command_id: None,
                            keywords: vec![],
                            section: "Plugins".into(),
                            enabled: true,
                            surface_url: None,
                            wasm_url: None,
                            mount_kind: None,
                            schema: None,
                        });
                    let sid = ui.selected.get().map(|(s, _)| s);
                    return view! {
                        <DynamicPluginView descriptor=desc session_id=sid on_close=close />
                    }.into_any();
                }
                let sid = ui.selected.get().map(|(s, _)| s);
                let errs = ui.plugin_errors.get();
                match ui.panel.get() {
                    SidePanel::None => ().into_any(),
                    SidePanel::Cost => view! {
                        <CostPanel costs=ui.costs.get() on_close=close />
                    }.into_any(),
                    SidePanel::Artifacts => view! {
                        <ArtifactsPanel
                            view_state=ui.artifacts.get()
                            session_id=sid
                            err=errs.get(P2_ARTIFACTS.name).cloned()
                            on_close=close
                            on_read=Callback::new(move |id| ui.command(Command::ReadArtifact(id)))
                            on_remove=Callback::new(move |id| ui.command(Command::RemoveArtifact(id)))
                        />
                    }.into_any(),
                    SidePanel::Models => view! {
                        <ModelsPanel
                            view_state=ui.models.get()
                            err=errs.get(P2_MODELS.name).cloned()
                            on_close=close
                            on_set_default=Callback::new(move |(p, m, t)| ui.command(Command::SetDefaultModel(p, m, t)))
                            on_set_session_model=Callback::new(move |(p, m, t)| ui.command(Command::SetSessionModel(p, m, t)))
                            on_set_enabled=Callback::new(move |pats| ui.command(Command::SetEnabledModels(pats)))
                        />
                    }.into_any(),
                    SidePanel::Settings => {
                        let view_state = ui.settings.get();
                        let err = errs.get(P2_SETTINGS.name).cloned().or_else(|| view_state.error.clone());
                        view! {
                            <SettingsPanel
                                view_state=view_state
                                err=err
                                on_close=close
                                on_update=Callback::new(move |(k, v)| ui.command(Command::UpdateSetting(k, v)))
                                on_unset=Callback::new(move |k| ui.command(Command::UnsetSetting(k)))
                            />
                        }.into_any()
                    }
                    SidePanel::Mcp => view! {
                        <McpPanel
                            view_state=ui.mcp.get()
                            err=errs.get(P2_MCP.name).cloned()
                            on_close=close
                            on_restart=Callback::new(move |id| ui.command(Command::McpRestart(id)))
                            on_sign_in=Callback::new(move |srv| ui.command(Command::McpSignIn(srv)))
                            on_sign_out=Callback::new(move |srv| ui.command(Command::McpSignOut(srv)))
                            on_paste_redirect=Callback::new(move |(id, url)| ui.command(Command::McpPasteRedirect(id, url)))
                            on_cancel_sign_in=Callback::new(move |id| ui.command(Command::McpCancelSignIn(id)))
                        />
                    }.into_any(),
                    SidePanel::Sharing => view! {
                        <SharingPanel
                            view_state=ui.sharing.get()
                            err=errs.get(P2_SHARING.name).cloned()
                            on_close=close
                            on_create_link=Callback::new(move |(m, r)| ui.command(Command::ShareCreateLink(m, r)))
                            on_share_email=Callback::new(move |(e, m)| ui.command(Command::ShareEmail(e, m)))
                            on_remove_guest=Callback::new(move |id| ui.command(Command::ShareRemoveGuest(id)))
                        />
                    }.into_any(),
                    SidePanel::Tree => view! {
                        <TreePanel
                            view_state=ui.tree.get()
                            err=errs.get(P2_TREE.name).cloned()
                            on_close=close
                            on_select_branch=Callback::new(move |c| ui.command(Command::SelectBranch(c)))
                            on_navigate=Callback::new(move |e| ui.command(Command::TreeNavigate(e)))
                            on_branch=Callback::new(move |e| ui.command(Command::TreeBranch(e)))
                            on_rename_branch=Callback::new(move |(c, l)| ui.command(Command::TreeRename(c, l)))
                        />
                    }.into_any(),
                    SidePanel::Voice => {
                        let view_state = ui.voice.get();
                        let err = errs.get(P2_VOICE_MODE.name).cloned().or_else(|| view_state.failure.clone());
                        let v_sig = Signal::derive(move || ui.voice.get());
                        view! {
                            <div class="sheet" role="dialog" aria-label="Voice" data-testid="panel-voice">
                                <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Voice ×"</button>
                                {err.map(|e| view! { <p class="muted small">{format!("Status: {e}")}</p> })}
                                <Voice
                                    state=ui.voice_rt.state
                                    on_toggle=Callback::new(move |_| ui.command(Command::ToggleVoice))
                                    view_info=v_sig
                                    mic_muted=ui.voice_rt.muted
                                    pause_secs=ui.voice_rt.pause_secs
                                    interim_text=Signal::derive(move || ui.voice_rt.interim.get())
                                    final_text=Signal::derive(move || ui.voice_rt.final_text.get())
                                    on_choose_voice=Callback::new(move |(e, v, s)| ui.command(Command::ChooseVoice(e, v, s)))
                                    on_set_vad=Callback::new(move |secs| ui.voice_rt.send(VoiceWsAction::SetVad(secs)))
                                    on_test_sample=Callback::new(move |_| {
                                        let v = ui.voice.get_untracked();
                                        play_voice_sample(v.engine, v.voice, v.speed.unwrap_or(1.0));
                                    })
                                    on_cancel_tts=Callback::new(move |_| ui.voice_rt.send(VoiceWsAction::CancelTts))
                                />
                            </div>
                        }.into_any()
                    }
                    SidePanel::Login => view! {
                        <LoginPanel
                            view_state=ui.login.get()
                            err=errs.get(P2_PROVIDER_LOGIN.name).cloned()
                            on_close=close
                            on_start=Callback::new(move |(p, a)| ui.command(Command::LoginStart(p, a)))
                            on_answer=Callback::new(move |(a, p, v)| ui.command(Command::LoginAnswer(a, p, v)))
                            on_cancel=Callback::new(move |id| ui.command(Command::LoginCancel(id)))
                            on_dismiss=Callback::new(move |id| ui.command(Command::LoginDismiss(id)))
                            on_logout=Callback::new(move |id| ui.command(Command::LoginLogout(id)))
                            on_refresh=Callback::new(move |id| ui.command(Command::LoginRefresh(id)))
                        />
                    }.into_any(),
                    SidePanel::Devices => view! {
                        <DevicesPanel
                            view_state=ui.devices.get()
                            err=errs.get(P2_DEVICES.name).cloned()
                            on_close=close
                            on_mint_token=Callback::new(move |_| ui.command(Command::MintDeviceToken))
                        />
                    }.into_any(),
                    SidePanel::Discord => view! {
                        <DiscordPanel
                            view_state=ui.discord.get()
                            active_session=sid
                            err=errs.get(P2_DISCORD.name).cloned()
                            on_close=close
                            on_add_bot=Callback::new(move |t| ui.command(Command::DiscordAddBot(t)))
                            on_remove_bot=Callback::new(move |b| ui.command(Command::DiscordRemoveBot(b)))
                            on_link=Callback::new(move |(s, b, g, c)| ui.command(Command::DiscordLink(s, b, g, c)))
                            on_unlink=Callback::new(move |s| ui.command(Command::DiscordUnlink(s)))
                        />
                    }.into_any(),
                    SidePanel::Envs => view! {
                        <EnvsPanel
                            view_state=ui.envs.get()
                            err=errs.get(P2_ENVS.name).cloned()
                            on_close=close
                            on_save=Callback::new(move |(n, h)| ui.command(Command::EnvSave(n, h)))
                            on_test=Callback::new(move |n| ui.command(Command::EnvTest(n)))
                            on_delete=Callback::new(move |n| ui.command(Command::EnvDelete(n)))
                            on_generate_key=Callback::new(move |n| ui.command(Command::EnvGenerateKey(n)))
                            on_sandbox_start=Callback::new(move |n| ui.command(Command::EnvSandboxStart(n)))
                            on_sandbox_stop=Callback::new(move |n| ui.command(Command::EnvSandboxStop(n)))
                        />
                    }.into_any(),
                    SidePanel::Ports => view! {
                        <PortsPanel
                            view_state=ui.ports.get()
                            err=errs.get(P2_PORTS.name).cloned()
                            on_close=close
                            on_add=Callback::new(move |(p, n, pub_)| ui.command(Command::PortAdd(p, n, pub_)))
                            on_set_public=Callback::new(move |(id, pub_)| ui.command(Command::PortSetPublic(id, pub_)))
                            on_remove=Callback::new(move |id| ui.command(Command::PortRemove(id)))
                        />
                    }.into_any(),
                    SidePanel::Logs => view! {
                        <LogsPanel
                            view_state=ui.logs.get()
                            err=errs.get(P2_LOGS.name).cloned()
                            on_close=close
                            on_select_source=Callback::new(move |s| ui.command(Command::SelectLogSource(s)))
                        />
                    }.into_any(),
                    SidePanel::Telemetry => view! {
                        <TelemetryPanel
                            view_state=ui.traces.get()
                            err=errs.get(P2_TRACES.name).cloned()
                            on_close=close
                            on_find_traces=Callback::new(move |q| ui.command(Command::FindTraces(q)))
                            on_get_trace=Callback::new(move |id| ui.command(Command::GetTrace(id)))
                        />
                    }.into_any(),
                    SidePanel::Subagents => view! {
                        <SubagentsPanel
                            view_state=ui.subagents.get()
                            err=errs.get(P2_SUBAGENTS.name).cloned()
                            on_close=close
                            on_open_session=Callback::new(move |id| ui.command(Command::Open(id, 1)))
                        />
                    }.into_any(),
                    SidePanel::ChatMode => view! {
                        <ChatModePanel
                            view_state=ui.chat_mode.get()
                            err=errs.get(P2_CHAT_MODE.name).cloned()
                            on_close=close
                            on_set_enabled=Callback::new(move |en| ui.command(Command::ChatModeSet(en)))
                            on_rename=Callback::new(move |n| ui.command(Command::ChatModeRename(n)))
                        />
                    }.into_any(),
                    SidePanel::Familiar => view! {
                        <FamiliarPanel
                            view_state=ui.familiar.get()
                            err=errs.get(P2_FAMILIAR.name).cloned()
                            on_close=close
                            on_open_familiar=Callback::new(move |_| ui.command(Command::OpenFamiliar))
                        />
                    }.into_any(),
                    SidePanel::Prompt => view! {
                        <PromptPanel
                            view_state=ui.prompt_res.get()
                            err=errs.get(P2_PROMPT_RESOURCES.name).cloned()
                            on_close=close
                            on_expand=Callback::new(move |t| ui.command(Command::ExpandPrompt(t)))
                        />
                    }.into_any(),
                    SidePanel::History => {
                        let entries = ui
                            .messages
                            .get()
                            .into_iter()
                            .map(|m| (m.id, format!("{:?}", m.role), m.text))
                            .collect::<Vec<_>>();
                        view! {
                            <HistoryPanel
                                entries=entries
                                err=None
                                on_close=close
                                on_load_earlier=Callback::new(move |_| ui.command(Command::LoadEarlierEntries))
                            />
                        }.into_any()
                    }
                    SidePanel::ScopePrompt => view! {
                        <ScopePromptPanel
                            err=errs.get(P2_SCOPE_STAGE.name).cloned()
                            on_close=close
                            on_stage=Callback::new(move |(b, n)| ui.command(Command::StageScopePrompt(b, n)))
                        />
                    }.into_any(),
                    SidePanel::Views => view! {
                        <ViewsPanel
                            view_state=ui.views_state.get()
                            err=errs.get(P2_VIEWS.name).cloned()
                            on_close=close
                            on_open_view=Callback::new(move |_| ui.command(Command::ViewsOpen))
                            on_select_view=Callback::new(move |(id, sel)| ui.command(Command::ViewsSelect(id, sel)))
                            on_close_view=Callback::new(move |id| ui.command(Command::ViewsClose(id)))
                            on_unannotate=Callback::new(move |ids| ui.command(Command::ViewsUnannotate(ids)))
                        />
                    }.into_any(),
                }
            }}
        </div>
    }.into_any()
}

pub fn mount_app() -> std::result::Result<(), JsValue> {
    let root = document()
        .get_element_by_id("root")
        .ok_or("Missing application root")?
        .dyn_into::<HtmlElement>()?;
    let parent = root.clone();
    let app = move || {
        let loc = window().location();
        let path = loc.pathname().unwrap_or_default();
        let search = loc.search().unwrap_or_default();
        let initial_route = path_route(&path, &search);
        let nav_state = NavigationState::from_initial(
            initial_route.clone(),
            session_get(NAV_STORAGE_KEY).as_deref(),
            session_get(SCROLL_STORAGE_KEY).as_deref(),
        );
        let session_panels_store =
            SessionPanelsStore::from_json(local_get(SESSION_PANELS_STORAGE_KEY).as_deref());

        let reload_snap =
            local_get(RELOAD_STATE_STORAGE_KEY).and_then(|raw| decode_reload_snapshot(&raw));
        if reload_snap.is_some() {
            local_remove(RELOAD_STATE_STORAGE_KEY);
        }
        let url_selected = initial_route
            .session
            .clone()
            .map(|id| (id, initial_route.conversation));
        let last_conv =
            decode_last_conversation(local_get(LAST_CONVERSATION_STORAGE_KEY).as_deref())
                .map(|l| (l.session, l.conversation));
        let selected = url_selected
            .or_else(|| {
                reload_snap
                    .as_ref()
                    .and_then(|s| s.session_id.clone().zip(s.conversation))
            })
            .or_else(|| {
                (!initial_route.new_session && initial_route.page.is_none())
                    .then_some(last_conv)
                    .flatten()
            });

        let restored_draft = selected
            .as_ref()
            .and_then(|(sid, conv)| {
                let key = draft_key_for(sid, *conv);
                let raw_draft =
                    session_get(DRAFT_STORAGE_KEY).or_else(|| local_get(DRAFT_STORAGE_KEY));
                restore_draft_for_conversation(raw_draft.as_deref(), &key).or_else(|| {
                    reload_snap
                        .as_ref()
                        .filter(|s| s.draft_key.as_deref() == Some(key.as_str()))
                        .map(|s| crate::state::composer_state::rename_references(&s.draft_text))
                })
            })
            .unwrap_or_default();

        let (restored_panel, restored_custom_panel) = match reload_snap
            .as_ref()
            .and_then(|s| s.panel_id.as_deref())
            .or(initial_route.panel.as_deref())
        {
            Some(pid) => match SidePanel::from_id(pid) {
                Some(p) => (p, None),
                None => (SidePanel::None, Some(pid.to_owned())),
            },
            None => (SidePanel::None, None),
        };

        let transcript_store = PersistedTranscriptStore::from_json_or_empty(
            local_get(STORAGE_KEY_TRANSCRIPTS).as_deref(),
        );
        let initial_messages = selected
            .as_ref()
            .and_then(|(sid, conv)| transcript_store.get(sid, *conv))
            .map(|conv| chat_messages_from_projected(projected_from_cached(&conv.messages)))
            .unwrap_or_default();

        let recents_init = Recents::read(local_get(RECENTS_STORAGE_KEY).as_deref());
        let native = is_native_host(webkit_handler(PI_HOST_HANDLER).is_some());
        let media_query: Option<MediaQueryList> = window()
            .match_media("(prefers-color-scheme: dark)")
            .ok()
            .flatten();
        let system_dark = media_query.as_ref().is_some_and(MediaQueryList::matches);
        let initial_pref = if native {
            ThemePreference::System
        } else {
            local_get(THEME_STORAGE_KEY)
                .as_deref()
                .and_then(ThemePreference::parse)
                .unwrap_or(ThemePreference::System)
        };
        let initial_theme_state = ThemeBridgeState::new(initial_pref, system_dark, native);
        let initial_label = initial_pref.label();
        apply_html_theme(initial_theme_state.html_attributes());

        let initial_collapsed = local_get(SIDEBAR_COLLAPSED_KEY).as_deref() == Some("1");
        let initial_sb_w = read_size(local_get(SIDEBAR_WIDTH_KEY).as_deref()).unwrap_or(260);
        let initial_pnl_w = read_size(local_get(PANEL_WIDTH_KEY).as_deref()).unwrap_or(380);
        let initial_wrap = local_get(WORKSPACE_WRAP_KEY).as_deref() == Some("1");

        let ui = Ui {
            sessions: RwSignal::new(vec![]),
            messages: RwSignal::new(initial_messages),
            selected: RwSignal::new(selected),
            nav: StoredValue::new(nav_state),
            route_page: RwSignal::new(initial_route.page),
            route_new: RwSignal::new(initial_route.new_session),
            session_panels: StoredValue::new(session_panels_store),
            status: RwSignal::new("Connecting".into()),
            write: RwSignal::new(false),
            create: RwSignal::new(false),
            default_cwd: RwSignal::new(".".into()),
            default_env: RwSignal::new(None),
            draft: RwSignal::new(restored_draft),
            guest: RwSignal::new(false),
            commands: StoredValue::new(None),
            busy: RwSignal::new(false),
            streaming: RwSignal::new(false),
            last_submission: StoredValue::new(None),
            pane: RwSignal::new(MainPane::Chat),
            panel: RwSignal::new(restored_panel),
            custom_panel: RwSignal::new(restored_custom_panel),
            command_menu_open: RwSignal::new(false),
            command_query: RwSignal::new(String::new()),
            command_recents: RwSignal::new(recents_init.ids().to_vec()),
            completion: RwSignal::new(None),
            sidebar_collapsed: RwSignal::new(initial_collapsed),
            sidebar_width: RwSignal::new(initial_sb_w),
            panel_width: RwSignal::new(initial_pnl_w),
            workspace_tab: RwSignal::new(WorkspaceTab::Files),
            workspace_wrap: RwSignal::new(initial_wrap),
            models: RwSignal::new(ModelsView::default()),
            settings: RwSignal::new(SettingsView::default()),
            mcp: RwSignal::new(McpServersView::default()),
            sharing: RwSignal::new(SharingView::default()),
            session_status: RwSignal::new(SessionStatusView::default()),
            costs: RwSignal::new(vec![]),
            artifacts: RwSignal::new(ArtifactsView::default()),
            tree: RwSignal::new(TreeView::default()),
            terminals: RwSignal::new(TerminalsView::default()),
            terminal_rt: TerminalRuntime::new(),
            voice: RwSignal::new(VoiceView::default()),
            voice_rt: VoiceRuntime::new(),
            login: RwSignal::new(LoginView::default()),
            devices: RwSignal::new(DevicesView::default()),
            discord: RwSignal::new(DiscordView::default()),
            envs: RwSignal::new(EnvsView::default()),
            ports: RwSignal::new(PortsView::default()),
            logs: RwSignal::new(LogsView::default()),
            log_notices: RwSignal::new(vec![]),
            dismissed_notices: RwSignal::new(HashSet::new()),
            traces: RwSignal::new(TracesView::default()),
            workspace: RwSignal::new(WorkspaceView::default()),
            views_state: RwSignal::new(ViewsLayoutView::default()),
            subagents: RwSignal::new(SubagentsView::default()),
            chat_mode: RwSignal::new(ChatModeView::default()),
            familiar: RwSignal::new(FamiliarView::default()),
            prompt_res: RwSignal::new(PromptResourcesView::default()),
            projects: RwSignal::new(ProjectsView::default()),
            rollups: RwSignal::new(vec![]),
            search_hits: RwSignal::new(vec![]),
            session_metrics: RwSignal::new(SessionMetricsView::default()),
            attachments: RwSignal::new(ComposerAttachments::default()),
            plugin_errors: RwSignal::new(HashMap::new()),
            transcript_cache: StoredValue::new(transcript_store),
            plugin_registry: RwSignal::new(PluginRegistryState::builtin()),
            asset_revision: StoredValue::new(String::new()),
            theme: StoredValue::new(initial_theme_state),
            theme_label: RwSignal::new(initial_label),
        };

        // Hydrate transcripts asynchronously from IndexedDB `p2-conversations`.
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(idb_store) = idb_load_all().await {
                let _ = ui.transcript_cache.try_update_value(|store| {
                    for (k, conv) in idb_store.conversations {
                        store.conversations.entry(k).or_insert(conv);
                    }
                });
                if let Some((sid, conv)) = ui.selected.get_untracked()
                    && ui.messages.with_untracked(Vec::is_empty)
                {
                    hydrate_messages_from_cache(ui, &sid, conv);
                }
            }
        });

        // Persist draft to sessionStorage on change.
        Effect::new(move |_| {
            let text = ui.draft.get();
            if let Some((sid, conv)) = ui.selected.get_untracked() {
                if text.is_empty() {
                    session_remove(DRAFT_STORAGE_KEY);
                } else {
                    let _ =
                        session_set(DRAFT_STORAGE_KEY, &format_draft_storage(&sid, conv, &text));
                }
            }
            if let Some(prefix) = extract_at_prefix(&text) {
                ui.command(Command::CompleteFiles(prefix));
            } else {
                ui.completion.set(None);
            }
        });

        // Listen to browser popstate (Back / Forward).
        let on_popstate =
            Closure::<dyn FnMut(web_sys::PopStateEvent)>::new(move |ev: web_sys::PopStateEvent| {
                let idx = ev.state().as_f64().map(|n| n.max(0.0) as usize);
                let loc = window().location();
                let fallback = path_route(
                    &loc.pathname().unwrap_or_default(),
                    &loc.search().unwrap_or_default(),
                );
                let target = ui
                    .nav
                    .try_update_value(|nav| nav.on_popstate(idx, fallback.clone()))
                    .unwrap_or(fallback);
                ui.command(Command::Navigate(target, false));
            });
        let _ = window()
            .add_event_listener_with_callback("popstate", on_popstate.as_ref().unchecked_ref());
        on_popstate.forget();

        sync_theme_dom_and_shell(ui);
        if let Some((turn, msg)) = ui
            .theme
            .try_update_value(|state| state.start_sync())
            .flatten()
        {
            send_pi_host_theme_request(ui, turn, msg);
        }

        if let Some(mql) = media_query {
            let mql_ref = mql.clone();
            let on_media = Closure::<dyn FnMut(web_sys::Event)>::new(move |_ev: web_sys::Event| {
                let dark = mql_ref.matches();
                let changed = ui
                    .theme
                    .try_update_value(|state| state.handle_system_media_change(dark))
                    .unwrap_or(false);
                if changed {
                    sync_theme_dom_and_shell(ui);
                }
            });
            let _ =
                mql.add_event_listener_with_callback("change", on_media.as_ref().unchecked_ref());
            on_media.forget();
        }

        let on_host_theme =
            Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
                let detail_val = js_sys::JSON::stringify(&ev.detail())
                    .ok()
                    .and_then(|s| s.as_string())
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
                let changed = ui
                    .theme
                    .try_update_value(|state| state.handle_host_event(detail_val.as_ref()))
                    .unwrap_or(false);
                if changed {
                    sync_theme_dom_and_shell(ui);
                }
            });
        let _ = window().add_event_listener_with_callback(
            HOST_THEME_EVENT,
            on_host_theme.as_ref().unchecked_ref(),
        );
        on_host_theme.forget();

        let on_host_menu =
            Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
                let detail_val = js_sys::JSON::stringify(&ev.detail())
                    .ok()
                    .and_then(|s| s.as_string())
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
                if matches!(
                    HostMenuEvent::from_value(detail_val.as_ref()),
                    Some(HostMenuEvent::Settings)
                ) {
                    ui.command(Command::OpenPanel(SidePanel::Settings));
                }
            });
        let _ = window().add_event_listener_with_callback(
            HOST_MENU_EVENT,
            on_host_menu.as_ref().unchecked_ref(),
        );
        on_host_menu.forget();

        let on_plugin_reload =
            Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
                let Some((rev, descriptors)) = js_sys::JSON::stringify(&ev.detail())
                    .ok()
                    .and_then(|s| s.as_string())
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .and_then(|v| decode_plugin_manifest(&v))
                else {
                    return;
                };
                let active_panel = ui.custom_panel.get_untracked().or_else(|| {
                    let p = ui.panel.get_untracked();
                    p.is_open().then(|| p.id().to_owned())
                });
                let mut closed_active = false;
                ui.plugin_registry.update(|reg| {
                    let report = reg.hot_swap(rev, descriptors, active_panel.as_deref());
                    closed_active = report.closed_active_panel;
                });
                if closed_active {
                    ui.panel.set(SidePanel::None);
                    ui.custom_panel.set(None);
                }
            });
        let _ = window().add_event_listener_with_callback(
            PLUGIN_RELOAD_EVENT,
            on_plugin_reload.as_ref().unchecked_ref(),
        );
        on_plugin_reload.forget();

        let on_beforeunload =
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_ev: web_sys::Event| {
                save_reload_state(ui);
            });
        let _ = window().add_event_listener_with_callback(
            "beforeunload",
            on_beforeunload.as_ref().unchecked_ref(),
        );
        on_beforeunload.forget();

        Effect::new(move |_| {
            let selected_id = ui.selected.get().map(|(s, _)| s);
            let sessions = ui.sessions.get();
            let title = selected_id
                .as_deref()
                .and_then(|sid| sessions.iter().find(|s| s.id == sid))
                .map(|s| s.title.as_str());
            let statuses = ui.session_status.get();
            let busy_count: usize = statuses
                .statuses
                .values()
                .filter(|s| !s.busy_conversations.is_empty() || s.running)
                .count();
            let formatted = format_document_title(title, busy_count);
            document().set_title(&formatted);
            let _ = post_shell_message(&ShellBridgeMessage::Title(formatted));
            let _ = post_shell_message(&ShellBridgeMessage::Badge(busy_count as u64));
        });

        let apple = window()
            .navigator()
            .platform()
            .map(|p| p.to_ascii_lowercase().contains("mac"))
            .unwrap_or(false);
        let on_keydown = Closure::<dyn FnMut(KeyboardEvent)>::new(move |ev: KeyboardEvent| {
            let mods = Modifiers {
                meta: ev.meta_key(),
                ctrl: ev.ctrl_key(),
                alt: ev.alt_key(),
                shift: ev.shift_key(),
            };
            let modal_open = ui.command_menu_open.get_untracked()
                || ui.panel.get_untracked().is_open()
                || ui.custom_panel.get_untracked().is_some();
            if let Some(action) = resolve_shortcut(
                &ev.key(),
                mods,
                apple,
                ui.streaming.get_untracked(),
                modal_open,
            ) {
                ev.prevent_default();
                match action {
                    ShortcutAction::OpenCommandMenu => {
                        ui.command_menu_open.update(|open| *open = !*open);
                    }
                    ShortcutAction::ToggleScopePrompt => {
                        let next = if ui.panel.get_untracked() == SidePanel::ScopePrompt {
                            SidePanel::None
                        } else {
                            SidePanel::ScopePrompt
                        };
                        ui.command(Command::OpenPanel(next));
                    }
                    ShortcutAction::ToggleSidebar => {
                        ui.sidebar_collapsed.update(|c| {
                            *c = !*c;
                            let _ = local_set(SIDEBAR_COLLAPSED_KEY, if *c { "1" } else { "0" });
                        });
                    }
                    ShortcutAction::ToggleTerminal => {
                        let next = if ui.pane.get_untracked() == MainPane::Terminal {
                            MainPane::Chat
                        } else {
                            MainPane::Terminal
                        };
                        ui.command(Command::OpenPane(next));
                    }
                    ShortcutAction::ToggleWorkspace => {
                        let next = if ui.pane.get_untracked() == MainPane::Workspace {
                            MainPane::Chat
                        } else {
                            MainPane::Workspace
                        };
                        ui.command(Command::OpenPane(next));
                    }
                    ShortcutAction::StopGeneration => ui.command(Command::Stop),
                    ShortcutAction::CloseModal => {
                        ui.command_menu_open.set(false);
                        ui.command(Command::OpenPanel(SidePanel::None));
                    }
                }
            }
        });
        let _ = window()
            .add_event_listener_with_callback("keydown", on_keydown.as_ref().unchecked_ref());
        on_keydown.forget();

        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = run(root, ui).await {
                ui.status.set(e)
            }
        });

        let show_archived = RwSignal::new(false);
        let sidebar = view! {
            <div class="sidebar-content">
                <div class="sidebar-header-row">
                    <span>"Sessions"</span>
                    <button
                        type="button"
                        class="button small"
                        aria-label="Collapse sidebar"
                        on:click=move |_| {
                            ui.sidebar_collapsed.update(|c| {
                                *c = !*c;
                                let _ = local_set(SIDEBAR_COLLAPSED_KEY, if *c { "1" } else { "0" });
                            });
                        }
                    >"⇆"</button>
                </div>
                <button disabled=move || !ui.create.get() || ui.busy.get()
                    on:click=move |_| ui.command(Command::Create)>"New session"</button>
                <button type="button" class="button small" on:click=move |_| {
                    ui.command(Command::Navigate(Route::new_session_screen(), true));
                }>"New (Options)"</button>
                <RichSessionsControls
                    projects=Signal::derive(move || ui.projects.get())
                    rollups=Signal::derive(move || ui.rollups.get())
                    search_hits=Signal::derive(move || ui.search_hits.get())
                    show_archived=show_archived
                    on_search=Callback::new(move |(q, arch)| ui.command(Command::SearchSessions(q, arch)))
                    on_open_list=Callback::new(move |page| ui.command(Command::Navigate(Route::list_page(page), true)))
                    on_open_session=Callback::new(move |(id, c)| ui.command(Command::Open(id, c)))
                />
                {move || {
                    let statuses = ui.session_status.get();
                    let include_archived = show_archived.get();
                    ui.sessions
                        .get()
                        .into_iter()
                        .filter(|s| include_archived || !s.archived)
                        .map(|session| {
                            let sid_open = session.id.clone();
                            let sid_pin = session.id.clone();
                            let sid_ren = session.id.clone();
                            let sid_clone = session.id.clone();
                            let sid_arch = session.id.clone();
                            let pinned = session.pinned;
                            let archived = session.archived;
                            let renaming = RwSignal::new(false);
                            let rename_val = RwSignal::new(session.title.clone());
                            let status_entry = statuses.statuses.get(&session.id).cloned();
                            let badge = status_entry.as_ref().map(|s| {
                                if !s.busy_conversations.is_empty() {
                                    "busy"
                                } else if s.running {
                                    "running"
                                } else {
                                    "idle"
                                }
                            });
                            let main = session.main;
                            view! {
                                <div class="sidebar-session-row">
                                    {move || if renaming.get() {
                                        let sid_submit = sid_ren.clone();
                                        view! {
                                            <form on:submit=move |ev| {
                                                ev.prevent_default();
                                                let next_t = rename_val.get_untracked().trim().to_owned();
                                                if !next_t.is_empty() {
                                                    ui.command(Command::SessionRename(sid_submit.clone(), next_t));
                                                }
                                                renaming.set(false);
                                            }>
                                                <input type="text" prop:value=move || rename_val.get() on:input=move |ev| rename_val.set(event_target_value(&ev)) />
                                            </form>
                                        }.into_any()
                                    } else {
                                        let sid_click = sid_open.clone();
                                        let label = if pinned { format!("★ {}", session.title) } else { session.title.clone() };
                                        view! {
                                            <button disabled=move || ui.busy.get()
                                                on:click=move |_| ui.command(Command::Open(sid_click.clone(), main))>
                                                {label}
                                                {badge.map(|b| view! { <span class="session-badge">{format!(" ({b})")}</span> })}
                                            </button>
                                        }.into_any()
                                    }}
                                    <button type="button" class="button small" title="Pin" on:click=move |_| ui.command(Command::SessionPin(sid_pin.clone(), !pinned))>"★"</button>
                                    <button type="button" class="button small" title="Rename" on:click=move |_| renaming.update(|r| *r = !*r)>"✎"</button>
                                    <button type="button" class="button small" title="Clone" on:click=move |_| ui.command(Command::SessionClone(sid_clone.clone()))>"⧉"</button>
                                    <button type="button" class="button small" title="Archive" on:click=move |_| ui.command(Command::SessionArchive(sid_arch.clone(), archived))>"×"</button>
                                </div>
                            }
                        }).collect_view()
                }}
                <div class="sidebar-tools" data-testid="sidebar-panels">
                    {move || {
                        ui.plugin_registry.get().active_panels().into_iter().map(|desc| {
                            let panel_id = desc.panel.clone().unwrap_or_else(|| desc.id.clone());
                            let testid = format!("panel-btn-{}", desc.id);
                            let label = match desc.id.as_str() {
                                "mcp" => "MCP".to_owned(),
                                "tree" => "Tree".to_owned(),
                                _ => desc.title.clone(),
                            };
                            view! {
                                <button type="button" data-testid=testid on:click=move |_| {
                                    if let Some(p) = SidePanel::from_id(&panel_id) {
                                        ui.command(Command::OpenPanel(p));
                                    } else {
                                        ui.command(Command::OpenCustomPanel(panel_id.clone()));
                                    }
                                }>{label}</button>
                            }
                        }).collect_view()
                    }}
                    <button type="button" data-testid="pane-btn-terminal" on:click=move |_| ui.command(Command::OpenPane(MainPane::Terminal))>"Terminal"</button>
                    <button type="button" data-testid="pane-btn-workspace" on:click=move |_| ui.command(Command::OpenPane(MainPane::Workspace))>"Workspace"</button>
                    <button type="button" class="theme-toggle" data-testid="theme-toggle" title=move || ui.theme_label.get() on:click=move |_| cycle_theme(ui)>
                        {move || ui.theme_label.get()}
                    </button>
                    <button type="button" data-testid="command-menu-btn" on:click=move |_| ui.command_menu_open.update(|o| *o = !*o)>"Commands"</button>
                </div>
            </div>
        }.into_any();

        let can_send_empty =
            Signal::derive(move || ui.attachments.with(|a| a.has_ready_attachments()));
        let on_attach_files = Callback::new(move |files: Vec<SelectedUploadFile>| {
            if files.is_empty() {
                return;
            }
            let sid = ui.selected.get_untracked().map(|(s, _)| s);
            let chosen: Vec<(String, u64)> =
                files.iter().map(|f| (f.name.clone(), f.size)).collect();
            let mut keys = Vec::with_capacity(files.len());
            for _ in 0..files.len() {
                keys.push(uuid().unwrap_or_else(|_| format!("att-{}", js_sys::Date::now() as u64)));
            }
            let mut accepted = Vec::new();
            ui.attachments.update(|att| {
                let res = att.enqueue_files(sid.as_deref(), &chosen, |i| keys[i].clone());
                accepted = res.accepted_keys;
            });
            for file in files {
                if accepted.is_empty() {
                    break;
                }
                if let Some(pos) = chosen
                    .iter()
                    .position(|(n, s)| n == &file.name && *s == file.size)
                    && pos < MAX_ATTACHMENTS_PER_MESSAGE
                {
                    let key = keys[pos].clone();
                    if accepted.contains(&key) {
                        ui.command(Command::UploadFile {
                            key,
                            name: file.name,
                            bytes: file.bytes,
                        });
                    }
                }
            }
        });

        let on_choose_completion = Callback::new(move |idx: usize| {
            let pop = ui.completion.get_untracked();
            if let Some(popover) = pop
                && let Some(item) = popover.items.get(idx)
            {
                let cur = ui.draft.get_untracked();
                let (next_text, _) = apply_file_completion(&cur, cur.len(), item, &popover.prefix);
                ui.draft.set(next_text);
            }
            ui.completion.set(None);
        });

        let main = view! {
            <div class="main-content">
                <p role="status">{move || ui.status.get()}</p>
                {move || (!ui.guest.get()).then(|| view! {
                    <OwnerNotices
                        notices=Signal::derive(move || ui.log_notices.get())
                        dismissed=ui.dismissed_notices
                        on_dismiss=Callback::new(move |id| ui.command(Command::DismissNotice(id)))
                        on_show_logs=Callback::new(move |src| {
                            ui.command(Command::SelectLogSource(src));
                            ui.command(Command::OpenPanel(SidePanel::Logs));
                        })
                    />
                })}
                {move || {
                    if ui.route_new.get() {
                        return view! {
                            <NewSessionView
                                default_cwd=Signal::derive(move || ui.default_cwd.get())
                                default_env=Signal::derive(move || ui.default_env.get())
                                can_create=Signal::derive(move || ui.create.get())
                                on_create_with=Callback::new(move |(cwd, env, title)| ui.command(Command::CreateWith { cwd, env, title }))
                                on_cancel=Callback::new(move |_| ui.command(Command::Navigate(Route::home(), true)))
                            />
                        }.into_any();
                    }
                    if let Some(page) = ui.route_page.get() {
                        let summaries = Signal::derive(move || {
                            ui.sessions
                                .get()
                                .into_iter()
                                .map(|s| SessionSummary {
                                    id: s.id,
                                    title: s.title,
                                    main: s.main,
                                    pinned: s.pinned,
                                    archived: s.archived,
                                    badge: None,
                                })
                                .collect::<Vec<_>>()
                        });
                        return view! {
                            <ListPageView
                                page=Signal::derive(move || page.clone())
                                sessions=summaries
                                rollups=Signal::derive(move || ui.rollups.get())
                                on_open_session=Callback::new(move |(id, c)| ui.command(Command::Open(id, c)))
                                on_back=Callback::new(move |_| ui.command(Command::Navigate(Route::home(), true)))
                            />
                        }.into_any();
                    }
                    match ui.pane.get() {
                        MainPane::Terminal => view! {
                            <div class="terminal-wrap">
                                <div class="terminal-toolbar">
                                    <button type="button" data-testid="pane-btn-chat" on:click=move |_| ui.command(Command::OpenPane(MainPane::Chat))>"Back to Chat"</button>
                                    <button type="button" data-testid="terminal-open-btn" on:click=move |_| ui.command(Command::OpenTerminal)>"Open Terminal"</button>
                                </div>
                                <Terminal
                                    status=Signal::derive(move || ui.terminal_rt.status.get())
                                    lines=ui.terminal_rt.lines
                                    title=Signal::derive(move || ui.terminal_rt.title.get())
                                    tabs=Signal::derive(move || ui.terminal_rt.tabs.get())
                                    active_id=Signal::derive(move || ui.terminal_rt.active_id.get())
                                    cols=ui.terminal_rt.cols
                                    rows=ui.terminal_rt.rows
                                    on_new_tab=Callback::new(move |_| ui.command(Command::OpenTerminal))
                                    on_select_tab=Callback::new(move |id| ui.command(Command::TerminalAttach(id)))
                                    on_close_tab=Callback::new(move |id| ui.command(Command::TerminalClose(id)))
                                    on_key_input=Callback::new(move |bytes| ui.terminal_rt.send_input(bytes))
                                    on_resize=Callback::new(move |(c, r)| {
                                        ui.terminal_rt.send_input(vec![]);
                                        if let Some(tx) = ui.terminal_rt.tx.get_value() {
                                            let _ = tx.unbounded_send(TerminalWsAction::Resize(c, r));
                                        }
                                        ui.command(Command::TerminalResize(c, r));
                                    })
                                />
                            </div>
                        }.into_any(),
                        MainPane::Workspace => view! {
                            <WorkspacePane
                                state=Signal::derive(move || ui.workspace.get())
                                active_tab=ui.workspace_tab
                                wrap_lines=ui.workspace_wrap
                                on_back_to_chat=Callback::new(move |_| ui.command(Command::OpenPane(MainPane::Chat)))
                                on_open_terminal=Callback::new(move |_| ui.command(Command::OpenPane(MainPane::Terminal)))
                                on_select_file=Callback::new(move |path| ui.command(Command::ReadWorkspaceFile(path)))
                                on_toggle_wrap=Callback::new(move |w| {
                                    let _ = local_set(WORKSPACE_WRAP_KEY, if w { "1" } else { "0" });
                                })
                                on_refresh_changes=Callback::new(move |_| ui.command(Command::QueryWorkspace))
                            />
                        }.into_any(),
                        MainPane::Chat => view! {
                            <Chat messages=ui.messages />
                            <SessionStats
                                metrics=Signal::derive(move || ui.session_metrics.get())
                                on_cost=Callback::new(move |_| ui.command(Command::OpenPanel(SidePanel::Cost)))
                                on_clients=Callback::new(move |_| ui.command(Command::OpenPanel(SidePanel::Sharing)))
                            />
                            {move || {
                                let att = ui.attachments.get();
                                (!att.files.is_empty() || att.last_error.is_some()).then(|| view! {
                                    <div class="composer-attachments" data-testid="composer-attachments">
                                        {att.last_error.map(|err| view! {
                                            <span class="notice error small" data-testid="attachment-error">{err}</span>
                                        })}
                                        {att.files.into_iter().map(|f| {
                                            let key = f.key.clone();
                                            let status_text = att.uploads.get(&key).map(|u| match &u.phase {
                                                crate::state::attachments::UploadPhase::Queued => "queued".to_owned(),
                                                crate::state::attachments::UploadPhase::Beginning => "starting...".to_owned(),
                                                crate::state::attachments::UploadPhase::Uploading { .. } => format!("{}%", u.phase.progress_percent()),
                                                crate::state::attachments::UploadPhase::Finishing => "finishing...".to_owned(),
                                                crate::state::attachments::UploadPhase::Uploaded { .. } => "ready".to_owned(),
                                                crate::state::attachments::UploadPhase::Failed(e) => format!("failed: {e}"),
                                            });
                                            view! {
                                                <span class="attachment-chip" data-testid="attachment-chip">
                                                    {f.name}
                                                    {status_text.map(|st| view! { <span class="muted small">{format!(" ({st})")}</span> })}
                                                    <button type="button" on:click=move |_| ui.command(Command::CancelUpload(key.clone()))>"×"</button>
                                                </span>
                                            }
                                        }).collect_view()}
                                    </div>
                                })
                            }}
                            <fieldset disabled=move || !ui.write.get() || ui.busy.get()>
                                <Composer
                                    draft=ui.draft
                                    clear_on_send=false
                                    on_send=Callback::new(move |text| ui.command(Command::Send(text)))
                                    on_attach_files=on_attach_files
                                    can_send_empty=can_send_empty
                                    streaming=Signal::derive(move || ui.streaming.get())
                                    on_stop=Callback::new(move |_| ui.command(Command::Stop))
                                    completion=ui.completion
                                    on_choose_completion=on_choose_completion
                                />
                            </fieldset>
                        }.into_any(),
                    }
                }}
                {move || ui.command_menu_open.get().then(|| {
                    let commands = ui.plugin_registry.get().dynamic_commands();
                    let recents = ui.command_recents.get();
                    view! {
                        <CommandMenu
                            commands=commands
                            query=ui.command_query
                            recents=recents
                            on_run=Callback::new(move |id: String| {
                                ui.command_recents.update(|r| {
                                    r.retain(|x| x != &id);
                                    r.insert(0, id.clone());
                                    r.truncate(8);
                                    if let Ok(json) = serde_json::to_string(r) {
                                        let _ = local_set(RECENTS_STORAGE_KEY, &json);
                                    }
                                });
                                ui.command_menu_open.set(false);
                                match id.as_str() {
                                    "new-session" => ui.command(Command::Create),
                                    "pane-chat" => ui.command(Command::OpenPane(MainPane::Chat)),
                                    "pane-terminal" => ui.command(Command::OpenPane(MainPane::Terminal)),
                                    "pane-workspace" => ui.command(Command::OpenPane(MainPane::Workspace)),
                                    "toggle-sidebar" => ui.sidebar_collapsed.update(|c| {
                                        *c = !*c;
                                        let _ = local_set(SIDEBAR_COLLAPSED_KEY, if *c { "1" } else { "0" });
                                    }),
                                    "stop-generation" => ui.command(Command::Stop),
                                    "scope-prompt-toggle" => ui.command(Command::OpenPanel(SidePanel::ScopePrompt)),
                                    "toggle-theme" => cycle_theme(ui),
                                    other => {
                                        if let Some(pid) = other.strip_prefix("panel-") {
                                            if let Some(sp) = SidePanel::from_id(pid) {
                                                ui.command(Command::OpenPanel(sp));
                                            } else {
                                                ui.command(Command::OpenCustomPanel(pid.to_owned()));
                                            }
                                        }
                                    }
                                }
                            })
                            on_close=Callback::new(move |_| ui.command_menu_open.set(false))
                        />
                    }
                })}
            </div>
        }.into_any();

        let _ = MAINS_STORAGE_KEY;
        let panel = Some(render_side_panel(ui));
        view! {
            <AppLayout
                sidebar=sidebar
                main=main
                panel=panel
                pane_signal=ui.pane
                sidebar_collapsed=ui.sidebar_collapsed
                sidebar_width=ui.sidebar_width
                panel_width=ui.panel_width
                on_resize_sidebar=Callback::new(move |w: u32| {
                    let _ = local_set(SIDEBAR_WIDTH_KEY, &w.to_string());
                })
                on_resize_panel=Callback::new(move |w: u32| {
                    let _ = local_set(PANEL_WIDTH_KEY, &w.to_string());
                })
            />
        }
    };
    mount_to(parent, app).forget();
    Ok(())
}

#[wasm_bindgen(start)]
pub fn start() -> std::result::Result<(), JsValue> {
    mount_app()
}
