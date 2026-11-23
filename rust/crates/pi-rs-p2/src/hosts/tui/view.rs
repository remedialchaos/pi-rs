//! Read the Sessions v7 catalog and Chat v6 conversation through a single Chord channel.
use pi_rs_chord::{
    Error, Result,
    delta::{Decoder, apply},
    json::JsonValue,
    link::{
        Body, Channel, ChannelHandler, ChannelOptions, Codec, Limits, Link, Message, Part, Payload,
    },
    sync::SyncFuture,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    mpsc::{self, Receiver, TryRecvError},
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

pub(super) struct View {
    channel: Arc<Channel>,
    incoming: Receiver<Message>,
    catalog: Member,
    transcript: Member,
    catalog_index: Option<u64>,
    chat: u64,
    sessions: pi_rs_chord::contracts::Contract,
    conversation: pi_rs_chord::contracts::Contract,
    send_method: Option<u64>,
    view_index: Option<u64>,
    next_call: u64,
    pending: Option<(u64, String)>,
    pub(super) accepted: Option<String>,
    preferred: Option<String>,
    selected: Option<(String, u64)>,
    rows: Vec<(String, u64, String)>,
    picker: Option<usize>,
    title: String,
    pub(super) text: String,
    pub(super) last_message: String,
}
impl View {
    pub(super) fn connect(link: Arc<dyn Link>, preferred: Option<String>) -> Result<Self> {
        let (send, incoming) = mpsc::sync_channel(64);
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
            transcript: Member::default(),
            catalog_index: None,
            chat: 2,
            sessions: crate::api::v1::p2_contracts::lookup("p2.sessions")?,
            conversation: crate::api::v1::p2_contracts::lookup("p2.chat")?,
            send_method: None,
            view_index: None,
            next_call: 0,
            pending: None,
            accepted: None,
            preferred,
            selected: None,
            rows: vec![],
            picker: None,
            title: String::new(),
            text: "Connecting to sessions…".into(),
            last_message: String::new(),
        };
        view.hold(None)?;
        Ok(view)
    }
    fn hold(&self, target: Option<(&str, u64)>) -> Result<()> {
        let (hold, contract, params, kind, address) = match target {
            None => (1, &self.sessions, json!({}), "server", json!({})),
            Some((id, conversation)) => (
                self.chat,
                &self.conversation,
                json!({"session":id,"conversation":conversation}),
                "session",
                json!({"id":id}),
            ),
        };
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
    pub(super) fn submit(&mut self, text: &str) -> Result<()> {
        let Some(method) = self.send_method else {
            return Ok(());
        };
        if self.pending.is_some() || text.trim().is_empty() {
            return Ok(());
        }
        self.next_call += 1;
        let request =
            json!({"text":text,"requestId":uuid::Uuid::new_v4().to_string(),"whenBusy":"steer"});
        self.channel.send(Message::new(Body::Call {
            call: self.next_call,
            hold: self.chat,
            method,
            args: Some(Payload {
                part: Part::Text(request.to_string()),
                segments: None,
            }),
        }))?;
        self.pending = Some((self.next_call, text.to_owned()));
        Ok(())
    }
    pub(super) fn step(&mut self) -> Result<bool> {
        if self.channel.closed() {
            return Err(Error::Closed);
        }
        let msg = match self.incoming.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(_) => return Err(Error::Closed),
        };
        match msg.body {
            Body::Bound(entries) => {
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
                    }
                    if e.hold == 1 {
                        self.catalog_index = bound_names(&self.sessions, e.variant)?
                            .state
                            .iter()
                            .position(|n| n == "sessions")
                            .map(|index| index as u64);
                    }
                    for (i, part) in e.members {
                        self.publish(e.hold, i, &part, true)?;
                    }
                }
            }
            Body::State(entries) => {
                for e in entries {
                    for (i, part) in e.members {
                        self.publish(e.hold, i, &part, false)?;
                    }
                }
            }
            Body::Result { call, .. }
                if self.pending.as_ref().is_some_and(|(id, _)| *id == call) =>
            {
                self.accepted = self.pending.take().map(|(_, text)| text);
                return Ok(true);
            }
            Body::Error { call, .. }
                if self.pending.as_ref().is_some_and(|(id, _)| *id == call) =>
            {
                self.pending = None;
                self.text.push_str("\nSend failed");
                return Ok(true);
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
                self.last_message.clear();
                self.text = "Waiting for session…".into();
                return Ok(true);
            }
            Body::Failed { hold, .. } | Body::Gone { hold, .. }
                if hold == 1 || hold == self.chat =>
            {
                return Err(Error::Invalid("Session access ended or unavailable".into()));
            }
            _ => return Ok(false),
        }
        self.channel
            .send(Message::new(Body::Ack { events: None }))?;
        Ok(true)
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
            let next = self
                .rows
                .iter()
                .find(|(id, _, _)| {
                    self.selected
                        .as_ref()
                        .is_some_and(|(current, _)| id == current)
                })
                .or(self.rows.first())
                .cloned();
            self.select(next)?;
            self.render();
        } else if self.selected.is_some() && hold == self.chat && Some(index) == self.view_index {
            self.transcript.read(part, snapshot)?;
            self.render();
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
            self.send_method = None;
            self.view_index = None;
            self.pending = None;
            self.transcript = Member::default();
            self.last_message.clear();
            self.chat += 1;
            if let Some((id, c)) = &next {
                self.hold(Some((id, *c)))?;
            }
            self.selected = next;
        }
        Ok(())
    }
    pub(super) fn destination(&self) -> Option<&(String, u64)> {
        self.selected.as_ref()
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
                    self.model(&entry["model"][0]);
                }
            }
            self.model(&v["docs"]["pi.live"]["generation"]["message"]);
        }
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
    let variant = variants
        .get(variant as usize)
        .and_then(|name| contract.current().variants.get(name))
        .ok_or_else(|| Error::Invalid("Unknown terminal contract variant".into()))?;
    Ok(pi_rs_chord::contracts::variant_names(variant))
}
