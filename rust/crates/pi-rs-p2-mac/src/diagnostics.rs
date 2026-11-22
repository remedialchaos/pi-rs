use crate::{
    Result,
    appearance::Appearance,
    host_bridge::{Message, Theme, admit},
    server_config::Launch,
    write_json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageCapture {
    pub url: Option<String>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub notification: Option<String>,
    pub service_worker: Option<bool>,
    pub clipboard: Option<bool>,
    pub get_user_media: Option<bool>,
    pub user_agent: Option<String>,
    pub shell: Option<String>,
    pub inset: Option<String>,
    pub sidebar_padding: Option<String>,
    pub theme: Option<String>,
    pub color_scheme: Option<String>,
    pub prefers_dark: Option<bool>,
    pub pi_host: Option<bool>,
    pub chrome: Option<Value>,
    pub corner: Option<Value>,
    pub app_appearance: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeCapture {
    pub page: Value,
    pub app_appearance: Option<String>,
    pub foreign: Value,
    pub app_appearance_after_refusals: Option<String>,
    pub menu: Value,
    #[serde(rename = "switch")]
    pub switches: Value,
    pub fade: Value,
}

pub struct Diagnostics {
    directory: PathBuf,
    bridge: Option<String>,
    targets: VecDeque<String>,
    pub open_path: String,
    index: usize,
}
impl Diagnostics {
    pub fn new(launch: &Launch) -> Option<Self> {
        Some(Self {
            directory: launch.diag_dir.clone()?,
            bridge: launch.diag_bridge.clone(),
            targets: launch.diag_open.clone().into(),
            open_path: launch.diag_path.clone(),
            index: 0,
        })
    }
    pub fn next_page(&mut self) -> (usize, Option<String>) {
        self.index += 1;
        (self.index, self.targets.pop_front())
    }
    pub fn consume_bridge(&mut self, has_host: bool) -> Option<String> {
        if has_host { self.bridge.take() } else { None }
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn image(&self, kind: &str, index: usize) -> PathBuf {
        self.directory.join(format!("{kind}-{index}.png"))
    }
    pub fn page(&self, index: usize, capture: &PageCapture) -> Result<()> {
        write_json(&self.directory.join(format!("page-{index}.json")), capture)
    }
    pub fn bridge(&self, index: usize, capture: &BridgeCapture) -> Result<()> {
        write_json(
            &self.directory.join(format!("bridge-{index}.json")),
            capture,
        )
    }
}

/// Linux's diagnostic command exercises the same admission and captures the same core reply/error shapes.
pub fn bridge_sample(
    origin: &Url,
    preference: Appearance,
    requested: &str,
) -> Result<BridgeCapture> {
    let mut current = preference;
    let mut call = |main, body: Value| match admit(Some(origin), origin, main, &body) {
        Ok(message) => {
            if let Message::SetTheme(value) = message {
                current = value;
            }
            json!({"reply":Theme { preference: current }})
        }
        Err(error) => json!({"error":error.to_string()}),
    };
    let before = call(true, json!({"type":"theme"}));
    let set = call(true, json!({"type":"set-theme","preference":requested}));
    let after = call(true, json!({"type":"theme"}));
    let invalid = call(true, json!({"type":"set-theme","preference":"sepia"}));
    let unknown = call(true, json!({"type":"launch"}));
    let from_frame = call(false, json!({"type":"set-theme","preference":"light"}));
    let foreign = Url::parse("https://foreign.invalid/")?;
    let error = admit(Some(origin), &foreign, true, &json!({"type":"theme"}))
        .err()
        .map(|e| e.to_string());
    let events = set
        .get("reply")
        .map_or_else(Vec::new, |reply| vec![reply.clone()]);
    Ok(BridgeCapture {
        page: json!({"before":{"get":before},"set":set,"after":{"get":after},"invalid":invalid,"unknown":unknown,"fromFrame":from_frame,"events":events}),
        foreign: json!({"origin":foreign.origin().ascii_serialization(),"error":error}),
        ..BridgeCapture::default()
    })
}
