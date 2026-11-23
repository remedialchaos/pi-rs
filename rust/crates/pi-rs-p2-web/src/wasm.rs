//! Leptos browser host for the p2 Chord contracts.
#![cfg(target_arch = "wasm32")]
mod wire;

use crate::chord_bridge::replica::Replica;
use crate::chord_bridge::{backoff, encode_varint};
use crate::ui::{
    app::AppLayout,
    chat::{Chat, ChatMessage, MessageRole},
    composer::Composer,
};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::{mount::mount_to, prelude::*};
use serde_json::{Value, json};
use std::collections::HashMap;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlElement, Url};
use wire::{Reader, Result};

#[derive(Clone, PartialEq)]
struct Session {
    id: String,
    title: String,
    main: u64,
}
enum Command {
    Open(String, u64),
    Send(String),
    Create,
}
#[derive(Clone)]
struct Submission {
    destination: (String, u64),
    text: String,
    id: String,
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
}
impl Ui {
    fn command(self, cmd: Command) {
        if let Some(tx) = self.commands.get_value() {
            let _ = tx.unbounded_send(cmd);
        }
    }
}
fn endpoint(root: &HtmlElement) -> Result<(String, bool)> {
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
        return Ok((url.href(), true));
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
    ))
}
fn js_error(v: JsValue) -> String {
    format!("{v:?}")
}
fn uuid() -> Result<String> {
    Ok(window().crypto().map_err(js_error)?.random_uuid())
}
fn transcript(view: &Value) -> Vec<ChatMessage> {
    let row = |id: String, m: &Value, streaming| {
        let role = match m["role"].as_str()? {
            "assistant" => MessageRole::Assistant,
            "user" => MessageRole::User,
            _ => return None,
        };
        let content = &m["content"];
        let text = content.as_str().map(str::to_owned).unwrap_or_else(|| {
            content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
        Some(ChatMessage {
            id,
            role,
            text,
            streaming,
        })
    };
    let mut rows: Vec<_> = view["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry["id"].to_string();
            let data = &entry["data"];
            if matches!(data["author"]["kind"].as_str(), Some("owner" | "guest"))
                && data["attachments"].is_array()
                && let Some(text) = data["text"].as_str()
            {
                return Some(ChatMessage {
                    id,
                    role: MessageRole::User,
                    text: text.into(),
                    streaming: false,
                });
            }
            row(id, &entry["model"][0], false)
        })
        .collect();
    rows.extend(row(
        "live".into(),
        &view["docs"]["pi.live"]["generation"]["message"],
        true,
    ));
    rows
}
struct Link {
    ui: Ui,
    catalog_member: u64,
    chat: u64,
    members: HashMap<(u64, u64), Replica>,
    next_call: u64,
    pending: Option<(u64, Command)>,
    greeted: bool,
}
impl Link {
    fn select(&mut self, id: String, conversation: u64, out: &mut Vec<Vec<u8>>) {
        if self.chat >= 3 && self.ui.selected.get_untracked() == Some((id.clone(), conversation)) {
            return;
        }
        if self.chat >= 3 && self.ui.selected.get_untracked().is_some() {
            let mut release = vec![2];
            encode_varint(self.chat, &mut release);
            out.push(release);
        }
        self.members.retain(|(h, _), _| *h <= 2);
        self.chat = self.chat.max(2) + 1;
        self.ui.write.set(false);
        self.ui.messages.set(vec![]);
        out.push(wire::hold(self.chat, Some((&id, conversation))));
        self.ui.selected.set(Some((id, conversation)));
    }
    fn publish(&mut self, h: u64, m: u64, out: &mut Vec<Vec<u8>>) {
        let value = &self.members[&(h, m)].value;
        if h == 2 && m == self.catalog_member {
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
                let mut release = vec![2];
                encode_varint(self.chat, &mut release);
                out.push(release);
                self.ui.selected.set(None);
                self.ui.messages.set(vec![]);
                self.ui.write.set(false);
            }
        } else if h == self.chat && m == 3 {
            self.ui.messages.set(transcript(value));
        }
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
                out.extend([wire::hold(1, None), wire::hold(2, None)]);
            }
            5 | 6 => {
                for _ in 0..r.number()? {
                    let h = r.number()?;
                    if kind == 5 {
                        let variant = r.number()?;
                        self.members.retain(|(id, _), _| *id != h);
                        if h == 1 {
                            self.ui.guest.set(variant == 0);
                        }
                        if h == 2 {
                            self.catalog_member = if variant == 1 { 3 } else { 0 };
                            self.ui.create.set(variant == 1);
                        }
                        if h == self.chat {
                            self.ui
                                .write
                                .set(variant == 0 && self.ui.selected.get_untracked().is_some());
                        }
                    }
                    for _ in 0..r.number()? {
                        let m = r.number()?;
                        let v = r.part()?;
                        if h <= 2 || (h == self.chat && self.ui.selected.get_untracked().is_some())
                        {
                            if kind == 5 {
                                self.members.insert((h, m), Replica::snapshot(v));
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
                        self.ui.status.set(e.to_string());
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
                if self.pending.as_ref().is_some_and(|(call, _)| *call == id)
                    && let Some((_, cmd)) = self.pending.take()
                {
                    self.ui.busy.set(false);
                    match cmd {
                        Command::Send(text) => {
                            self.ui.last_submission.set_value(None);
                            if kind == 8 && self.ui.draft.get_untracked() == text {
                                self.ui.draft.set(String::new())
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
                if h == self.chat {
                    self.ui.write.set(false);
                }
                self.ui.status.set(reason);
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
        if let Command::Open(id, c) = cmd {
            if self.pending.is_none() {
                self.select(id, c, &mut out);
            }
            return Ok(out);
        }
        if self.pending.is_some() {
            return Ok(out);
        }
        let (hold, method, args) = match &cmd {
            Command::Send(text) if self.ui.write.get_untracked() => {
                let destination = self
                    .ui
                    .selected
                    .get_untracked()
                    .ok_or("no selected conversation")?;
                let previous = self.ui.last_submission.get_value();
                let id = match previous {
                    Some(prior) if prior.destination == destination && prior.text == *text => {
                        prior.id
                    }
                    _ => uuid()?,
                };
                self.ui.last_submission.set_value(Some(Submission {
                    destination,
                    text: text.clone(),
                    id: id.clone(),
                }));
                (
                    self.chat,
                    8,
                    json!({"text":text, "requestId":id, "whenBusy":"steer"}),
                )
            }
            Command::Create if self.ui.create.get_untracked() => {
                let cwd = self
                    .members
                    .get(&(2, 0))
                    .and_then(|m| m.value.as_str())
                    .ok_or("missing default working directory")?;
                (
                    2,
                    2,
                    json!({"cwd":cwd,"env":self.members.get(&(2,1)).map(|m|&m.value)}),
                )
            }
            _ => return Ok(out),
        };
        self.next_call += 1;
        out.push(wire::call(self.next_call, hold, method, args));
        self.ui.busy.set(true);
        self.pending = Some((self.next_call, cmd));
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
        catalog_member: 3,
        chat: 0,
        next_call: 0,
        pending: None,
        greeted: false,
        members: HashMap::new(),
    };
    loop {
        let out = futures_util::select! {
            msg = socket.next().fuse() => match msg {
                Some(Ok(Message::Bytes(bytes))) => link.frame(&bytes)?,
                Some(Err(gloo_net::websocket::WebSocketError::ConnectionClose(event))) if event.code == 4001 => {
                    window().location().reload().map_err(js_error)?;
                    return Ok(());
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
    let (url, cloud) = endpoint(&root)?;
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
        if ui.guest.get_untracked()
            && let Ok(r) = gloo_net::http::Request::get("/")
                .method(gloo_net::http::Method::HEAD)
                .cache(web_sys::RequestCache::NoStore)
                .send()
                .await
            && matches!(r.status(), 401 | 403)
        {
            ui.sessions.set(vec![]);
            ui.messages.set(vec![]);
            ui.selected.set(None);
            return Err("Share ended".into());
        }
        if js_sys::Date::now() - start >= 10000.0 {
            failures = 0
        }
        gloo_timers::future::TimeoutFuture::new(backoff(failures) as u32).await;
        failures = failures.saturating_add(1);
    }
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
        let selected = match route.as_slice() {
            ["s", id] => Some((id.to_string(), 1)),
            ["s", id, "c", c] => c.parse().ok().map(|c| (id.to_string(), c)),
            _ => None,
        };
        let ui = Ui {
            sessions: RwSignal::new(vec![]),
            messages: RwSignal::new(vec![]),
            selected: RwSignal::new(selected),
            status: RwSignal::new("Connecting".into()),
            write: RwSignal::new(false),
            create: RwSignal::new(false),
            draft: RwSignal::new(String::new()),
            guest: RwSignal::new(false),
            commands: StoredValue::new(None),
            busy: RwSignal::new(false),
            last_submission: StoredValue::new(None),
        };
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
                {move || ui.sessions.get().into_iter().map(|session| view! {
                    <button disabled=move || ui.busy.get()
                        on:click=move |_| ui.command(Command::Open(session.id.clone(), session.main))>
                        {session.title}
                    </button>
                }).collect_view()}
            </div>
        }.into_any();
        let main = view! {
            <div class="main-content">
                <p role="status">{move || ui.status.get()}</p>
                <Chat messages=ui.messages />
                <fieldset disabled=move || !ui.write.get() || ui.busy.get()>
                    <Composer draft=ui.draft clear_on_send=false
                        on_send=Callback::new(move |text| ui.command(Command::Send(text))) />
                </fieldset>
            </div>
        }
        .into_any();
        view! { <AppLayout sidebar=sidebar main=main panel=None /> }
    };
    mount_to(parent, app).forget();
    Ok(())
}
#[wasm_bindgen(start)]
pub fn start() -> std::result::Result<(), JsValue> {
    mount_app()
}
