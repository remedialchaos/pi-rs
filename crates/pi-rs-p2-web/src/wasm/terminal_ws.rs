//! Live WebSocket PTY channel client (`/terminal?token=...`) feeding [`VtScreen`].

use crate::state::vt::VtScreen;
use crate::ui::terminal::{TerminalStatus, TerminalTabInfo};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::prelude::*;
use serde_json::Value;

#[derive(Clone, Debug)]
pub enum TerminalWsAction {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

#[derive(Clone, Copy)]
pub struct TerminalRuntime {
    pub status: RwSignal<TerminalStatus>,
    pub title: RwSignal<String>,
    pub lines: RwSignal<Vec<String>>,
    pub tabs: RwSignal<Vec<TerminalTabInfo>>,
    pub active_id: RwSignal<Option<String>>,
    pub cols: RwSignal<u16>,
    pub rows: RwSignal<u16>,
    pub tx: StoredValue<Option<UnboundedSender<TerminalWsAction>>>,
}

impl TerminalRuntime {
    pub fn new() -> Self {
        Self {
            status: RwSignal::new(TerminalStatus::Closed),
            title: RwSignal::new("Terminal".to_owned()),
            lines: RwSignal::new(Vec::new()),
            tabs: RwSignal::new(Vec::new()),
            active_id: RwSignal::new(None),
            cols: RwSignal::new(80),
            rows: RwSignal::new(24),
            tx: StoredValue::new(None),
        }
    }

    pub fn send_input(&self, bytes: Vec<u8>) {
        if let Some(sender) = self.tx.get_value() {
            let _ = sender.unbounded_send(TerminalWsAction::Input(bytes));
        }
    }

    pub fn close_channel(&self) {
        if let Some(sender) = self.tx.get_value() {
            let _ = sender.unbounded_send(TerminalWsAction::Close);
        }
        self.tx.set_value(None);
    }
}

pub fn terminal_ws_url(token: &str) -> Option<String> {
    let loc = window().location();
    let proto = loc.protocol().ok()?;
    let host = loc.host().ok()?;
    let scheme = if proto == "https:" { "wss" } else { "ws" };
    Some(format!(
        "{scheme}://{host}/terminal?token={}",
        js_sys::encode_uri_component(token)
    ))
}

pub fn spawn_terminal_channel(
    runtime: TerminalRuntime,
    token: String,
    on_ack: impl Fn(u64) + 'static,
) {
    runtime.close_channel();
    let Some(url) = terminal_ws_url(&token) else {
        runtime.status.set(TerminalStatus::Closed);
        return;
    };
    let (tx, mut rx): (
        UnboundedSender<TerminalWsAction>,
        UnboundedReceiver<TerminalWsAction>,
    ) = unbounded();
    runtime.tx.set_value(Some(tx));
    runtime.status.set(TerminalStatus::Connecting);

    wasm_bindgen_futures::spawn_local(async move {
        let Ok(mut ws) = WebSocket::open(&url) else {
            runtime.status.set(TerminalStatus::Closed);
            return;
        };
        runtime.status.set(TerminalStatus::Live);
        let mut vt = VtScreen::new(runtime.cols.get_untracked(), runtime.rows.get_untracked());

        loop {
            futures_util::select! {
                msg = ws.next().fuse() => match msg {
                    Some(Ok(Message::Bytes(bytes))) => {
                        let len = bytes.len() as u64;
                        vt.feed(&bytes);
                        if let Some(t) = vt.title() {
                            runtime.title.set(t.to_owned());
                        }
                        runtime.lines.set(vt.styled_lines_html());
                        on_ack(len);
                    }
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(val) = serde_json::from_str::<Value>(&txt) {
                            handle_terminal_event(runtime, &mut vt, &val);
                        }
                    }
                    Some(Err(_)) | None => {
                        runtime.status.set(TerminalStatus::Closed);
                        break;
                    }
                },
                act = rx.next().fuse() => match act {
                    Some(TerminalWsAction::Input(bytes)) => {
                        if ws.send(Message::Bytes(bytes)).await.is_err() {
                            runtime.status.set(TerminalStatus::Closed);
                            break;
                        }
                    }
                    Some(TerminalWsAction::Resize(c, r)) => {
                        vt.resize(c, r);
                        runtime.lines.set(vt.styled_lines_html());
                    }
                    Some(TerminalWsAction::Close) | None => {
                        let _ = ws.close(None, None);
                        runtime.status.set(TerminalStatus::Closed);
                        break;
                    }
                }
            }
        }
    });
}

fn handle_terminal_event(runtime: TerminalRuntime, vt: &mut VtScreen, val: &Value) {
    match val["type"].as_str() {
        Some("terminals" | "list") => {
            let tabs: Vec<TerminalTabInfo> = val["terminals"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    Some(TerminalTabInfo {
                        id: t["id"].as_str()?.to_owned(),
                        title: t["title"]
                            .as_str()
                            .or_else(|| t["name"].as_str())
                            .unwrap_or("shell")
                            .to_owned(),
                        cols: t["cols"].as_u64().unwrap_or(80) as u16,
                        rows: t["rows"].as_u64().unwrap_or(24) as u16,
                    })
                })
                .collect();
            runtime.tabs.set(tabs);
        }
        Some("attached") => {
            if let Some(id) = val["terminal"]["id"].as_str() {
                runtime.active_id.set(Some(id.to_owned()));
            }
            let c = val["terminal"]["cols"].as_u64().unwrap_or(80) as u16;
            let r = val["terminal"]["rows"].as_u64().unwrap_or(24) as u16;
            runtime.cols.set(c);
            runtime.rows.set(r);
            *vt = VtScreen::new(c, r);
            runtime.lines.set(Vec::new());
            if let Some(t) = val["terminal"]["title"].as_str() {
                runtime.title.set(t.to_owned());
            }
        }
        Some("changed") => {
            let c = val["terminal"]["cols"].as_u64().unwrap_or(80) as u16;
            let r = val["terminal"]["rows"].as_u64().unwrap_or(24) as u16;
            runtime.cols.set(c);
            runtime.rows.set(r);
            vt.resize(c, r);
            runtime.lines.set(vt.styled_lines_html());
        }
        Some("closed") => {
            if let Some(closed_id) = val["terminalId"].as_str() {
                if runtime.active_id.get_untracked().as_deref() == Some(closed_id) {
                    runtime.active_id.set(None);
                }
                runtime
                    .tabs
                    .update(|tabs| tabs.retain(|t| t.id != closed_id));
            }
        }
        _ => {}
    }
}
