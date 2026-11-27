//! Read the Sessions v7 catalog, Chat v6 conversation, and built-in server/session contracts through a single Chord channel.

use pi_rs_chord::{
    Error, Result,
    delta::{Decoder, apply},
    json::JsonValue,
    link::{
        Body, Channel, ChannelHandler, ChannelOptions, Codec, Limits, Link, Message, Part, Payload,
    },
    sync::SyncFuture,
};
use pi_rs_p2_tui::{
    ThemeRef, Transcript, TranscriptComponentOptions,
    components::{
        AssistantMessage, ContentBlock, EntryRecord, Item, ToolView, TranscriptView,
        shell_entry_renderer,
    },
    tools::{ToolArgs, ToolStatus, create_default_tool_renderers},
    transcript_component,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
};

struct Incoming(mpsc::SyncSender<Message>);
impl ChannelHandler for Incoming {
    fn opened(&self) {}
    fn message(&self, message: Message) -> SyncFuture<()> {
        let queued = self
            .0
            .try_send(message)
            .map_err(|_| Error::Invalid("Terminal event queue full or closed".into()));
        Box::pin(async move { queued })
    }
    fn closed(&self) {}
}

#[derive(Default)]
struct Member {
    value: Option<JsonValue>,
    decoder: Decoder,
}
impl Member {
    fn read(&mut self, part: &Part, snapshot: bool) -> Result<&JsonValue> {
        let payload = part.parse(Limits::for_link(false))?;
        let value = if snapshot {
            self.decoder = Decoder::default();
            payload
        } else {
            let JsonValue::Array(changes) = payload else {
                return Err(Error::Invalid("Invalid session delta".into()));
            };
            let ops = self
                .decoder
                .decode(&changes)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            apply(
                self.value
                    .take()
                    .ok_or_else(|| Error::Invalid("Missing session snapshot".into()))?,
                &ops,
            )
            .map_err(|e| Error::Invalid(e.to_string()))?
        };
        Ok(self.value.insert(value))
    }
}

#[derive(Clone, PartialEq, Eq)]
enum CallKind {
    ChatSend,
    ChatStop,
    SessionCreate,
    SessionOp(String),
    TreeFetch,
    TreeOp(String),
    ModelsOp(String),
    CostsFetch,
    Dynamic { contract: String, method: String },
}

#[derive(Default)]
struct ModelsMembers {
    hold: Option<u64>,
    models_index: Option<u64>,
    default_index: Option<u64>,
    errors_index: Option<u64>,
    enabled_index: Option<u64>,
    scoped_index: Option<u64>,
    set_default_method: Option<u64>,
    set_enabled_method: Option<u64>,
    models_member: Member,
    default_member: Member,
    errors_member: Member,
    enabled_member: Member,
    scoped_member: Member,
}

#[derive(Default)]
struct StatusMembers {
    hold: Option<u64>,
    statuses_index: Option<u64>,
    statuses_member: Member,
}

#[derive(Default)]
struct TreeMembers {
    hold: Option<u64>,
    tree_method: Option<u64>,
    navigate_method: Option<u64>,
    select_method: Option<u64>,
    pending_fetch: bool,
}

#[derive(Default)]
struct CostsMembers {
    hold: Option<u64>,
    costs_method: Option<u64>,
    pending_fetch: bool,
}

pub(super) struct DynamicHold {
    contract: pi_rs_chord::contracts::Contract,
    hold_id: u64,
    params: Value,
    kind: String,
    address: Value,
    bound: bool,
    state_names: HashMap<u64, String>,
    methods: HashMap<String, u64>,
    members: HashMap<String, Member>,
    values: HashMap<String, Value>,
    queued_calls: VecDeque<(String, Value)>,
    error: Option<String>,
}

pub(super) struct View {
    channel: Arc<Channel>,
    incoming: Receiver<Message>,
    catalog: Member,
    default_cwd_member: Member,
    default_env_member: Member,
    transcript: Member,
    stats_member: Member,
    model_member: Member,
    live_shell_member: Member,
    catalog_index: Option<u64>,
    default_cwd_index: Option<u64>,
    default_env_index: Option<u64>,
    create_method: Option<u64>,
    session_methods: HashMap<String, u64>,
    default_cwd: Option<String>,
    default_env: Option<String>,
    next_hold: u64,
    chat: u64,
    sessions: pi_rs_chord::contracts::Contract,
    conversation: pi_rs_chord::contracts::Contract,
    models_contract: pi_rs_chord::contracts::Contract,
    status_contract: pi_rs_chord::contracts::Contract,
    tree_contract: pi_rs_chord::contracts::Contract,
    costs_contract: pi_rs_chord::contracts::Contract,
    send_method: Option<u64>,
    chat_methods: HashMap<String, u64>,
    view_index: Option<u64>,
    stats_index: Option<u64>,
    model_index: Option<u64>,
    live_shell_index: Option<u64>,
    next_call: u64,
    pending_calls: HashMap<u64, CallKind>,
    pending_send: Option<(u64, String)>,
    pending_switch_session: Option<String>,
    queued_create: Option<Option<String>>,
    queued_submit: Option<String>,
    pub(super) accepted: Option<String>,
    pub(super) restored_draft: Option<String>,
    pub(super) flash_notice: Option<String>,
    preferred: Option<String>,
    selected: Option<(String, u64)>,
    rows: Vec<(String, u64, String)>,
    raw_catalog: Vec<Value>,
    title: String,
    pub(super) text: String,
    pub(super) last_message: String,
    models_state: ModelsMembers,
    status_state: StatusMembers,
    tree_state: TreeMembers,
    costs_state: CostsMembers,
    dynamic_holds: HashMap<String, DynamicHold>,
    dynamic_by_hold: HashMap<u64, String>,
    pub(super) dynamic_results: HashMap<(String, String), std::result::Result<Value, String>>,
    pub(super) models_ready: bool,
    pub(super) models_list: Vec<Value>,
    pub(super) default_model: Value,
    pub(super) enabled_models: Vec<Value>,
    pub(super) scoped_models: Vec<Value>,
    pub(super) models_errors: Vec<Value>,
    pub(super) models_error: Option<String>,
    pub(super) statuses: Option<Value>,
    pub(super) status_error: Option<String>,
    pub(super) tree_data: Option<Value>,
    pub(super) tree_error: Option<String>,
    pub(super) costs_data: Option<Value>,
    pub(super) costs_error: Option<String>,
    pub(super) stats_data: Option<Value>,
    pub(super) chat_model: Option<Value>,
    pub(super) live_shell: Option<Value>,
    pub(super) show_thinking: bool,
    pub(super) expanded_tools: bool,
    pub(super) quiet_tools: bool,
    pub(super) output_pad: usize,
    transcript_view: RefCell<Option<TranscriptView>>,
    transcript_dirty: bool,
}

impl View {
    pub(super) fn connect(link: Arc<dyn Link>, preferred: Option<String>) -> Result<Self> {
        let (send, incoming) = mpsc::sync_channel(128);
        let channel = Channel::start(
            link,
            Arc::new(Incoming(send)),
            ChannelOptions {
                codec: Codec::Binary,
                incarnation: *uuid::Uuid::new_v4().as_bytes(),
                calls: None,
                report: Arc::new(|_| {}),
            },
        )?;
        let view = Self {
            channel,
            incoming,
            catalog: Member::default(),
            default_cwd_member: Member::default(),
            default_env_member: Member::default(),
            transcript: Member::default(),
            stats_member: Member::default(),
            model_member: Member::default(),
            live_shell_member: Member::default(),
            catalog_index: None,
            default_cwd_index: None,
            default_env_index: None,
            create_method: None,
            session_methods: HashMap::new(),
            default_cwd: None,
            default_env: None,
            next_hold: 2,
            chat: 2,
            sessions: crate::api::v1::p2_contracts::lookup("p2.sessions")?,
            conversation: crate::api::v1::p2_contracts::lookup("p2.chat")?,
            models_contract: crate::api::v1::p2_contracts::lookup("p2.models")?,
            status_contract: crate::api::v1::p2_contracts::lookup("p2.session-status")?,
            tree_contract: crate::api::v1::p2_contracts::lookup("p2.tree")?,
            costs_contract: crate::api::v1::p2_contracts::lookup("p2.chat-costs")?,
            send_method: None,
            chat_methods: HashMap::new(),
            view_index: None,
            stats_index: None,
            model_index: None,
            live_shell_index: None,
            next_call: 0,
            pending_calls: HashMap::new(),
            pending_send: None,
            pending_switch_session: None,
            queued_create: None,
            queued_submit: None,
            accepted: None,
            restored_draft: None,
            flash_notice: None,
            preferred,
            selected: None,
            rows: vec![],
            raw_catalog: Vec::new(),
            title: String::new(),
            text: "Connecting to sessions…".into(),
            last_message: String::new(),
            models_state: ModelsMembers::default(),
            status_state: StatusMembers::default(),
            tree_state: TreeMembers::default(),
            costs_state: CostsMembers::default(),
            dynamic_holds: HashMap::new(),
            dynamic_by_hold: HashMap::new(),
            dynamic_results: HashMap::new(),
            models_ready: false,
            models_list: Vec::new(),
            default_model: Value::Null,
            enabled_models: Vec::new(),
            scoped_models: Vec::new(),
            models_errors: Vec::new(),
            models_error: None,
            statuses: None,
            status_error: None,
            tree_data: None,
            tree_error: None,
            costs_data: None,
            costs_error: None,
            stats_data: None,
            chat_model: None,
            live_shell: None,
            show_thinking: false,
            expanded_tools: false,
            quiet_tools: true,
            output_pad: 1,
            transcript_view: RefCell::new(None),
            transcript_dirty: true,
        };
        view.send_hold(1, &view.sessions, json!({}), "server", json!({}))?;
        Ok(view)
    }

    pub(super) fn configure_display(
        &mut self,
        quiet_tools: bool,
        output_pad: usize,
        hide_thinking_default: Option<bool>,
    ) {
        self.quiet_tools = quiet_tools;
        self.output_pad = output_pad;
        if let Some(hide) = hide_thinking_default {
            self.show_thinking = !hide;
        }
        self.transcript_dirty = true;
    }

    pub(super) fn toggle_expanded_tools(&mut self) -> bool {
        self.expanded_tools = !self.expanded_tools;
        self.transcript_dirty = true;
        self.expanded_tools
    }

    pub(super) fn toggle_show_thinking(&mut self) -> bool {
        self.show_thinking = !self.show_thinking;
        self.transcript_dirty = true;
        self.show_thinking
    }

    pub(super) fn active_cwd(&self) -> String {
        if let Some((sid, _)) = &self.selected
            && let Some(row) = self
                .raw_catalog
                .iter()
                .find(|r| r["id"].as_str() == Some(sid.as_str()))
            && let Some(cwd) = row["cwd"].as_str()
        {
            return cwd.to_owned();
        }
        self.default_cwd.clone().unwrap_or_default()
    }

    pub(super) fn active_env(&self) -> Option<String> {
        let (sid, _) = self.selected.as_ref()?;
        self.raw_catalog
            .iter()
            .find(|r| r["id"].as_str() == Some(sid.as_str()))
            .and_then(|r| r["env"].as_str())
            .map(str::to_owned)
    }

    pub(super) fn catalog_rows(&self) -> &[Value] {
        &self.raw_catalog
    }

    pub(super) fn session_list(&self) -> &[(String, u64, String)] {
        &self.rows
    }

    pub(super) fn has_active_run(&self) -> bool {
        let Some(val) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        else {
            return false;
        };
        !val["docs"]["pi.live"]["run"].is_null() || !val["docs"]["pi.live"]["generation"].is_null()
    }

    pub(super) fn is_compacting(&self) -> bool {
        let Some(val) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        else {
            return false;
        };
        val["docs"]["pi.live"]["compactions"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    }

    pub(super) fn has_live_shell(&self) -> Option<String> {
        let shell = self.live_shell.as_ref()?;
        if shell["running"].as_bool() == Some(true) {
            shell["id"].as_str().map(str::to_owned)
        } else {
            None
        }
    }

    fn alloc_hold(&mut self) -> u64 {
        self.next_hold += 1;
        self.next_hold
    }

    fn send_hold(
        &self,
        hold: u64,
        contract: &pi_rs_chord::contracts::Contract,
        params: Value,
        kind: &str,
        address: Value,
    ) -> Result<()> {
        self.channel.send(Message::new(Body::Hold {
            hold,
            type_name: contract.name().into(),
            version: contract.version(),
            fingerprint: u64::from_str_radix(contract.fingerprint(), 16)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_be_bytes(),
            params: Part::Text(params.to_string()),
            address_kind: kind.into(),
            address_params: Box::new(JsonValue::from_serde(address)),
            principal: None,
            incarnation: None,
            chain: None,
            limit: None,
        }))
    }

    fn call_method(&mut self, hold: u64, method: u64, args: Value, kind: CallKind) -> Result<u64> {
        self.next_call += 1;
        let call = self.next_call;
        let args = if args.is_null() {
            None
        } else {
            Some(Payload {
                part: Part::Text(args.to_string()),
                segments: None,
            })
        };
        self.channel.send(Message::new(Body::Call {
            call,
            hold,
            method,
            args,
        }))?;
        self.pending_calls.insert(call, kind);
        Ok(call)
    }

    pub(super) fn create_session(&mut self, title: Option<&str>) -> Result<()> {
        self.create_session_at(None, None, title)
    }

    pub(super) fn create_session_at(
        &mut self,
        env_name: Option<&str>,
        cwd_override: Option<&str>,
        title: Option<&str>,
    ) -> Result<()> {
        let Some(method) = self.create_method else {
            self.queued_create = Some(title.map(str::to_owned));
            return Ok(());
        };
        let cwd = cwd_override
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| self.default_cwd.clone())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "/".into())
            });
        let mut args = json!({
            "cwd": cwd,
            "title": title.unwrap_or(""),
        });
        if let Some(env_val) = env_name.filter(|s| !s.is_empty() && *s != "local") {
            args["env"] = json!(env_val);
        }
        self.call_method(1, method, args, CallKind::SessionCreate)?;
        Ok(())
    }

    pub(super) fn switch_session(&mut self, id: &str) -> Result<()> {
        if let Some(row) = self.rows.iter().find(|(rid, _, _)| rid == id).cloned() {
            self.select(Some(row))?;
            self.render();
            Ok(())
        } else {
            Err(Error::Invalid(format!("Session not found: {id}")))
        }
    }

    pub(super) fn call_session_op(&mut self, method_name: &str, args: Value) -> Result<()> {
        let method = self
            .session_methods
            .get(method_name)
            .copied()
            .ok_or_else(|| Error::Invalid(format!("Session method unavailable: {method_name}")))?;
        self.call_method(1, method, args, CallKind::SessionOp(method_name.to_owned()))?;
        Ok(())
    }

    pub(super) fn call_chat_op(&mut self, method_name: &str, args: Value) -> Result<()> {
        if self.selected.is_none() {
            return Err(Error::Invalid("No active session".into()));
        }
        let method = self
            .chat_methods
            .get(method_name)
            .copied()
            .ok_or_else(|| Error::Invalid(format!("Chat method unavailable: {method_name}")))?;
        let kind = if method_name == "stop" {
            CallKind::ChatStop
        } else {
            CallKind::SessionOp(format!("chat.{method_name}"))
        };
        self.call_method(self.chat, method, args, kind)?;
        Ok(())
    }

    pub(super) fn stop_chat(&mut self) -> Result<()> {
        self.call_chat_op("stop", Value::Null)
    }

    pub(super) fn abort_compaction(&mut self) -> Result<()> {
        self.call_chat_op("abortCompaction", Value::Null)
    }

    pub(super) fn compact_chat(&mut self, instructions: Option<&str>) -> Result<()> {
        let args = match instructions.filter(|s| !s.trim().is_empty()) {
            Some(inst) => json!({ "instructions": inst }),
            None => json!({}),
        };
        self.call_chat_op("compact", args)
    }

    pub(super) fn reset_chat(&mut self, handoff: Option<&str>) -> Result<()> {
        let args = match handoff.filter(|s| !s.trim().is_empty()) {
            Some(text) => json!({ "handoff": text }),
            None => json!({}),
        };
        self.call_chat_op("reset", args)
    }

    pub(super) fn run_shell(&mut self, command: &str, include_in_context: bool) -> Result<()> {
        self.call_chat_op(
            "shell",
            json!({
                "command": command,
                "includeInContext": include_in_context,
            }),
        )
    }

    pub(super) fn abort_shell(&mut self, shell_id: &str) -> Result<()> {
        self.call_chat_op("abortShell", json!({ "shellId": shell_id }))
    }

    pub(super) fn configure_chat(&mut self, args: Value) -> Result<()> {
        self.call_chat_op("configure", args)
    }

    pub(super) fn set_default_model(&mut self, provider: &str, model_id: &str) -> Result<()> {
        let _ = self.request_models();
        let hold = self
            .models_state
            .hold
            .ok_or_else(|| Error::Invalid("Models contract not held".into()))?;
        let method = self
            .models_state
            .set_default_method
            .ok_or_else(|| Error::Invalid("Models setDefault unavailable".into()))?;
        self.call_method(
            hold,
            method,
            json!({ "provider": provider, "modelId": model_id }),
            CallKind::ModelsOp("setDefault".into()),
        )?;
        Ok(())
    }

    pub(super) fn navigate_tree(&mut self, entry: u64) -> Result<()> {
        let hold = self
            .tree_state
            .hold
            .ok_or_else(|| Error::Invalid("Tree contract not held".into()))?;
        let method = self
            .tree_state
            .navigate_method
            .ok_or_else(|| Error::Invalid("Tree navigate unavailable".into()))?;
        self.call_method(
            hold,
            method,
            json!({ "entry": entry }),
            CallKind::TreeOp("navigate".into()),
        )?;
        Ok(())
    }

    pub(super) fn ensure_hold(
        &mut self,
        contract_name: &str,
        params: Value,
        kind: &str,
        address: Value,
    ) -> Result<()> {
        if let Some(existing) = self.dynamic_holds.get(contract_name)
            && existing.params == params
            && existing.kind == kind
            && existing.address == address
        {
            return Ok(());
        }
        if let Some(old) = self.dynamic_holds.remove(contract_name) {
            self.dynamic_by_hold.remove(&old.hold_id);
            let _ = self
                .channel
                .send(Message::new(Body::Release { hold: old.hold_id }));
        }
        let contract = crate::api::v1::p2_contracts::lookup(contract_name)?;
        let hold_id = self.alloc_hold();
        self.send_hold(hold_id, &contract, params.clone(), kind, address.clone())?;
        self.dynamic_by_hold
            .insert(hold_id, contract_name.to_owned());
        self.dynamic_holds.insert(
            contract_name.to_owned(),
            DynamicHold {
                contract,
                hold_id,
                params,
                kind: kind.to_owned(),
                address,
                bound: false,
                state_names: HashMap::new(),
                methods: HashMap::new(),
                members: HashMap::new(),
                values: HashMap::new(),
                queued_calls: VecDeque::new(),
                error: None,
            },
        );
        Ok(())
    }

    pub(super) fn dynamic_state(&self, contract_name: &str, member: &str) -> Option<&Value> {
        self.dynamic_holds
            .get(contract_name)
            .and_then(|h| h.values.get(member))
    }

    pub(super) fn dynamic_error(&self, contract_name: &str) -> Option<&str> {
        self.dynamic_holds
            .get(contract_name)
            .and_then(|h| h.error.as_deref())
    }

    pub(super) fn call_dynamic(
        &mut self,
        contract_name: &str,
        method_name: &str,
        args: Value,
    ) -> Result<()> {
        let Some(entry) = self.dynamic_holds.get_mut(contract_name) else {
            return Err(Error::Invalid(format!(
                "Contract {contract_name} not held yet"
            )));
        };
        if !entry.bound {
            entry.queued_calls.push_back((method_name.to_owned(), args));
            return Ok(());
        }
        let Some(&method) = entry.methods.get(method_name) else {
            return Err(Error::Invalid(format!(
                "{contract_name}.{method_name} unavailable"
            )));
        };
        let hold_id = entry.hold_id;
        self.call_method(
            hold_id,
            method,
            args,
            CallKind::Dynamic {
                contract: contract_name.to_owned(),
                method: method_name.to_owned(),
            },
        )?;
        Ok(())
    }

    pub(super) fn request_models(&mut self) -> Result<()> {
        self.models_error = None;
        if self.models_state.hold.is_none() {
            let hold = self.alloc_hold();
            self.models_state.hold = Some(hold);
            self.send_hold(hold, &self.models_contract, json!({}), "server", json!({}))?;
        }
        Ok(())
    }

    pub(super) fn request_status(&mut self) -> Result<()> {
        self.status_error = None;
        if self.status_state.hold.is_none() {
            let hold = self.alloc_hold();
            self.status_state.hold = Some(hold);
            self.send_hold(hold, &self.status_contract, json!({}), "server", json!({}))?;
        }
        Ok(())
    }

    pub(super) fn request_tree(&mut self) -> Result<()> {
        let Some((session_id, _)) = self.selected.clone() else {
            self.tree_error = Some("No active session".into());
            return Ok(());
        };
        self.tree_error = None;
        if let (Some(hold), Some(method)) = (self.tree_state.hold, self.tree_state.tree_method) {
            self.call_method(hold, method, json!({}), CallKind::TreeFetch)?;
        } else {
            self.tree_state.pending_fetch = true;
            if self.tree_state.hold.is_none() {
                let hold = self.alloc_hold();
                self.tree_state.hold = Some(hold);
                self.send_hold(
                    hold,
                    &self.tree_contract,
                    json!({"session": session_id}),
                    "session",
                    json!({"id": session_id}),
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn request_costs(&mut self) -> Result<()> {
        let Some((session_id, conversation)) = self.selected.clone() else {
            self.costs_error = Some("No active session".into());
            return Ok(());
        };
        self.costs_error = None;
        if let (Some(hold), Some(method)) = (self.costs_state.hold, self.costs_state.costs_method) {
            self.call_method(hold, method, Value::Null, CallKind::CostsFetch)?;
        } else {
            self.costs_state.pending_fetch = true;
            if self.costs_state.hold.is_none() {
                let hold = self.alloc_hold();
                self.costs_state.hold = Some(hold);
                self.send_hold(
                    hold,
                    &self.costs_contract,
                    json!({"session": session_id, "conversation": conversation}),
                    "session",
                    json!({"id": session_id}),
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn submit(&mut self, text: &str) -> Result<()> {
        if self.pending_send.is_some() || text.trim().is_empty() {
            return Ok(());
        }
        let Some(method) = self.send_method else {
            self.queued_submit = Some(text.to_owned());
            return Ok(());
        };
        let request =
            json!({"text":text,"requestId":uuid::Uuid::new_v4().to_string(),"whenBusy":"steer"});
        let call = self.call_method(self.chat, method, request, CallKind::ChatSend)?;
        self.pending_send = Some((call, text.to_owned()));
        Ok(())
    }

    pub(super) fn step(&mut self) -> Result<bool> {
        if self.channel.closed() {
            return Err(Error::Closed);
        }
        let mut changed = false;
        loop {
            let msg = match self.incoming.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => break,
                Err(_) => return Err(Error::Closed),
            };
            if self.handle_message(msg)? {
                changed = true;
            }
        }
        Ok(changed)
    }

    fn handle_message(&mut self, msg: Message) -> Result<bool> {
        let mut ack = false;
        let changed = match msg.body {
            Body::Bound(entries) => {
                ack = true;
                for e in entries {
                    if e.hold == self.chat && self.selected.is_some() {
                        let names = bound_names(&self.conversation, e.variant)?;
                        self.chat_methods.clear();
                        for (idx, name) in names.methods.iter().enumerate() {
                            self.chat_methods.insert(name.clone(), idx as u64);
                        }
                        self.send_method = self.chat_methods.get("send").copied();
                        self.view_index = names
                            .state
                            .iter()
                            .position(|n| n == "view")
                            .map(|i| i as u64);
                        self.stats_index = names
                            .state
                            .iter()
                            .position(|n| n == "stats")
                            .map(|i| i as u64);
                        self.model_index = names
                            .state
                            .iter()
                            .position(|n| n == "model")
                            .map(|i| i as u64);
                        self.live_shell_index = names
                            .state
                            .iter()
                            .position(|n| n == "liveShell")
                            .map(|i| i as u64);
                        if let Some(text) = self.queued_submit.take() {
                            self.submit(&text)?;
                        }
                    } else if e.hold == 1 {
                        let names = bound_names(&self.sessions, e.variant)?;
                        self.catalog_index = names
                            .state
                            .iter()
                            .position(|n| n == "sessions")
                            .map(|index| index as u64);
                        self.default_cwd_index = names
                            .state
                            .iter()
                            .position(|n| n == "defaultCwd")
                            .map(|index| index as u64);
                        self.default_env_index = names
                            .state
                            .iter()
                            .position(|n| n == "defaultEnv")
                            .map(|index| index as u64);
                        self.session_methods.clear();
                        for (idx, name) in names.methods.iter().enumerate() {
                            self.session_methods.insert(name.clone(), idx as u64);
                        }
                        self.create_method = self.session_methods.get("create").copied();
                        if let Some(title) = self.queued_create.take() {
                            self.create_session(title.as_deref())?;
                        }
                    } else if Some(e.hold) == self.models_state.hold {
                        let names = bound_names(&self.models_contract, e.variant)?;
                        self.models_state.models_index = names
                            .state
                            .iter()
                            .position(|n| n == "models")
                            .map(|i| i as u64);
                        self.models_state.default_index = names
                            .state
                            .iter()
                            .position(|n| n == "defaultModel")
                            .map(|i| i as u64);
                        self.models_state.errors_index = names
                            .state
                            .iter()
                            .position(|n| n == "errors")
                            .map(|i| i as u64);
                        self.models_state.enabled_index = names
                            .state
                            .iter()
                            .position(|n| n == "enabledModels")
                            .map(|i| i as u64);
                        self.models_state.scoped_index = names
                            .state
                            .iter()
                            .position(|n| n == "scopedModels")
                            .map(|i| i as u64);
                        self.models_state.set_default_method = names
                            .methods
                            .iter()
                            .position(|n| n == "setDefault")
                            .map(|i| i as u64);
                        self.models_state.set_enabled_method = names
                            .methods
                            .iter()
                            .position(|n| n == "setEnabledModels")
                            .map(|i| i as u64);
                        self.models_ready = true;
                    } else if Some(e.hold) == self.status_state.hold {
                        let names = bound_names(&self.status_contract, e.variant)?;
                        self.status_state.statuses_index = names
                            .state
                            .iter()
                            .position(|n| n == "statuses")
                            .map(|i| i as u64);
                    } else if Some(e.hold) == self.tree_state.hold {
                        let names = bound_names(&self.tree_contract, e.variant)?;
                        self.tree_state.tree_method = names
                            .methods
                            .iter()
                            .position(|n| n == "tree")
                            .map(|i| i as u64);
                        self.tree_state.navigate_method = names
                            .methods
                            .iter()
                            .position(|n| n == "navigate")
                            .map(|i| i as u64);
                        self.tree_state.select_method = names
                            .methods
                            .iter()
                            .position(|n| n == "select")
                            .map(|i| i as u64);
                        if std::mem::take(&mut self.tree_state.pending_fetch)
                            && let Some(method) = self.tree_state.tree_method
                        {
                            self.call_method(e.hold, method, json!({}), CallKind::TreeFetch)?;
                        }
                    } else if Some(e.hold) == self.costs_state.hold {
                        let names = bound_names(&self.costs_contract, e.variant)?;
                        self.costs_state.costs_method = names
                            .methods
                            .iter()
                            .position(|n| n == "costs")
                            .map(|i| i as u64);
                        if std::mem::take(&mut self.costs_state.pending_fetch)
                            && let Some(method) = self.costs_state.costs_method
                        {
                            self.call_method(e.hold, method, Value::Null, CallKind::CostsFetch)?;
                        }
                    } else if let Some(cname) = self.dynamic_by_hold.get(&e.hold).cloned()
                        && let Some(dyn_hold) = self.dynamic_holds.get_mut(&cname)
                    {
                        let names = bound_names(&dyn_hold.contract, e.variant)?;
                        dyn_hold.state_names.clear();
                        for (idx, sname) in names.state.iter().enumerate() {
                            dyn_hold.state_names.insert(idx as u64, sname.clone());
                        }
                        dyn_hold.methods.clear();
                        for (idx, mname) in names.methods.iter().enumerate() {
                            dyn_hold.methods.insert(mname.clone(), idx as u64);
                        }
                        dyn_hold.bound = true;
                        dyn_hold.error = None;
                        let queued: Vec<_> = dyn_hold.queued_calls.drain(..).collect();
                        for (mname, args) in queued {
                            if let Some(&method) = self
                                .dynamic_holds
                                .get(&cname)
                                .and_then(|h| h.methods.get(&mname))
                            {
                                self.call_method(
                                    e.hold,
                                    method,
                                    args,
                                    CallKind::Dynamic {
                                        contract: cname.clone(),
                                        method: mname,
                                    },
                                )?;
                            }
                        }
                    }
                    for (i, part) in e.members {
                        self.publish(e.hold, i, &part, true)?;
                    }
                }
                true
            }
            Body::State(entries) => {
                ack = true;
                for e in entries {
                    for (i, part) in e.members {
                        self.publish(e.hold, i, &part, false)?;
                    }
                }
                true
            }
            Body::Result { call, value } => {
                if let Some(kind) = self.pending_calls.remove(&call) {
                    let parsed = value
                        .map(|payload| payload.part.parse(Limits::for_link(false))?.to_serde())
                        .transpose()?;
                    match kind {
                        CallKind::ChatSend => {
                            if self
                                .pending_send
                                .as_ref()
                                .is_some_and(|(id, _)| *id == call)
                            {
                                self.accepted = self.pending_send.take().map(|(_, text)| text);
                            }
                        }
                        CallKind::ChatStop => {
                            if let Some(val) = parsed {
                                if let Some(restored) = val["restored"].as_str() {
                                    self.restored_draft = Some(restored.to_owned());
                                } else if let Some(arr) = val["restored"].as_array() {
                                    let joined: String = arr
                                        .iter()
                                        .filter_map(|v| v.as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    if !joined.is_empty() {
                                        self.restored_draft = Some(joined);
                                    }
                                }
                            }
                        }
                        CallKind::SessionCreate => {
                            if let Some(row) = parsed
                                && let Some(id) = row["id"].as_str()
                            {
                                let main = row["main"].as_u64().unwrap_or(1);
                                let title = clean(row["title"].as_str().unwrap_or(id));
                                self.pending_switch_session = Some(id.to_owned());
                                if !self.rows.iter().any(|(existing, _, _)| existing == id) {
                                    self.rows.insert(0, (id.to_owned(), main, title.clone()));
                                }
                                self.select(Some((id.to_owned(), main, title)))?;
                                self.render();
                            }
                        }
                        CallKind::SessionOp(op) => {
                            self.flash_notice = Some(format!("Session {op} completed"));
                        }
                        CallKind::TreeOp(_) => {
                            let _ = self.request_tree();
                        }
                        CallKind::ModelsOp(op) => {
                            self.flash_notice = Some(format!("Models {op} updated"));
                        }
                        CallKind::TreeFetch => {
                            if let Some(tree) = parsed {
                                self.tree_data = Some(tree);
                                self.tree_error = None;
                            }
                        }
                        CallKind::CostsFetch => {
                            if let Some(costs) = parsed {
                                self.costs_data = Some(costs);
                                self.costs_error = None;
                            }
                        }
                        CallKind::Dynamic { contract, method } => {
                            self.dynamic_results
                                .insert((contract, method), Ok(parsed.unwrap_or(Value::Null)));
                        }
                    }
                    true
                } else {
                    false
                }
            }
            Body::Error { call, error } => {
                if let Some(kind) = self.pending_calls.remove(&call) {
                    let message = error
                        .to_serde()
                        .ok()
                        .and_then(|v| {
                            v["error"]["reason"]
                                .as_str()
                                .or_else(|| v["message"].as_str())
                                .map(str::to_owned)
                                .or_else(|| Some(v.to_string()))
                        })
                        .unwrap_or_else(|| "Request failed".into());
                    match kind {
                        CallKind::ChatSend => {
                            if self
                                .pending_send
                                .as_ref()
                                .is_some_and(|(id, _)| *id == call)
                            {
                                self.pending_send = None;
                            }
                            self.text.push_str("\nSend failed");
                        }
                        CallKind::ChatStop | CallKind::SessionOp(_) | CallKind::ModelsOp(_) => {
                            self.flash_notice = Some(message);
                        }
                        CallKind::SessionCreate => {
                            self.text
                                .push_str(&format!("\nCreate session failed: {message}"));
                            self.flash_notice = Some(message);
                        }
                        CallKind::TreeFetch | CallKind::TreeOp(_) => {
                            self.tree_error = Some(message);
                        }
                        CallKind::CostsFetch => {
                            self.costs_error = Some(message);
                        }
                        CallKind::Dynamic { contract, method } => {
                            self.flash_notice = Some(message.clone());
                            self.dynamic_results
                                .insert((contract, method), Err(message));
                        }
                    }
                    true
                } else {
                    false
                }
            }
            Body::Loading { hold } | Body::Failed { hold, cause: 2, .. }
                if hold == 1 || hold == self.chat =>
            {
                if hold == 1 {
                    self.rows.clear();
                    self.raw_catalog.clear();
                    self.select(None)?;
                }
                self.send_method = None;
                self.transcript = Member::default();
                self.transcript_dirty = true;
                self.stats_member = Member::default();
                self.stats_data = None;
                self.last_message.clear();
                self.text = "Waiting for session…".into();
                true
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if hold == 1 || hold == self.chat =>
            {
                return Err(Error::Invalid("Session access ended or unavailable".into()));
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if Some(hold) == self.models_state.hold =>
            {
                self.models_state.hold = None;
                self.models_error = Some("Models service unavailable".into());
                true
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if Some(hold) == self.status_state.hold =>
            {
                self.status_state.hold = None;
                self.status_error = Some("Session status service unavailable".into());
                true
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if Some(hold) == self.tree_state.hold =>
            {
                self.tree_state.hold = None;
                self.tree_state.tree_method = None;
                self.tree_error = Some("Tree service unavailable".into());
                true
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if Some(hold) == self.costs_state.hold =>
            {
                self.costs_state.hold = None;
                self.costs_state.costs_method = None;
                self.costs_error = Some("Costs service unavailable".into());
                true
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if self.dynamic_by_hold.contains_key(&hold) =>
            {
                if let Some(cname) = self.dynamic_by_hold.remove(&hold)
                    && let Some(dyn_hold) = self.dynamic_holds.get_mut(&cname)
                {
                    dyn_hold.bound = false;
                    dyn_hold.error = Some(format!("{cname} unavailable"));
                }
                true
            }
            _ => false,
        };
        if ack {
            self.channel
                .send(Message::new(Body::Ack { events: None }))?;
        }
        Ok(changed)
    }

    fn publish(&mut self, hold: u64, index: u64, part: &Part, snapshot: bool) -> Result<()> {
        if hold == 1 && Some(index) == self.catalog_index {
            let catalog = self.catalog.read(part, snapshot)?.to_serde()?;
            let rows = catalog
                .as_array()
                .ok_or_else(|| Error::Invalid("Invalid sessions catalog".into()))?;
            self.raw_catalog = rows.clone();
            self.rows = rows
                .iter()
                .filter(|r| r["archivedAt"].is_null())
                .filter_map(|r| {
                    let id = r["id"].as_str()?;
                    if self.preferred.as_deref().is_some_and(|wanted| wanted != id) {
                        return None;
                    }
                    Some((
                        id.to_owned(),
                        r["main"].as_u64().unwrap_or(1),
                        clean(r["title"].as_str().unwrap_or(id)),
                    ))
                })
                .collect();
            let switch_target = self
                .pending_switch_session
                .as_deref()
                .and_then(|target| self.rows.iter().find(|(id, _, _)| id == target).cloned());
            if switch_target.is_some() {
                self.pending_switch_session = None;
            }
            let next = switch_target
                .or_else(|| {
                    self.rows
                        .iter()
                        .find(|(id, _, _)| {
                            self.selected
                                .as_ref()
                                .is_some_and(|(current, _)| id == current)
                        })
                        .cloned()
                })
                .or_else(|| self.rows.first().cloned());
            self.select(next)?;
            self.render();
        } else if hold == 1 && Some(index) == self.default_cwd_index {
            let val = self.default_cwd_member.read(part, snapshot)?.to_serde()?;
            self.default_cwd = val.as_str().map(str::to_owned);
        } else if hold == 1 && Some(index) == self.default_env_index {
            let val = self.default_env_member.read(part, snapshot)?.to_serde()?;
            self.default_env = val.as_str().map(str::to_owned);
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.view_index {
            self.transcript.read(part, snapshot)?;
            self.transcript_dirty = true;
            self.render();
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.stats_index {
            self.stats_data = Some(self.stats_member.read(part, snapshot)?.to_serde()?);
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.model_index {
            self.chat_model = Some(self.model_member.read(part, snapshot)?.to_serde()?);
        } else if self.selected.is_some()
            && hold == self.chat
            && Some(index) == self.live_shell_index
        {
            self.live_shell = Some(self.live_shell_member.read(part, snapshot)?.to_serde()?);
        } else if Some(hold) == self.models_state.hold {
            if Some(index) == self.models_state.models_index {
                let val = self
                    .models_state
                    .models_member
                    .read(part, snapshot)?
                    .to_serde()?;
                self.models_list = val.as_array().cloned().unwrap_or_default();
                self.models_ready = true;
            } else if Some(index) == self.models_state.default_index {
                self.default_model = self
                    .models_state
                    .default_member
                    .read(part, snapshot)?
                    .to_serde()?;
                self.models_ready = true;
            } else if Some(index) == self.models_state.errors_index {
                let val = self
                    .models_state
                    .errors_member
                    .read(part, snapshot)?
                    .to_serde()?;
                self.models_errors = val.as_array().cloned().unwrap_or_default();
            } else if Some(index) == self.models_state.enabled_index {
                let val = self
                    .models_state
                    .enabled_member
                    .read(part, snapshot)?
                    .to_serde()?;
                self.enabled_models = val.as_array().cloned().unwrap_or_default();
            } else if Some(index) == self.models_state.scoped_index {
                let val = self
                    .models_state
                    .scoped_member
                    .read(part, snapshot)?
                    .to_serde()?;
                self.scoped_models = val.as_array().cloned().unwrap_or_default();
            }
        } else if Some(hold) == self.status_state.hold
            && Some(index) == self.status_state.statuses_index
        {
            self.statuses = Some(
                self.status_state
                    .statuses_member
                    .read(part, snapshot)?
                    .to_serde()?,
            );
        } else if let Some(cname) = self.dynamic_by_hold.get(&hold).cloned()
            && let Some(dyn_hold) = self.dynamic_holds.get_mut(&cname)
            && let Some(member_name) = dyn_hold.state_names.get(&index).cloned()
        {
            let member = dyn_hold.members.entry(member_name.clone()).or_default();
            let val = member.read(part, snapshot)?.to_serde()?;
            dyn_hold.values.insert(member_name, val);
        }
        Ok(())
    }

    fn select(&mut self, row: Option<(String, u64, String)>) -> Result<()> {
        let next = row.as_ref().map(|(id, c, _)| (id.clone(), *c));
        self.title = row
            .map(|(_, _, title)| title)
            .unwrap_or_else(|| "No sessions".into());
        if next != self.selected {
            if self.selected.is_some() {
                self.channel
                    .send(Message::new(Body::Release { hold: self.chat }))?;
            }
            if let Some(tree_hold) = self.tree_state.hold.take() {
                let _ = self
                    .channel
                    .send(Message::new(Body::Release { hold: tree_hold }));
            }
            if let Some(costs_hold) = self.costs_state.hold.take() {
                let _ = self
                    .channel
                    .send(Message::new(Body::Release { hold: costs_hold }));
            }
            self.tree_state.tree_method = None;
            self.tree_state.navigate_method = None;
            self.tree_state.select_method = None;
            self.tree_state.pending_fetch = false;
            self.tree_data = None;
            self.tree_error = None;
            self.costs_state.costs_method = None;
            self.costs_state.pending_fetch = false;
            self.costs_data = None;
            self.costs_error = None;

            self.send_method = None;
            self.chat_methods.clear();
            self.view_index = None;
            self.stats_index = None;
            self.model_index = None;
            self.live_shell_index = None;
            self.pending_send = None;
            self.queued_submit = None;
            self.transcript = Member::default();
            self.transcript_dirty = true;
            *self.transcript_view.borrow_mut() = None;
            self.stats_member = Member::default();
            self.stats_data = None;
            self.chat_model = None;
            self.live_shell = None;
            self.last_message.clear();
            self.chat = self.alloc_hold();
            if let Some((id, c)) = &next {
                self.send_hold(
                    self.chat,
                    &self.conversation,
                    json!({"session": id, "conversation": c}),
                    "session",
                    json!({"id": id}),
                )?;
            }
            self.selected = next;
        }
        Ok(())
    }

    pub(super) fn destination(&self) -> Option<&(String, u64)> {
        self.selected.as_ref()
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    fn render(&mut self) {
        self.text = self.title.clone();
        self.last_message.clear();
        if let Some(v) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        {
            for entry in v["entries"].as_array().into_iter().flatten() {
                if let Some(text) = entry["data"]["text"].as_str() {
                    self.append(text);
                } else {
                    for m in entry["model"].as_array().into_iter().flatten() {
                        self.model(m);
                    }
                }
            }
            self.model(&v["docs"]["pi.live"]["generation"]["message"]);
        }
    }

    pub(super) fn render_transcript_lines(
        &self,
        theme_ref: &ThemeRef,
        width: usize,
    ) -> Vec<String> {
        let width = width.max(1);
        let theme = theme_ref.current();

        if let Some(v) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        {
            let transcript = build_transcript(&v);
            let mut tv_slot = self.transcript_view.borrow_mut();
            let view = tv_slot.get_or_insert_with(|| {
                let mut entry_renderers = indexmap::IndexMap::new();
                entry_renderers.insert(
                    "p2.shell".to_string(),
                    Box::new(shell_entry_renderer) as pi_rs_p2_tui::components::TuiEntryRenderer,
                );
                let tool_renderers = create_default_tool_renderers();
                transcript_component(
                    &transcript,
                    self.show_thinking,
                    self.expanded_tools,
                    TranscriptComponentOptions {
                        theme: Some(theme_ref.clone()),
                        tool_renderers,
                        entry_renderers,
                        quiet_tools: Some(self.quiet_tools),
                        cwd: self.active_cwd(),
                        output_pad: Some(self.output_pad),
                        mermaid_settings: None,
                        key_text: Some(Rc::new(|id| match id {
                            "app.tools.expand" => "ctrl+o".to_string(),
                            _ => id.to_string(),
                        })),
                    },
                )
            });
            view.expanded = self.expanded_tools;
            view.show_thinking = self.show_thinking;
            view.quiet_tools = self.quiet_tools;
            view.output_pad = self.output_pad;
            view.cwd = self.active_cwd();
            view.sync(&transcript);
            let mut rendered = pi_rs_tui::Component::render(view, width);
            if rendered.last().is_some_and(String::is_empty) {
                rendered.pop();
            }
            return rendered;
        }

        let mut lines = Vec::new();
        if let Some((_, body)) = self.text.split_once('\n') {
            for raw in body.lines() {
                for wrapped in pi_rs_tui::wrap_text_with_ansi(raw, width.saturating_sub(2).max(1)) {
                    lines.push(format!(" {}", theme.fg("muted", &wrapped)));
                }
            }
        }
        lines
    }

    pub(super) fn user_messages(&self) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        let Some(v) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        else {
            return out;
        };
        for (idx, entry) in v["entries"].as_array().into_iter().flatten().enumerate() {
            let id = entry["id"].as_u64().unwrap_or(idx as u64 + 1) as u32;
            if let Some(text) = entry["data"]["text"].as_str() {
                let cleaned = clean(text);
                if !cleaned.is_empty() {
                    out.push((id, cleaned));
                }
            } else if let Some(first) = entry["model"].as_array().and_then(|a| a.first())
                && first["role"] == "user"
                && let Some(text) = first["content"].as_str()
            {
                let cleaned = clean(text);
                if !cleaned.is_empty() {
                    out.push((id, cleaned));
                }
            }
        }
        out
    }

    fn model(&mut self, m: &Value) {
        if let Some(s) = m["content"].as_str() {
            self.append(s);
            if m["role"] == "assistant" {
                self.last_message = clean(s);
            }
        } else {
            for b in m["content"].as_array().into_iter().flatten() {
                if b["type"] == "text"
                    && let Some(s) = b["text"].as_str()
                {
                    self.append(s);
                    if m["role"] == "assistant" {
                        self.last_message = clean(s);
                    }
                } else if b["type"] == "toolCall"
                    && let Some(name) = b["name"].as_str()
                {
                    self.append(&format!("[tool: {name}]"));
                }
            }
        }
    }

    fn append(&mut self, text: &str) {
        let text = clean(text);
        self.text.push('\n');
        self.text.push_str(&text);
    }
}

fn value_to_tool_args(val: &Value) -> ToolArgs {
    val.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn tool_result_text(m: &Value) -> String {
    if let Some(s) = m["content"].as_str() {
        return clean(s);
    }
    let mut parts = Vec::new();
    for block in m["content"].as_array().into_iter().flatten() {
        if let Some(s) = block["text"].as_str() {
            parts.push(clean(s));
        }
    }
    parts.join("\n")
}

fn build_transcript(v: &Value) -> Transcript {
    let mut items = Vec::new();
    let entries = v["entries"].as_array().map(Vec::as_slice).unwrap_or(&[]);

    // Collect toolResults across entries and live state keyed by toolCallId.
    let mut results_by_id: HashMap<String, (bool, String, Option<Value>)> = HashMap::new();
    for entry in entries {
        for m in entry["model"].as_array().into_iter().flatten() {
            if m["role"].as_str() == Some("toolResult")
                && let Some(id) = m["toolCallId"].as_str()
            {
                let is_error = m["isError"].as_bool().unwrap_or(false);
                let out = tool_result_text(m);
                let details = (!m["details"].is_null()).then(|| m["details"].clone());
                results_by_id.insert(id.to_owned(), (is_error, out, details));
            }
        }
    }
    for live_tool in v["docs"]["pi.live"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(id) = live_tool["id"].as_str() {
            let status_str = live_tool["status"].as_str().unwrap_or("running");
            let is_error = status_str == "error";
            let out = live_tool["output"].as_str().unwrap_or_default().to_owned();
            let details = (!live_tool["details"].is_null()).then(|| live_tool["details"].clone());
            results_by_id
                .entry(id.to_owned())
                .or_insert((is_error, out, details));
        }
    }

    for (idx, entry) in entries.iter().enumerate() {
        let entry_id = entry["id"].as_u64();
        let kind = entry["kind"].as_str().unwrap_or_default();
        if kind == "p2.shell" {
            items.push(Item::Entry {
                key: format!("{idx}-shell"),
                entry: EntryRecord {
                    id: entry_id.unwrap_or(idx as u64),
                    kind: "p2.shell".to_owned(),
                    data: entry["data"].clone(),
                },
            });
            continue;
        }
        if matches!(kind, "compaction" | "branchSummary") {
            let label = if kind == "compaction" {
                "Compaction"
            } else {
                "Branch summary"
            }
            .to_owned();
            let detail = entry["data"]["summary"].as_str().map(clean);
            items.push(Item::Marker {
                key: format!("{idx}-{kind}"),
                label,
                detail,
            });
            continue;
        }
        if let Some(text) = entry["data"]["text"].as_str() {
            let cleaned = clean(text);
            if !cleaned.is_empty() {
                items.push(Item::User {
                    key: format!("{idx}-u"),
                    entry_id,
                    text: cleaned,
                });
            }
        }
        for (midx, m) in entry["model"].as_array().into_iter().flatten().enumerate() {
            let role = m["role"].as_str().unwrap_or_default();
            if role == "user" {
                let text = tool_result_text(m);
                if !text.is_empty() {
                    items.push(Item::User {
                        key: format!("{idx}-mu-{midx}"),
                        entry_id,
                        text,
                    });
                }
            } else if role == "assistant" {
                let (content, tools) = extract_assistant_blocks(m, &results_by_id, false);
                if !content.is_empty() || !tools.is_empty() {
                    items.push(Item::Assistant {
                        key: format!("{idx}-ma-{midx}"),
                        entry_id,
                        message: AssistantMessage { content },
                        tools,
                        streaming: false,
                    });
                }
                if let Some(err) = m["errorMessage"].as_str().filter(|s| !s.is_empty()) {
                    items.push(Item::Failure {
                        key: format!("{idx}-err-{midx}"),
                        text: clean(err),
                    });
                }
            }
        }
    }

    let live_msg = &v["docs"]["pi.live"]["generation"]["message"];
    if !live_msg.is_null() {
        let (content, _) = extract_assistant_blocks(live_msg, &results_by_id, true);
        if !content.is_empty() {
            items.push(Item::Assistant {
                key: "live-assistant".to_owned(),
                entry_id: None,
                message: AssistantMessage { content },
                tools: Vec::new(),
                streaming: true,
            });
        }
    }

    Transcript { items }
}

fn extract_assistant_blocks(
    m: &Value,
    results_by_id: &HashMap<String, (bool, String, Option<Value>)>,
    streaming: bool,
) -> (Vec<ContentBlock>, Vec<ToolView>) {
    let mut blocks = Vec::new();
    let mut tools = Vec::new();
    if let Some(s) = m["content"].as_str() {
        let cleaned = clean(s);
        if !cleaned.is_empty() {
            blocks.push(ContentBlock::Text { text: cleaned });
        }
        return (blocks, tools);
    }
    for b in m["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => {
                if let Some(s) = b["text"].as_str() {
                    blocks.push(ContentBlock::Text { text: clean(s) });
                }
            }
            Some("thinking") => {
                if let Some(s) = b["thinking"].as_str() {
                    blocks.push(ContentBlock::Thinking { thinking: clean(s) });
                }
            }
            Some("toolCall") => {
                let id = b["id"].as_str().unwrap_or_default().to_owned();
                let name = b["name"].as_str().unwrap_or("tool").to_owned();
                let args = value_to_tool_args(&b["arguments"]);
                blocks.push(ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                });
                if !streaming {
                    let (status, output, details) =
                        if let Some((is_err, out, det)) = results_by_id.get(&id) {
                            (
                                if *is_err {
                                    ToolStatus::Error
                                } else {
                                    ToolStatus::Success
                                },
                                out.clone(),
                                det.clone(),
                            )
                        } else {
                            (ToolStatus::Success, String::new(), None)
                        };
                    tools.push(ToolView {
                        id,
                        name,
                        args,
                        status,
                        output,
                        details,
                    });
                }
            }
            _ => {}
        }
    }
    (blocks, tools)
}

impl Drop for View {
    fn drop(&mut self) {
        self.channel.close();
    }
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

fn bound_names(
    contract: &pi_rs_chord::contracts::Contract,
    variant: u64,
) -> Result<pi_rs_chord::contracts::MemberNames> {
    let variants = contract.variant_order();
    let name = variants
        .get(variant as usize)
        .ok_or_else(|| Error::Invalid("Unknown terminal contract variant".into()))?;
    contract.variant_member_names(Some(name))
}
