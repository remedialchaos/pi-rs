//! Leptos browser host for the p2 Chord contracts.
#![cfg(target_arch = "wasm32")]
mod wire;

use crate::activity::model::ToolActivityTable;
use crate::browser_plugins::{
    ArtifactsView, ComposerAttachments, ContractSpec, DRAFT_STORAGE_KEY, HoldTracker,
    McpServersView, ModelsView, P2_ACCESS, P2_ARTIFACTS, P2_CHAT, P2_CHAT_COSTS, P2_MCP, P2_MODELS,
    P2_SESSION_FILES, P2_SESSION_STATUS, P2_SESSIONS, P2_SETTINGS, P2_SHARING, P2_TERMINALS,
    P2_TREE, P2_VOICE_MODE, P2_VOICE_SERVICE, PLUGIN_RELOAD_EVENT, PluginRegistryState,
    ProjectedMessage, ProjectedRole, RELOAD_STATE_STORAGE_KEY, ReloadSnapshot, ReloadTrigger,
    SessionStatusView, SettingsView, SharingView, SidePanel, TerminalsView, TreeView, VoiceView,
    decode_artifact_content, decode_artifacts_view, decode_costs_view, decode_mcp_view,
    decode_models_view, decode_plugin_manifest, decode_reload_snapshot, decode_session_status_view,
    decode_settings_view, decode_sharing_view, decode_terminals_list, decode_tree_view,
    decode_voice_view, detect_asset_revision_change, detect_close_reload, draft_key_for,
    encode_release_frame, encode_reload_snapshot, format_draft_storage, hold_for_contract, indexes,
    project_chat_transcript, restore_draft_for_conversation,
};
use crate::chord_bridge::{
    HOST_MENU_EVENT, HOST_THEME_EVENT, HostAnswer, HostMenuEvent, HtmlThemeAttributes,
    PI_HOST_HANDLER, ShellBridgeMessage, THEME_STORAGE_KEY, ThemeBridgeState, ThemePreference,
    format_document_title, is_native_host,
};
use crate::cost::AssistantCost;
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
use crate::ui::{
    app::{AppLayout, MainPane},
    chat::{Chat, ChatMessage, MessageRole},
    command_menu::CommandMenu,
    composer::{Composer, SelectedUploadFile},
    cost_ui::CostPanel,
    terminal::{Terminal, TerminalStatus, Voice, VoiceState},
};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::{mount::mount_to, prelude::*};
use serde_json::{Value, json};
use std::collections::HashMap;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlElement, KeyboardEvent, MediaQueryList, Url};
use wire::{Reader, Result};

#[derive(Clone, PartialEq)]
struct Session {
    id: String,
    title: String,
    main: u64,
}

#[derive(Clone)]
enum Command {
    Open(String, u64),
    Send(String),
    Create,
    OpenPanel(SidePanel),
    OpenCustomPanel(String),
    OpenPane(MainPane),
    ReadArtifact(String),
    RefreshCosts,
    RefreshTree,
    SelectBranch(u64),
    SetDefaultModel(String, String, String),
    OpenTerminal,
    ToggleVoice,
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
    status: RwSignal<String>,
    write: RwSignal<bool>,
    create: RwSignal<bool>,
    draft: RwSignal<String>,
    guest: RwSignal<bool>,
    // Preserve the id of an unacknowledged send across a reconnect.
    last_submission: StoredValue<Option<Submission>>,
    busy: RwSignal<bool>,
    commands: StoredValue<Option<UnboundedSender<Command>>>,
    pane: RwSignal<MainPane>,
    panel: RwSignal<SidePanel>,
    custom_panel: RwSignal<Option<String>>,
    command_menu_open: RwSignal<bool>,
    command_query: RwSignal<String>,
    command_recents: RwSignal<Vec<String>>,
    models: RwSignal<ModelsView>,
    settings: RwSignal<SettingsView>,
    mcp: RwSignal<McpServersView>,
    sharing: RwSignal<SharingView>,
    session_status: RwSignal<SessionStatusView>,
    costs: RwSignal<Vec<AssistantCost>>,
    artifacts: RwSignal<ArtifactsView>,
    tree: RwSignal<TreeView>,
    terminals: RwSignal<TerminalsView>,
    terminal_lines: RwSignal<Vec<String>>,
    voice: RwSignal<VoiceView>,
    voice_state: RwSignal<VoiceState>,
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

fn storage_get(key: &str) -> Option<String> {
    let storage = window().local_storage().ok().flatten()?;
    storage.get_item(key).ok().flatten()
}

fn storage_set(key: &str, value: &str) -> bool {
    let Ok(Some(storage)) = window().local_storage() else {
        return false;
    };
    storage.set_item(key, value).is_ok()
}

fn storage_remove(key: &str) {
    if let Ok(Some(storage)) = window().local_storage() {
        let _ = storage.remove_item(key);
    }
}

fn persist_transcript_cache(store: &PersistedTranscriptStore) {
    let json = store.to_bounded_json(MAX_PERSISTED_STORE_BYTES);
    if !storage_set(STORAGE_KEY_TRANSCRIPTS, &json) {
        let smaller = store.to_bounded_json(MAX_PERSISTED_STORE_BYTES / 2);
        let _ = storage_set(STORAGE_KEY_TRANSCRIPTS, &smaller);
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

fn save_reload_state(ui: Ui) {
    let selected = ui.selected.get_untracked();
    let draft = ui.draft.get_untracked();
    if let Some((ref sid, conv)) = selected {
        let _ = storage_set(DRAFT_STORAGE_KEY, &format_draft_storage(sid, conv, &draft));
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
    let _ = storage_set(RELOAD_STATE_STORAGE_KEY, &encode_reload_snapshot(&snap));
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
        let _ = storage_set(THEME_STORAGE_KEY, next.as_str());
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
    project_chat_transcript(view, &ToolActivityTable::new())
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
        let Some((sid, conv)) = self.ui.selected.get_untracked() else {
            return;
        };
        match panel {
            SidePanel::Cost => {
                let h = self.hold_contract(&P2_CHAT_COSTS, false, Some(&sid), Some(conv), out);
                self.send_plugin_call(
                    h,
                    indexes::chat_costs::METHOD_COSTS,
                    json!(null),
                    Command::RefreshCosts,
                    out,
                );
            }
            SidePanel::Artifacts => {
                self.hold_contract(&P2_ARTIFACTS, false, Some(&sid), None, out);
            }
            SidePanel::Tree => {
                let h = self.hold_contract(&P2_TREE, false, Some(&sid), None, out);
                self.send_plugin_call(
                    h,
                    indexes::tree::METHOD_TREE,
                    json!(null),
                    Command::RefreshTree,
                    out,
                );
            }
            SidePanel::Voice => {
                self.hold_contract(&P2_VOICE_MODE, false, Some(&sid), Some(conv), out);
            }
            SidePanel::Sharing => {
                self.hold_contract(&P2_SHARING, false, Some(&sid), None, out);
            }
            SidePanel::None | SidePanel::Models | SidePanel::Settings | SidePanel::Mcp => {}
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
        out.push(wire::hold(hold_id, Some((&id, conversation))));
        self.ui.selected.set(Some((id, conversation)));
        let current_panel = self.ui.panel.get_untracked();
        if current_panel.is_open() {
            self.ensure_plugin_hold_for_panel(current_panel, out);
        }
        if self.ui.pane.get_untracked() == MainPane::Terminal {
            let sid = self
                .ui
                .selected
                .get_untracked()
                .map(|(s, _)| s)
                .unwrap_or_default();
            let h = self.hold_contract(&P2_TERMINALS, false, Some(&sid), None, out);
            self.next_call += 1;
            out.push(wire::call(
                self.next_call,
                h,
                indexes::terminals::METHOD_LIST,
                json!({}),
            ));
            self.pending_plugin
                .insert(self.next_call, Command::OpenTerminal);
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
                    .filter(|s| s["archivedAt"].is_null())
                    .filter_map(|s| {
                        Some(Session {
                            id: s["id"].as_str()?.into(),
                            title: s["title"].as_str()?.into(),
                            main: s["main"].as_u64().unwrap_or(1),
                        })
                    })
                    .collect();
                let pick = self
                    .ui
                    .selected
                    .get_untracked()
                    .filter(|(id, _)| rows.iter().any(|s| s.id == *id))
                    .or_else(|| rows.first().map(|s| (s.id.clone(), s.main)));
                self.ui.sessions.set(rows);
                if let Some((id, c)) = pick
                    && (self.chat == 0 || self.ui.selected.get_untracked() != Some((id.clone(), c)))
                {
                    self.select(id, c, out);
                } else if self.ui.sessions.with_untracked(Vec::is_empty)
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
            "p2.chat" if h == self.chat && m == indexes::chat::STATE_VIEW => {
                let live_rows = transcript(value);
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
                        persist_transcript_cache(&store);
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
                self.ui.voice_state.set(view.state);
                self.ui.voice.set(view);
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
                // Core server holds (ids 1 and 2 preserved).
                self.hold_contracts.insert(1, P2_ACCESS.name);
                self.contract_holds.insert(P2_ACCESS.name, 1);
                self.tracker.register(1, P2_ACCESS.name, true);
                self.hold_contracts.insert(2, P2_SESSIONS.name);
                self.contract_holds.insert(P2_SESSIONS.name, 2);
                self.tracker.register(2, P2_SESSIONS.name, true);
                self.next_hold = 3;
                out.extend([wire::hold(1, None), wire::hold(2, None)]);
                // Optional server plugin holds: isolated on failure.
                for spec in [
                    &P2_SESSION_STATUS,
                    &P2_VOICE_SERVICE,
                    &P2_MODELS,
                    &P2_SETTINGS,
                    &P2_MCP,
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
                                storage_remove(DRAFT_STORAGE_KEY);
                            }
                        }
                        Command::Create if kind == 8 => {
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
                    if kind == 8 {
                        match cmd {
                            Command::RefreshCosts => {
                                self.ui.costs.set(decode_costs_view(&value));
                            }
                            Command::ReadArtifact(_) => {
                                if let Some((aid, content)) = decode_artifact_content(&value) {
                                    self.ui.artifacts.update(|a| {
                                        a.selected_id = Some(aid);
                                        a.selected_content = Some(content);
                                    });
                                }
                            }
                            Command::RefreshTree | Command::SelectBranch(_) => {
                                if value.is_object() {
                                    self.ui.tree.set(decode_tree_view(&value));
                                }
                            }
                            Command::OpenTerminal => {
                                let list = decode_terminals_list(&value);
                                self.ui.terminals.update(|t| {
                                    t.available = list.clone();
                                    t.status = TerminalStatus::Live;
                                });
                                self.ui.terminal_lines.set(if list.is_empty() {
                                    vec!["Terminal ready (no active PTY attached)".into()]
                                } else {
                                    list.into_iter()
                                        .map(|id| format!("terminal: {id}"))
                                        .collect()
                                });
                            }
                            _ => {}
                        }
                    } else if let Some(contract) = match cmd {
                        Command::RefreshCosts => Some(P2_CHAT_COSTS.name),
                        Command::ReadArtifact(_) => Some(P2_ARTIFACTS.name),
                        Command::RefreshTree | Command::SelectBranch(_) => Some(P2_TREE.name),
                        Command::OpenTerminal => Some(P2_TERMINALS.name),
                        Command::ToggleVoice => Some(P2_VOICE_MODE.name),
                        _ => None,
                    } {
                        self.ui.plugin_errors.update(|errs| {
                            errs.insert(contract.to_owned(), value.to_string());
                        });
                    }
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

    fn command(&mut self, cmd: Command) -> Result<Vec<Vec<u8>>> {
        let mut out = vec![];
        match cmd {
            Command::Open(id, c) => {
                if self.pending_primary.is_none() {
                    self.select(id, c, &mut out);
                }
                return Ok(out);
            }
            Command::OpenPanel(panel) => {
                self.ui.custom_panel.set(None);
                self.ui.panel.set(panel);
                if panel.is_open() {
                    self.ensure_plugin_hold_for_panel(panel, &mut out);
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
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::terminals::METHOD_LIST,
                        json!({}),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::OpenTerminal);
                }
                return Ok(out);
            }
            Command::ReadArtifact(aid) => {
                if let Some(&h) = self.contract_holds.get(P2_ARTIFACTS.name) {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::artifacts::METHOD_READ,
                        json!({ "id": aid }),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::ReadArtifact(aid));
                }
                return Ok(out);
            }
            Command::RefreshCosts => {
                if let Some(&h) = self.contract_holds.get(P2_CHAT_COSTS.name) {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::chat_costs::METHOD_COSTS,
                        json!(null),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::RefreshCosts);
                }
                return Ok(out);
            }
            Command::RefreshTree => {
                if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::tree::METHOD_TREE,
                        json!(null),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::RefreshTree);
                }
                return Ok(out);
            }
            Command::SelectBranch(conv) => {
                if let Some((sid, _)) = self.ui.selected.get_untracked() {
                    if let Some(&h) = self.contract_holds.get(P2_TREE.name) {
                        self.next_call += 1;
                        out.push(wire::call(
                            self.next_call,
                            h,
                            indexes::tree::METHOD_SELECT,
                            json!({ "conversation": conv }),
                        ));
                        self.pending_plugin
                            .insert(self.next_call, Command::SelectBranch(conv));
                    }
                    self.select(sid, conv, &mut out);
                }
                return Ok(out);
            }
            Command::SetDefaultModel(provider, model_id, thinking_level) => {
                if let Some(&h) = self.contract_holds.get(P2_MODELS.name) {
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::models::METHOD_SET_DEFAULT,
                        json!({
                            "provider": provider,
                            "modelId": model_id,
                            "thinkingLevel": thinking_level
                        }),
                    ));
                    self.pending_plugin.insert(
                        self.next_call,
                        Command::SetDefaultModel(provider, model_id, thinking_level),
                    );
                }
                return Ok(out);
            }
            Command::OpenTerminal => {
                self.ui.pane.set(MainPane::Terminal);
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let h = self.hold_contract(&P2_TERMINALS, false, Some(&sid), None, &mut out);
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::terminals::METHOD_OPEN,
                        json!({ "conversation": conv, "cols": 80, "rows": 24 }),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::OpenTerminal);
                }
                return Ok(out);
            }
            Command::ToggleVoice => {
                let next_enabled = !self.ui.voice.get_untracked().enabled;
                if let Some((sid, conv)) = self.ui.selected.get_untracked() {
                    let h =
                        self.hold_contract(&P2_VOICE_MODE, false, Some(&sid), Some(conv), &mut out);
                    self.next_call += 1;
                    out.push(wire::call(
                        self.next_call,
                        h,
                        indexes::voice_mode::METHOD_SET,
                        json!({ "enabled": next_enabled }),
                    ));
                    self.pending_plugin
                        .insert(self.next_call, Command::ToggleVoice);
                }
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
            Command::Send(_) | Command::Create => {}
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
                        persist_transcript_cache(&store);
                    }
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
                    let title = ui
                        .plugin_registry
                        .with(|reg| reg.plugins.get(&custom_id).map(|d| d.title.clone()))
                        .unwrap_or_else(|| custom_id.clone());
                    let rev = ui
                        .plugin_registry
                        .with(|reg| reg.plugins.get(&custom_id).map(|d| d.revision.clone()))
                        .unwrap_or_default();
                    return view! {
                        <div class="sheet" role="dialog" aria-label=title.clone() data-testid=format!("panel-{custom_id}")>
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>{format!("{title} ×")}</button>
                            <p class="muted small">{format!("Dynamic plugin active (revision {rev})")}</p>
                        </div>
                    }.into_any();
                }
                match ui.panel.get() {
                SidePanel::None => ().into_any(),
                SidePanel::Cost => {
                    let costs = ui.costs.get();
                    view! { <CostPanel costs=costs on_close=close /> }.into_any()
                }
                SidePanel::Artifacts => {
                    let view_state = ui.artifacts.get();
                    let err = ui.plugin_errors.get().get(P2_ARTIFACTS.name).cloned();
                    view! {
                        <div class="sheet" role="dialog" aria-label="Artifacts" data-testid="panel-artifacts">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Artifacts ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
                            <ul class="plugin-list">
                                {view_state.items.into_iter().map(|item| {
                                    let id = item.id.clone();
                                    view! {
                                        <li>
                                            <button type="button" on:click=move |_| ui.command(Command::ReadArtifact(id.clone()))>
                                                {format!("{} ({})", item.title, item.kind)}
                                            </button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                            {view_state.selected_content.map(|content| view! {
                                <pre class="artifact-preview">{content}</pre>
                            })}
                        </div>
                    }.into_any()
                }
                SidePanel::Models => {
                    let view_state = ui.models.get();
                    let err = ui.plugin_errors.get().get(P2_MODELS.name).cloned();
                    view! {
                        <div class="sheet" role="dialog" aria-label="Models" data-testid="panel-models">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Models ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
                            {view_state.default_model_id.as_ref().map(|m| view! {
                                <p class="muted small">{format!("Default: {m}")}</p>
                            })}
                            <ul class="plugin-list">
                                {view_state.models.into_iter().map(|m| {
                                    let prov = m.provider.clone();
                                    let mid = m.model_id.clone();
                                    let thinking = m.thinking_levels.first().cloned().unwrap_or_else(|| "off".into());
                                    view! {
                                        <li>
                                            <button type="button" on:click=move |_| ui.command(Command::SetDefaultModel(prov.clone(), mid.clone(), thinking.clone()))>
                                                {format!("{}/{}", m.provider, m.model_id)}
                                            </button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        </div>
                    }.into_any()
                }
                SidePanel::Settings => {
                    let view_state = ui.settings.get();
                    let err = ui.plugin_errors.get().get(P2_SETTINGS.name).cloned().or(view_state.error);
                    view! {
                        <div class="sheet" role="dialog" aria-label="Settings" data-testid="panel-settings">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Settings ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Status: {e}")}</p> })}
                            <dl class="settings-list">
                                {view_state.entries.into_iter().map(|(k, v)| view! {
                                    <div class="settings-row"><dt>{k}</dt><dd>{v}</dd></div>
                                }).collect_view()}
                            </dl>
                        </div>
                    }.into_any()
                }
                SidePanel::Mcp => {
                    let view_state = ui.mcp.get();
                    let err = ui.plugin_errors.get().get(P2_MCP.name).cloned();
                    view! {
                        <div class="sheet" role="dialog" aria-label="MCP Servers" data-testid="panel-mcp">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"MCP Servers ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
                            <ul class="plugin-list">
                                {view_state.instances.into_iter().map(|inst| view! {
                                    <li>{format!("{} [{}] ({} tools)", inst.server, inst.status, inst.tool_count)}</li>
                                }).collect_view()}
                            </ul>
                        </div>
                    }.into_any()
                }
                SidePanel::Sharing => {
                    let view_state = ui.sharing.get();
                    let err = ui.plugin_errors.get().get(P2_SHARING.name).cloned();
                    view! {
                        <div class="sheet" role="dialog" aria-label="Sharing" data-testid="panel-sharing">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Sharing ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
                            <ul class="plugin-list">
                                {view_state.links.into_iter().map(|l| view! {
                                    <li>{format!("{}: {}", l.mode, l.url)}</li>
                                }).collect_view()}
                                {view_state.guests.into_iter().map(|g| view! {
                                    <li>{format!("{} ({})", g.name, g.mode)}</li>
                                }).collect_view()}
                            </ul>
                        </div>
                    }.into_any()
                }
                SidePanel::Tree => {
                    let view_state = ui.tree.get();
                    let err = ui.plugin_errors.get().get(P2_TREE.name).cloned();
                    view! {
                        <div class="sheet" role="dialog" aria-label="Session Tree" data-testid="panel-tree">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Session Tree ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Unavailable: {e}")}</p> })}
                            <ul class="plugin-list">
                                {view_state.branches.into_iter().map(|b| {
                                    let conv = b.conversation;
                                    let label = b.label.unwrap_or_else(|| format!("Branch {conv}"));
                                    view! {
                                        <li>
                                            <button type="button" on:click=move |_| ui.command(Command::SelectBranch(conv))>
                                                {label}
                                            </button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        </div>
                    }.into_any()
                }
                SidePanel::Voice => {
                    let view_state = ui.voice.get();
                    let err = ui.plugin_errors.get().get(P2_VOICE_MODE.name).cloned().or(view_state.failure);
                    view! {
                        <div class="sheet" role="dialog" aria-label="Voice" data-testid="panel-voice">
                            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| close.run(())>"Voice ×"</button>
                            {err.map(|e| view! { <p class="muted small">{format!("Status: {e}")}</p> })}
                            <Voice state=ui.voice_state on_toggle=Callback::new(move |_| ui.command(Command::ToggleVoice)) />
                            {view_state.engine.map(|eng| view! { <p class="muted small">{format!("Engine: {eng}")}</p> })}
                        </div>
                    }.into_any()
                }
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
        let path = window().location().pathname().unwrap_or_default();
        let route: Vec<_> = path.trim_matches('/').split('/').collect();
        let url_selected = match route.as_slice() {
            ["s", id] => Some((id.to_string(), 1)),
            ["s", id, "c", c] => c.parse().ok().map(|c| (id.to_string(), c)),
            _ => None,
        };
        let reload_snap =
            storage_get(RELOAD_STATE_STORAGE_KEY).and_then(|raw| decode_reload_snapshot(&raw));
        if reload_snap.is_some() {
            storage_remove(RELOAD_STATE_STORAGE_KEY);
        }
        let selected = url_selected.or_else(|| {
            reload_snap
                .as_ref()
                .and_then(|s| s.session_id.clone().zip(s.conversation))
        });
        let restored_draft = selected
            .as_ref()
            .and_then(|(sid, conv)| {
                let key = draft_key_for(sid, *conv);
                let raw_draft = storage_get(DRAFT_STORAGE_KEY);
                restore_draft_for_conversation(raw_draft.as_deref(), &key).or_else(|| {
                    reload_snap
                        .as_ref()
                        .filter(|s| s.draft_key.as_deref() == Some(key.as_str()))
                        .map(|s| crate::state::composer_state::rename_references(&s.draft_text))
                })
            })
            .unwrap_or_default();
        let (restored_panel, restored_custom_panel) =
            match reload_snap.as_ref().and_then(|s| s.panel_id.as_deref()) {
                Some(pid) => match SidePanel::from_id(pid) {
                    Some(p) => (p, None),
                    None => (SidePanel::None, Some(pid.to_owned())),
                },
                None => (SidePanel::None, None),
            };

        let transcript_store = PersistedTranscriptStore::from_json_or_empty(
            storage_get(STORAGE_KEY_TRANSCRIPTS).as_deref(),
        );
        let initial_messages = selected
            .as_ref()
            .and_then(|(sid, conv)| transcript_store.get(sid, *conv))
            .map(|conv| chat_messages_from_projected(projected_from_cached(&conv.messages)))
            .unwrap_or_default();

        let native = is_native_host(webkit_handler(PI_HOST_HANDLER).is_some());
        let media_query: Option<MediaQueryList> = window()
            .match_media("(prefers-color-scheme: dark)")
            .ok()
            .flatten();
        let system_dark = media_query.as_ref().is_some_and(MediaQueryList::matches);
        let initial_pref = if native {
            ThemePreference::System
        } else {
            storage_get(THEME_STORAGE_KEY)
                .as_deref()
                .and_then(ThemePreference::parse)
                .unwrap_or(ThemePreference::System)
        };
        let initial_theme_state = ThemeBridgeState::new(initial_pref, system_dark, native);
        let initial_label = initial_pref.label();
        apply_html_theme(initial_theme_state.html_attributes());

        let ui = Ui {
            sessions: RwSignal::new(vec![]),
            messages: RwSignal::new(initial_messages),
            selected: RwSignal::new(selected),
            status: RwSignal::new("Connecting".into()),
            write: RwSignal::new(false),
            create: RwSignal::new(false),
            draft: RwSignal::new(restored_draft),
            guest: RwSignal::new(false),
            commands: StoredValue::new(None),
            busy: RwSignal::new(false),
            last_submission: StoredValue::new(None),
            pane: RwSignal::new(MainPane::Chat),
            panel: RwSignal::new(restored_panel),
            custom_panel: RwSignal::new(restored_custom_panel),
            command_menu_open: RwSignal::new(false),
            command_query: RwSignal::new(String::new()),
            command_recents: RwSignal::new(vec![]),
            models: RwSignal::new(ModelsView::default()),
            settings: RwSignal::new(SettingsView::default()),
            mcp: RwSignal::new(McpServersView::default()),
            sharing: RwSignal::new(SharingView::default()),
            session_status: RwSignal::new(SessionStatusView::default()),
            costs: RwSignal::new(vec![]),
            artifacts: RwSignal::new(ArtifactsView::default()),
            tree: RwSignal::new(TreeView::default()),
            terminals: RwSignal::new(TerminalsView::default()),
            terminal_lines: RwSignal::new(vec![]),
            voice: RwSignal::new(VoiceView::default()),
            voice_state: RwSignal::new(VoiceState::Idle),
            attachments: RwSignal::new(ComposerAttachments::default()),
            plugin_errors: RwSignal::new(HashMap::new()),
            transcript_cache: StoredValue::new(transcript_store),
            plugin_registry: RwSignal::new(PluginRegistryState::builtin()),
            asset_revision: StoredValue::new(String::new()),
            theme: StoredValue::new(initial_theme_state),
            theme_label: RwSignal::new(initial_label),
        };

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

        let on_keydown = Closure::<dyn FnMut(KeyboardEvent)>::new(move |ev: KeyboardEvent| {
            if (ev.ctrl_key() || ev.meta_key()) && ev.key().eq_ignore_ascii_case("k") {
                ev.prevent_default();
                ui.command_menu_open.update(|open| *open = !*open);
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

        let sidebar = view! {
            <div class="sidebar-content">
                <span>"Sessions"</span>
                <button disabled=move || !ui.create.get() || ui.busy.get()
                    on:click=move |_| ui.command(Command::Create)>"New session"</button>
                {move || {
                    let statuses = ui.session_status.get();
                    ui.sessions.get().into_iter().map(|session| {
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
                        view! {
                            <button disabled=move || ui.busy.get()
                                on:click=move |_| ui.command(Command::Open(session.id.clone(), session.main))>
                                {session.title}
                                {badge.map(|b| view! { <span class="session-badge">{format!(" ({b})")}</span> })}
                            </button>
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

        let main = view! {
            <div class="main-content">
                <p role="status">{move || ui.status.get()}</p>
                {move || if ui.pane.get() == MainPane::Terminal {
                    let term = ui.terminals.get();
                    view! {
                        <div class="terminal-wrap">
                            <div class="terminal-toolbar">
                                <button type="button" data-testid="pane-btn-chat" on:click=move |_| ui.command(Command::OpenPane(MainPane::Chat))>"Back to Chat"</button>
                                <button type="button" data-testid="terminal-open-btn" on:click=move |_| ui.command(Command::OpenTerminal)>"Open Terminal"</button>
                            </div>
                            <Terminal status=term.status lines=ui.terminal_lines title=term.title />
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <Chat messages=ui.messages />
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
                            />
                        </fieldset>
                    }.into_any()
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
                                });
                                ui.command_menu_open.set(false);
                                match id.as_str() {
                                    "new-session" => ui.command(Command::Create),
                                    "pane-chat" => ui.command(Command::OpenPane(MainPane::Chat)),
                                    "pane-terminal" => ui.command(Command::OpenPane(MainPane::Terminal)),
                                    "toggle-theme" => cycle_theme(ui),
                                    "panel-cost" => ui.command(Command::OpenPanel(SidePanel::Cost)),
                                    "panel-artifacts" => ui.command(Command::OpenPanel(SidePanel::Artifacts)),
                                    "panel-models" => ui.command(Command::OpenPanel(SidePanel::Models)),
                                    "panel-settings" => ui.command(Command::OpenPanel(SidePanel::Settings)),
                                    "panel-mcp" => ui.command(Command::OpenPanel(SidePanel::Mcp)),
                                    "panel-sharing" => ui.command(Command::OpenPanel(SidePanel::Sharing)),
                                    "panel-tree" => ui.command(Command::OpenPanel(SidePanel::Tree)),
                                    "panel-voice" => ui.command(Command::OpenPanel(SidePanel::Voice)),
                                    other => {
                                        if let Some(pid) = other.strip_prefix("panel-") {
                                            ui.command(Command::OpenCustomPanel(pid.to_owned()));
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

        let panel = Some(render_side_panel(ui));
        view! { <AppLayout sidebar=sidebar main=main panel=panel pane_signal=ui.pane /> }
    };
    mount_to(parent, app).forget();
    Ok(())
}

#[wasm_bindgen(start)]
pub fn start() -> std::result::Result<(), JsValue> {
    mount_app()
}
