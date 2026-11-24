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
use pi_rs_p2_tui::ThemeRef;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallKind {
    ChatSend,
    SessionCreate,
    TreeFetch,
    CostsFetch,
}

#[derive(Default)]
struct ModelsMembers {
    hold: Option<u64>,
    models_index: Option<u64>,
    default_index: Option<u64>,
    errors_index: Option<u64>,
    models_member: Member,
    default_member: Member,
    errors_member: Member,
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
    pending_fetch: bool,
}

#[derive(Default)]
struct CostsMembers {
    hold: Option<u64>,
    costs_method: Option<u64>,
    pending_fetch: bool,
}

pub(super) struct View {
    channel: Arc<Channel>,
    incoming: Receiver<Message>,
    catalog: Member,
    default_cwd_member: Member,
    transcript: Member,
    stats_member: Member,
    catalog_index: Option<u64>,
    default_cwd_index: Option<u64>,
    create_method: Option<u64>,
    default_cwd: Option<String>,
    next_hold: u64,
    chat: u64,
    sessions: pi_rs_chord::contracts::Contract,
    conversation: pi_rs_chord::contracts::Contract,
    models_contract: pi_rs_chord::contracts::Contract,
    status_contract: pi_rs_chord::contracts::Contract,
    tree_contract: pi_rs_chord::contracts::Contract,
    costs_contract: pi_rs_chord::contracts::Contract,
    send_method: Option<u64>,
    view_index: Option<u64>,
    stats_index: Option<u64>,
    next_call: u64,
    pending_calls: HashMap<u64, CallKind>,
    pending_send: Option<(u64, String)>,
    pending_switch_session: Option<String>,
    queued_create: Option<Option<String>>,
    queued_submit: Option<String>,
    pub(super) accepted: Option<String>,
    preferred: Option<String>,
    selected: Option<(String, u64)>,
    rows: Vec<(String, u64, String)>,
    picker: Option<usize>,
    title: String,
    pub(super) text: String,
    pub(super) last_message: String,
    models_state: ModelsMembers,
    status_state: StatusMembers,
    tree_state: TreeMembers,
    costs_state: CostsMembers,
    pub(super) models_ready: bool,
    pub(super) models_list: Vec<Value>,
    pub(super) default_model: Value,
    pub(super) models_errors: Vec<Value>,
    pub(super) models_error: Option<String>,
    pub(super) statuses: Option<Value>,
    pub(super) status_error: Option<String>,
    pub(super) tree_data: Option<Value>,
    pub(super) tree_error: Option<String>,
    pub(super) costs_data: Option<Value>,
    pub(super) costs_error: Option<String>,
    pub(super) stats_data: Option<Value>,
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
            transcript: Member::default(),
            stats_member: Member::default(),
            catalog_index: None,
            default_cwd_index: None,
            create_method: None,
            default_cwd: None,
            next_hold: 2,
            chat: 2,
            sessions: crate::api::v1::p2_contracts::lookup("p2.sessions")?,
            conversation: crate::api::v1::p2_contracts::lookup("p2.chat")?,
            models_contract: crate::api::v1::p2_contracts::lookup("p2.models")?,
            status_contract: crate::api::v1::p2_contracts::lookup("p2.session-status")?,
            tree_contract: crate::api::v1::p2_contracts::lookup("p2.tree")?,
            costs_contract: crate::api::v1::p2_contracts::lookup("p2.chat-costs")?,
            send_method: None,
            view_index: None,
            stats_index: None,
            next_call: 0,
            pending_calls: HashMap::new(),
            pending_send: None,
            pending_switch_session: None,
            queued_create: None,
            queued_submit: None,
            accepted: None,
            preferred,
            selected: None,
            rows: vec![],
            picker: None,
            title: String::new(),
            text: "Connecting to sessions…".into(),
            last_message: String::new(),
            models_state: ModelsMembers::default(),
            status_state: StatusMembers::default(),
            tree_state: TreeMembers::default(),
            costs_state: CostsMembers::default(),
            models_ready: false,
            models_list: Vec::new(),
            default_model: Value::Null,
            models_errors: Vec::new(),
            models_error: None,
            statuses: None,
            status_error: None,
            tree_data: None,
            tree_error: None,
            costs_data: None,
            costs_error: None,
            stats_data: None,
        };
        view.send_hold(1, &view.sessions, json!({}), "server", json!({}))?;
        Ok(view)
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
        let Some(method) = self.create_method else {
            self.queued_create = Some(title.map(str::to_owned));
            return Ok(());
        };
        let cwd = self.default_cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into())
        });
        let args = json!({
            "cwd": cwd,
            "title": title.unwrap_or(""),
        });
        self.call_method(1, method, args, CallKind::SessionCreate)?;
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
                        self.send_method = names
                            .methods
                            .iter()
                            .position(|n| n == "send")
                            .map(|i| i as u64);
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
                        self.create_method = names
                            .methods
                            .iter()
                            .position(|n| n == "create")
                            .map(|index| index as u64);
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
                        CallKind::SessionCreate => {
                            if let Some(payload) = value {
                                let row =
                                    payload.part.parse(Limits::for_link(false))?.to_serde()?;
                                if let Some(id) = row["id"].as_str() {
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
                        }
                        CallKind::TreeFetch => {
                            if let Some(payload) = value {
                                let tree =
                                    payload.part.parse(Limits::for_link(false))?.to_serde()?;
                                self.tree_data = Some(tree);
                                self.tree_error = None;
                            }
                        }
                        CallKind::CostsFetch => {
                            if let Some(payload) = value {
                                let costs =
                                    payload.part.parse(Limits::for_link(false))?.to_serde()?;
                                self.costs_data = Some(costs);
                                self.costs_error = None;
                            }
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
                        CallKind::SessionCreate => {
                            self.text
                                .push_str(&format!("\nCreate session failed: {message}"));
                        }
                        CallKind::TreeFetch => {
                            self.tree_error = Some(message);
                        }
                        CallKind::CostsFetch => {
                            self.costs_error = Some(message);
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
                    self.select(None)?;
                }
                self.send_method = None;
                self.transcript = Member::default();
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
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.view_index {
            self.transcript.read(part, snapshot)?;
            self.render();
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.stats_index {
            self.stats_data = Some(self.stats_member.read(part, snapshot)?.to_serde()?);
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
            self.tree_state.pending_fetch = false;
            self.tree_data = None;
            self.tree_error = None;
            self.costs_state.costs_method = None;
            self.costs_state.pending_fetch = false;
            self.costs_data = None;
            self.costs_error = None;

            self.send_method = None;
            self.view_index = None;
            self.stats_index = None;
            self.pending_send = None;
            self.queued_submit = None;
            self.transcript = Member::default();
            self.stats_member = Member::default();
            self.stats_data = None;
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

    pub(super) fn choosing(&self) -> bool {
        self.picker.is_some()
    }

    pub(super) fn choose(&mut self, key: &str) -> Result<()> {
        match (self.picker, key) {
            (None, "open") => {
                self.picker = Some(
                    self.rows
                        .iter()
                        .position(|(id, _, _)| {
                            self.selected
                                .as_ref()
                                .is_some_and(|(current, _)| id == current)
                        })
                        .unwrap_or(0),
                )
            }
            (Some(pos), "down") => {
                self.picker = Some((pos + 1).min(self.rows.len().saturating_sub(1)))
            }
            (Some(pos), "up") => self.picker = Some(pos.saturating_sub(1)),
            (Some(pos), "enter") => {
                self.select(self.rows.get(pos).cloned())?;
                self.picker = None;
            }
            (_, _) => self.picker = None,
        }
        self.render();
        Ok(())
    }

    fn render(&mut self) {
        if let Some(pos) = self.picker {
            self.text = "Sessions".into();
            for (index, (_, _, title)) in self.rows.iter().enumerate() {
                self.text
                    .push_str(if index == pos { "\n> " } else { "\n  " });
                self.text.push_str(title);
            }
            self.last_message.clear();
            return;
        }
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

    /// Render the transcript or session picker wrapped cleanly to `width`.
    pub(super) fn render_transcript_lines(
        &self,
        theme_ref: &ThemeRef,
        width: usize,
    ) -> Vec<String> {
        let width = width.max(1);
        let theme = theme_ref.current();
        if let Some(pos) = self.picker {
            let mut body = vec![pi_rs_tui::truncate_to_width(
                &format!(" {}", theme.bold(&theme.fg("accent", "Sessions"))),
                width,
                true,
            )];
            for (index, (_, _, title)) in self.rows.iter().enumerate() {
                let line = if index == pos {
                    theme.fg("accent", &format!(" > {title}"))
                } else {
                    theme.fg("text", &format!("   {title}"))
                };
                body.push(pi_rs_tui::truncate_to_width(&line, width, true));
            }
            return body;
        }
        let mut lines = Vec::new();
        if let Some(v) = self
            .transcript
            .value
            .as_ref()
            .and_then(|v| v.to_serde().ok())
        {
            for entry in v["entries"].as_array().into_iter().flatten() {
                if let Some(text) = entry["data"]["text"].as_str() {
                    let cleaned = clean(text);
                    if !cleaned.is_empty() {
                        render_user_block(&cleaned, &theme, width, &mut lines);
                    }
                } else {
                    for m in entry["model"].as_array().into_iter().flatten() {
                        render_model_message(m, theme_ref, &theme, width, &mut lines, false);
                    }
                }
            }
            let live = &v["docs"]["pi.live"]["generation"]["message"];
            if !live.is_null() {
                render_model_message(live, theme_ref, &theme, width, &mut lines, true);
            }
        } else if let Some((_, body)) = self.text.split_once('\n') {
            for raw in body.lines() {
                for wrapped in pi_rs_tui::wrap_text_with_ansi(raw, width.saturating_sub(2).max(1)) {
                    lines.push(format!(" {}", theme.fg("muted", &wrapped)));
                }
            }
        }
        lines
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

fn render_user_block(
    text: &str,
    theme: &pi_rs_p2_tui::theme::Theme,
    width: usize,
    lines: &mut Vec<String>,
) {
    let inner = width.saturating_sub(2).max(1);
    let pad_line = theme.bg("userMessageBg", &" ".repeat(width));
    lines.push(String::new());
    lines.push(pad_line.clone());
    for wrapped in pi_rs_tui::wrap_text_with_ansi(text, inner) {
        let vis = pi_rs_tui::visible_width(&wrapped);
        let right_pad = width.saturating_sub(1 + vis);
        let row = format!(" {wrapped}{}", " ".repeat(right_pad));
        lines.push(theme.bg("userMessageBg", &theme.fg("userMessageText", &row)));
    }
    lines.push(pad_line);
}

fn push_model_text(
    s: &str,
    role: &str,
    theme_ref: &ThemeRef,
    width: usize,
    started: &mut bool,
    lines: &mut Vec<String>,
) {
    let cleaned = clean(s);
    if cleaned.is_empty() {
        return;
    }
    if !*started {
        lines.push(String::new());
        *started = true;
    }
    if role == "assistant" {
        let md = pi_rs_tui::Markdown::new(
            cleaned,
            1,
            0,
            theme_ref.markdown(),
            Default::default(),
            Default::default(),
        );
        lines.extend(pi_rs_tui::Component::render(&md, width));
    } else {
        let theme = theme_ref.current();
        let color = if role == "toolResult" {
            "muted"
        } else {
            "text"
        };
        for wrapped in pi_rs_tui::wrap_text_with_ansi(&cleaned, width.saturating_sub(2).max(1)) {
            lines.push(format!(" {}", theme.fg(color, &wrapped)));
        }
    }
}

fn render_model_message(
    m: &Value,
    theme_ref: &ThemeRef,
    theme: &pi_rs_p2_tui::theme::Theme,
    width: usize,
    lines: &mut Vec<String>,
    streaming: bool,
) {
    let role = m["role"].as_str().unwrap_or("");
    if role == "user" {
        if let Some(s) = m["content"].as_str() {
            let cleaned = clean(s);
            if !cleaned.is_empty() {
                render_user_block(&cleaned, theme, width, lines);
            }
        }
        return;
    }
    let mut started = false;
    if let Some(s) = m["content"].as_str() {
        push_model_text(s, role, theme_ref, width, &mut started, lines);
        return;
    }
    for b in m["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("thinking") => {
                if !started {
                    lines.push(String::new());
                    started = true;
                }
                lines.push(format!(
                    " {}",
                    theme.italic(&theme.fg("thinkingText", "Thinking..."))
                ));
            }
            Some("text") => {
                if let Some(s) = b["text"].as_str() {
                    push_model_text(s, role, theme_ref, width, &mut started, lines);
                }
            }
            Some("toolCall") => {
                if !started {
                    lines.push(String::new());
                    started = true;
                }
                let name = b["name"].as_str().unwrap_or("tool");
                let suffix = if streaming { "…" } else { "" };
                lines.push(pi_rs_tui::truncate_to_width(
                    &format!(
                        " {}",
                        theme.fg("toolTitle", &format!("[tool: {name}{suffix}]"))
                    ),
                    width,
                    true,
                ));
            }
            _ => {}
        }
    }
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
    contract.member_names(Some(name))
}
