//! Browser plugin contracts, wire encoding, and pure-Rust state projections.
//!
//! Kept free of `target_arch = "wasm32"` gating so every contract descriptor,
//! binary frame encoder, transcript projection, and plugin view decoder is
//! unit-tested on native hosts as well as compiled to `wasm32-unknown-unknown`.

use crate::activity::detail::describe;
use crate::activity::label::Described;
use crate::activity::model::{Step, StepStatus, ToolActivityTable, call_status};
use crate::chord_bridge::{encode_part, encode_string, encode_varint};
use crate::cost::AssistantCost;
use crate::state::attachments::UploadEntry;
use crate::state::composer_state::{
    PickedFile, decode_draft, encode_draft, pick_files, referenced_files, rename_references,
};
use crate::ui::command_menu::CommandItem;
use crate::ui::terminal::{TerminalStatus, VoiceState};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Address scope for a Chord contract hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractAddressKind {
    Server,
    Session,
}

impl ContractAddressKind {
    /// Canonical address kind string sent in Chord `Hold` frames.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Session => "session",
        }
    }
}

/// Canonical Chord contract descriptor verified against `pi-rs-p2` declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractSpec {
    pub name: &'static str,
    pub version: u64,
    pub fingerprint: u64,
    pub address_kind: ContractAddressKind,
}

pub const P2_ACCESS: ContractSpec = ContractSpec {
    name: "p2.access",
    version: 1,
    fingerprint: 0x2435548bc2ca4912,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SESSIONS: ContractSpec = ContractSpec {
    name: "p2.sessions",
    version: 7,
    fingerprint: 0x1e273ec42b804553,
    address_kind: ContractAddressKind::Server,
};

pub const P2_MODELS: ContractSpec = ContractSpec {
    name: "p2.models",
    version: 2,
    fingerprint: 0xb53f30ff1090f182,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SETTINGS: ContractSpec = ContractSpec {
    name: "p2.settings",
    version: 1,
    fingerprint: 0x0cf6e665070c59ea,
    address_kind: ContractAddressKind::Server,
};

pub const P2_MCP: ContractSpec = ContractSpec {
    name: "p2.mcp",
    version: 4,
    fingerprint: 0x736667f26edb49b9,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SHARING: ContractSpec = ContractSpec {
    name: "p2.sharing",
    version: 1,
    fingerprint: 0x6a0799ca459aa381,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SESSION_STATUS: ContractSpec = ContractSpec {
    name: "p2.session-status",
    version: 3,
    fingerprint: 0x16ce3275b261a4dc,
    address_kind: ContractAddressKind::Server,
};

pub const P2_VOICE_SERVICE: ContractSpec = ContractSpec {
    name: "p2.voice-service",
    version: 2,
    fingerprint: 0x70b2ca3fc8b19e9e,
    address_kind: ContractAddressKind::Server,
};

pub const P2_CHAT: ContractSpec = ContractSpec {
    name: "p2.chat",
    version: 6,
    fingerprint: 0x15b5fd3b4b340eca,
    address_kind: ContractAddressKind::Session,
};

pub const P2_CHAT_COSTS: ContractSpec = ContractSpec {
    name: "p2.chat-costs",
    version: 1,
    fingerprint: 0x28d677748ecc338f,
    address_kind: ContractAddressKind::Session,
};

pub const P2_ARTIFACTS: ContractSpec = ContractSpec {
    name: "p2.artifacts",
    version: 1,
    fingerprint: 0xcc0d576c70f3171d,
    address_kind: ContractAddressKind::Server,
};

pub const P2_TREE: ContractSpec = ContractSpec {
    name: "p2.tree",
    version: 3,
    fingerprint: 0xe22b01fefbe3b897,
    address_kind: ContractAddressKind::Session,
};

pub const P2_TERMINALS: ContractSpec = ContractSpec {
    name: "p2.terminals",
    version: 2,
    fingerprint: 0x90491c5c0969575f,
    address_kind: ContractAddressKind::Server,
};

pub const P2_VOICE_MODE: ContractSpec = ContractSpec {
    name: "p2.voice-mode",
    version: 3,
    fingerprint: 0xa5d7dfc72666a042,
    address_kind: ContractAddressKind::Session,
};

pub const P2_SESSION_FILES: ContractSpec = ContractSpec {
    name: "p2.session-files",
    version: 1,
    fingerprint: 0x19375d910d567914,
    address_kind: ContractAddressKind::Session,
};

pub const P2_PLUGIN_RELOAD: ContractSpec = ContractSpec {
    name: "p2.plugin-reload",
    version: 1,
    fingerprint: 0xea4fb3b08c01ee95,
    address_kind: ContractAddressKind::Server,
};

/// Local worker contract descriptor for `p2.attachments`.
pub const P2_ATTACHMENTS: ContractSpec = ContractSpec {
    name: "p2.attachments",
    version: 3,
    fingerprint: 0xee56778497e803af,
    address_kind: ContractAddressKind::Server,
};

/// All built-in contracts used by the browser host.
pub const ALL_CONTRACTS: &[ContractSpec] = &[
    P2_ACCESS,
    P2_SESSIONS,
    P2_MODELS,
    P2_SETTINGS,
    P2_MCP,
    P2_SHARING,
    P2_SESSION_STATUS,
    P2_VOICE_SERVICE,
    P2_CHAT,
    P2_CHAT_COSTS,
    P2_ARTIFACTS,
    P2_TREE,
    P2_TERMINALS,
    P2_VOICE_MODE,
    P2_SESSION_FILES,
    P2_PLUGIN_RELOAD,
];

/// Looks up a built-in contract descriptor by canonical name.
pub fn contract_spec(name: &str) -> Option<&'static ContractSpec> {
    ALL_CONTRACTS.iter().find(|spec| spec.name == name)
}

/// Sorted state and method member indexes verified against `p2_contracts/*.json`.
pub mod indexes {
    pub mod sessions {
        pub const VARIANT_GUEST: u64 = 0;
        pub const VARIANT_OWNER: u64 = 1;
        pub const STATE_OWNER_DEFAULT_CWD: u64 = 0;
        pub const STATE_OWNER_DEFAULT_ENV: u64 = 1;
        pub const STATE_OWNER_OPERATIONS: u64 = 2;
        pub const STATE_OWNER_SESSIONS: u64 = 3;
        pub const STATE_GUEST_SESSIONS: u64 = 0;
        pub const METHOD_CREATE: u64 = 2;
    }

    pub mod chat {
        pub const VARIANT_PARTICIPANT: u64 = 0;
        pub const STATE_LIVE_SHELL: u64 = 0;
        pub const STATE_MODEL: u64 = 1;
        pub const STATE_STATS: u64 = 2;
        pub const STATE_VIEW: u64 = 3;
        pub const METHOD_CONFIGURE: u64 = 3;
        pub const METHOD_SEND: u64 = 8;
        pub const METHOD_STOP: u64 = 10;
    }

    pub mod models {
        pub const STATE_DEFAULT_MODEL: u64 = 0;
        pub const STATE_ENABLED_MODELS: u64 = 1;
        pub const STATE_ERRORS: u64 = 2;
        pub const STATE_MODELS: u64 = 3;
        pub const STATE_RESOURCES: u64 = 4;
        pub const STATE_SCOPED_MODELS: u64 = 5;
        pub const METHOD_SET_DEFAULT: u64 = 0;
        pub const METHOD_SET_ENABLED_MODELS: u64 = 1;
    }

    pub mod settings {
        pub const STATE_ERROR: u64 = 0;
        pub const STATE_SETTINGS: u64 = 1;
        pub const METHOD_UPDATE: u64 = 0;
    }

    pub mod mcp {
        pub const STATE_ERRORS: u64 = 0;
        pub const STATE_INSTANCES: u64 = 1;
        pub const STATE_SIGN_INS: u64 = 2;
        pub const METHOD_CANCEL_SIGN_IN: u64 = 0;
        pub const METHOD_PASTE: u64 = 1;
        pub const METHOD_RESTART: u64 = 2;
        pub const METHOD_SIGN_IN: u64 = 3;
        pub const METHOD_SIGN_OUT: u64 = 4;
    }

    pub mod sharing {
        pub const STATE_ENV: u64 = 0;
        pub const STATE_GUESTS: u64 = 1;
        pub const STATE_LINKS: u64 = 2;
        pub const STATE_PEOPLE: u64 = 3;
        pub const STATE_PEOPLE_URL: u64 = 4;
        pub const METHOD_LINK: u64 = 0;
        pub const METHOD_REMOVE: u64 = 1;
        pub const METHOD_SHARE: u64 = 2;
    }

    pub mod session_status {
        pub const STATE_STATUSES: u64 = 0;
    }

    pub mod chat_costs {
        pub const METHOD_COSTS: u64 = 0;
    }

    pub mod artifacts {
        pub const STATE_ARTIFACTS: u64 = 0;
        pub const METHOD_READ: u64 = 0;
        pub const METHOD_REMOVE: u64 = 1;
    }

    pub mod tree {
        pub const METHOD_BRANCH: u64 = 0;
        pub const METHOD_NAVIGATE: u64 = 1;
        pub const METHOD_RENAME: u64 = 2;
        pub const METHOD_SELECT: u64 = 3;
        pub const METHOD_TREE: u64 = 4;
    }

    pub mod terminals {
        pub const METHOD_ACK: u64 = 0;
        pub const METHOD_ATTACH: u64 = 1;
        pub const METHOD_CHANNEL: u64 = 2;
        pub const METHOD_CLOSE: u64 = 3;
        pub const METHOD_LIST: u64 = 4;
        pub const METHOD_OPEN: u64 = 5;
        pub const METHOD_RESIZE: u64 = 6;
        pub const METHOD_RESTART_HOST: u64 = 7;
        pub const METHOD_THEME: u64 = 8;
    }

    pub mod voice_mode {
        pub const STATE_ENABLED: u64 = 0;
        pub const STATE_VOICE: u64 = 1;
        pub const METHOD_CHOOSE: u64 = 0;
        pub const METHOD_SET: u64 = 1;
    }

    pub mod voice_service {
        pub const STATE_FAILURE: u64 = 0;
        pub const STATE_VOICES: u64 = 1;
    }

    pub mod session_files {
        pub const VARIANT_SERVER: u64 = 0;
        pub const METHOD_APPEND: u64 = 0;
        pub const METHOD_BEGIN: u64 = 1;
        pub const METHOD_CANCEL: u64 = 2;
        pub const METHOD_DOWNLOAD: u64 = 3;
        pub const METHOD_FINISH: u64 = 4;
        pub const METHOD_READ: u64 = 5;
    }

    pub mod attachments {
        pub const METHOD_RESOLVE: u64 = 0;
    }

    pub mod plugin_reload {
        pub const METHOD_RELOAD: u64 = 0;
    }
}

/// Encodes a Chord `Hold` frame (kind = 1).
pub fn encode_hold_frame(
    hold_id: u64,
    spec: &ContractSpec,
    params: &Value,
    route: &Value,
) -> Vec<u8> {
    let mut buf = vec![1];
    encode_varint(hold_id, &mut buf);
    encode_string(spec.name, &mut buf);
    encode_varint(spec.version, &mut buf);
    buf.extend(spec.fingerprint.to_be_bytes());
    buf.extend(encode_part(params.to_string().as_bytes(), false));
    encode_string(spec.address_kind.as_str(), &mut buf);
    encode_string(&route.to_string(), &mut buf);
    buf.push(0);
    buf
}

/// Encodes a Chord `Release` frame (kind = 2).
pub fn encode_release_frame(hold_id: u64) -> Vec<u8> {
    let mut buf = vec![2];
    encode_varint(hold_id, &mut buf);
    buf
}

/// Encodes a Chord `Call` frame (kind = 3) with JSON-only payload (flags = 1).
pub fn encode_call_frame(call_id: u64, hold_id: u64, method: u64, args: &Value) -> Vec<u8> {
    let mut buf = vec![3];
    for field in [call_id, hold_id, method, 1] {
        encode_varint(field, &mut buf);
    }
    buf.extend(encode_part(args.to_string().as_bytes(), false));
    buf
}

/// Encodes a Chord `Call` frame (kind = 3) carrying binary byte segments (flags = 3).
pub fn encode_call_frame_with_segments(
    call_id: u64,
    hold_id: u64,
    method: u64,
    args: &Value,
    segments: &[&[u8]],
) -> Vec<u8> {
    if segments.is_empty() {
        return encode_call_frame(call_id, hold_id, method, args);
    }
    let mut buf = vec![3];
    for field in [call_id, hold_id, method, 3] {
        encode_varint(field, &mut buf);
    }
    buf.extend(encode_part(args.to_string().as_bytes(), false));
    encode_varint(segments.len() as u64, &mut buf);
    for seg in segments {
        encode_varint(seg.len() as u64, &mut buf);
    }
    for seg in segments {
        buf.extend_from_slice(seg);
    }
    buf
}

/// Builds a `Hold` frame for a contract using its canonical address and params shape.
pub fn hold_for_contract(
    hold_id: u64,
    spec: &ContractSpec,
    session: Option<&str>,
    conversation: Option<u64>,
    view: Option<&str>,
) -> Vec<u8> {
    let (params, route) = match spec.name {
        "p2.chat" | "p2.chat-costs" | "p2.voice-mode" => {
            let sid = session.unwrap_or("");
            let conv = conversation.unwrap_or(1);
            (
                json!({ "session": sid, "conversation": conv }),
                json!({ "id": sid }),
            )
        }
        "p2.tree" | "p2.session-files" => {
            let sid = session.unwrap_or("");
            (json!({ "session": sid }), json!({ "id": sid }))
        }
        "p2.sharing" | "p2.artifacts" => {
            let sid = session.unwrap_or("");
            (json!({ "session": sid }), json!({}))
        }
        "p2.terminals" => {
            let sid = session.unwrap_or("");
            let v = view.unwrap_or("web");
            (json!({ "session": sid, "view": v }), json!({}))
        }
        _ => (json!({}), json!({})),
    };
    encode_hold_frame(hold_id, spec, &params, &route)
}

/// Role of a projected transcript row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectedRole {
    User,
    Assistant,
    Tool,
}

/// Tool call activity card projected via `crate::activity::*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatActivity {
    pub step: Step,
    pub described: Described,
}

/// Rich projected transcript message for `Chat` rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedMessage {
    pub id: String,
    pub role: ProjectedRole,
    pub text: String,
    pub streaming: bool,
    pub activities: Vec<ChatActivity>,
}

fn extract_text_blocks(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Projects `p2.chat` `view` state into user, assistant, streaming, and tool/activity rows.
pub fn project_chat_transcript(view: &Value, tools: &ToolActivityTable) -> Vec<ProjectedMessage> {
    let entries = view["entries"].as_array();
    let slots = view["docs"]["pi.live"]["slots"].as_object();

    let mut results_by_call: HashMap<String, &Value> = HashMap::new();
    for entry in entries.into_iter().flatten() {
        let model = &entry["model"][0];
        if model["role"].as_str() == Some("toolResult")
            && let Some(call_id) = model["toolCallId"].as_str()
        {
            results_by_call.insert(call_id.to_owned(), model);
        }
    }

    let mut called_ids = std::collections::HashSet::new();
    let mut rows = Vec::new();

    for entry in entries.into_iter().flatten() {
        let id = entry["id"].to_string();
        let data = &entry["data"];
        if matches!(data["author"]["kind"].as_str(), Some("owner" | "guest"))
            && data["attachments"].is_array()
            && let Some(text) = data["text"].as_str()
        {
            rows.push(ProjectedMessage {
                id,
                role: ProjectedRole::User,
                text: text.to_owned(),
                streaming: false,
                activities: Vec::new(),
            });
            continue;
        }

        let model = &entry["model"][0];
        match model["role"].as_str() {
            Some("user") => {
                rows.push(ProjectedMessage {
                    id,
                    role: ProjectedRole::User,
                    text: extract_text_blocks(&model["content"]),
                    streaming: false,
                    activities: Vec::new(),
                });
            }
            Some("assistant") => {
                let text = extract_text_blocks(&model["content"]);
                let mut activities = Vec::new();
                if let Some(parts) = model["content"].as_array() {
                    for part in parts {
                        if part["type"] == "toolCall"
                            && let Some(call_id) = part["id"].as_str()
                            && let Some(name) = part["name"].as_str()
                        {
                            called_ids.insert(call_id.to_owned());
                            let step = if let Some(res) = results_by_call.get(call_id) {
                                let is_err = res["isError"].as_bool().unwrap_or(false);
                                Step {
                                    id: call_id.to_owned(),
                                    name: name.to_owned(),
                                    status: if is_err {
                                        StepStatus::Error
                                    } else {
                                        StepStatus::Done
                                    },
                                    output: extract_text_blocks(&res["content"]),
                                    duration_ms: res["durationMs"].as_u64(),
                                }
                            } else if let Some(slot) = slots.and_then(|s| s.get(call_id)) {
                                let status = slot["status"]
                                    .as_str()
                                    .map(call_status)
                                    .unwrap_or(StepStatus::Running);
                                Step {
                                    id: call_id.to_owned(),
                                    name: name.to_owned(),
                                    status,
                                    output: slot["output"].as_str().unwrap_or("").to_owned(),
                                    duration_ms: None,
                                }
                            } else {
                                Step::new(call_id, name, StepStatus::Running)
                            };
                            let described = describe(&step, tools);
                            activities.push(ChatActivity { step, described });
                        }
                    }
                }
                rows.push(ProjectedMessage {
                    id,
                    role: ProjectedRole::Assistant,
                    text,
                    streaming: false,
                    activities,
                });
            }
            Some("toolResult") => {
                let call_id = model["toolCallId"].as_str().unwrap_or("");
                if !called_ids.contains(call_id) {
                    let name = model["toolName"].as_str().unwrap_or("tool");
                    let is_err = model["isError"].as_bool().unwrap_or(false);
                    let step = Step {
                        id: if call_id.is_empty() {
                            id.clone()
                        } else {
                            call_id.to_owned()
                        },
                        name: name.to_owned(),
                        status: if is_err {
                            StepStatus::Error
                        } else {
                            StepStatus::Done
                        },
                        output: extract_text_blocks(&model["content"]),
                        duration_ms: model["durationMs"].as_u64(),
                    };
                    let described = describe(&step, tools);
                    rows.push(ProjectedMessage {
                        id,
                        role: ProjectedRole::Tool,
                        text: String::new(),
                        streaming: false,
                        activities: vec![ChatActivity { step, described }],
                    });
                }
            }
            _ => {}
        }
    }

    let live = &view["docs"]["pi.live"]["generation"]["message"];
    if matches!(live["role"].as_str(), Some("assistant" | "user")) {
        let role = if live["role"].as_str() == Some("user") {
            ProjectedRole::User
        } else {
            ProjectedRole::Assistant
        };
        rows.push(ProjectedMessage {
            id: "live".to_owned(),
            role,
            text: extract_text_blocks(&live["content"]),
            streaming: true,
            activities: Vec::new(),
        });
    }

    rows
}

/// Which right-hand plugin side panel is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidePanel {
    #[default]
    None,
    Cost,
    Artifacts,
    Models,
    Settings,
    Mcp,
    Sharing,
    Tree,
    Voice,
}

impl SidePanel {
    pub const fn is_open(self) -> bool {
        !matches!(self, Self::None)
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cost => "cost",
            Self::Artifacts => "artifacts",
            Self::Models => "models",
            Self::Settings => "settings",
            Self::Mcp => "mcp",
            Self::Sharing => "sharing",
            Self::Tree => "tree",
            Self::Voice => "voice",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Cost => "Cost",
            Self::Artifacts => "Artifacts",
            Self::Models => "Models",
            Self::Settings => "Settings",
            Self::Mcp => "MCP Servers",
            Self::Sharing => "Sharing",
            Self::Tree => "Session Tree",
            Self::Voice => "Voice",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "none" => Some(Self::None),
            "cost" => Some(Self::Cost),
            "artifacts" => Some(Self::Artifacts),
            "models" => Some(Self::Models),
            "settings" => Some(Self::Settings),
            "mcp" => Some(Self::Mcp),
            "sharing" => Some(Self::Sharing),
            "tree" => Some(Self::Tree),
            "voice" => Some(Self::Voice),
            _ => None,
        }
    }
}

/// Built-in commands exposed in the `CommandMenu`.
pub fn builtin_commands() -> Vec<CommandItem> {
    vec![
        CommandItem {
            id: "new-session".into(),
            title: "New session".into(),
            keywords: vec!["create".into(), "chat".into()],
            section: "Sessions".into(),
        },
        CommandItem {
            id: "pane-chat".into(),
            title: "Show chat".into(),
            keywords: vec!["conversation".into(), "transcript".into()],
            section: "Panes".into(),
        },
        CommandItem {
            id: "pane-terminal".into(),
            title: "Open terminal".into(),
            keywords: vec!["shell".into(), "pty".into()],
            section: "Panes".into(),
        },
        CommandItem {
            id: "panel-cost".into(),
            title: "Show cost".into(),
            keywords: vec!["usage".into(), "spend".into(), "tokens".into()],
            section: "Panels".into(),
        },
        CommandItem {
            id: "panel-artifacts".into(),
            title: "Show artifacts".into(),
            keywords: vec!["files".into(), "preview".into()],
            section: "Panels".into(),
        },
        CommandItem {
            id: "panel-models".into(),
            title: "Models".into(),
            keywords: vec!["provider".into(), "default".into()],
            section: "Settings".into(),
        },
        CommandItem {
            id: "panel-settings".into(),
            title: "Settings".into(),
            keywords: vec!["preferences".into(), "config".into()],
            section: "Settings".into(),
        },
        CommandItem {
            id: "panel-mcp".into(),
            title: "MCP servers".into(),
            keywords: vec!["tools".into(), "oauth".into()],
            section: "Settings".into(),
        },
        CommandItem {
            id: "panel-sharing".into(),
            title: "Sharing".into(),
            keywords: vec!["guests".into(), "links".into()],
            section: "Session".into(),
        },
        CommandItem {
            id: "panel-tree".into(),
            title: "Session tree".into(),
            keywords: vec!["branches".into(), "navigate".into()],
            section: "Session".into(),
        },
        CommandItem {
            id: "panel-voice".into(),
            title: "Voice".into(),
            keywords: vec!["audio".into(), "tts".into(), "mic".into()],
            section: "Session".into(),
        },
    ]
}

/// Decoded `p2.models` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelEntry {
    pub provider: String,
    pub model_id: String,
    pub name: String,
    pub context_window: u64,
    pub thinking_levels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelsView {
    pub models: Vec<ModelEntry>,
    pub default_provider: Option<String>,
    pub default_model_id: Option<String>,
    pub default_thinking: Option<String>,
    pub enabled_models: Vec<String>,
    pub errors: Vec<String>,
}

pub fn decode_models_view(
    models_val: Option<&Value>,
    default_val: Option<&Value>,
    enabled_val: Option<&Value>,
    errors_val: Option<&Value>,
) -> ModelsView {
    let models = models_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(ModelEntry {
                provider: m["provider"].as_str()?.to_owned(),
                model_id: m["modelId"].as_str()?.to_owned(),
                name: m["name"].as_str().unwrap_or("").to_owned(),
                context_window: m["contextWindow"].as_u64().unwrap_or(0),
                thinking_levels: m["thinkingLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect(),
            })
        })
        .collect();
    let default_provider = default_val
        .and_then(|v| v["provider"].as_str())
        .map(str::to_owned);
    let default_model_id = default_val
        .and_then(|v| v["modelId"].as_str())
        .map(str::to_owned);
    let default_thinking = default_val
        .and_then(|v| v["thinkingLevel"].as_str())
        .map(str::to_owned);
    let enabled_models = enabled_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let errors = errors_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    ModelsView {
        models,
        default_provider,
        default_model_id,
        default_thinking,
        enabled_models,
        errors,
    }
}

/// Decoded `p2.settings` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsView {
    pub entries: IndexMap<String, String>,
    pub error: Option<String>,
}

pub fn decode_settings_view(
    settings_val: Option<&Value>,
    error_val: Option<&Value>,
) -> SettingsView {
    let mut entries = IndexMap::new();
    if let Some(obj) = settings_val.and_then(Value::as_object) {
        for (k, v) in obj {
            let formatted = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            entries.insert(k.clone(), formatted);
        }
    }
    let error = error_val.and_then(Value::as_str).map(str::to_owned);
    SettingsView { entries, error }
}

/// Decoded `p2.mcp` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpInstanceView {
    pub id: String,
    pub server: String,
    pub source: String,
    pub status: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServersView {
    pub instances: Vec<McpInstanceView>,
    pub errors: Vec<String>,
    pub sign_ins: Vec<String>,
}

pub fn decode_mcp_view(
    instances_val: Option<&Value>,
    errors_val: Option<&Value>,
    sign_ins_val: Option<&Value>,
) -> McpServersView {
    let instances = instances_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(McpInstanceView {
                id: item["id"].as_str()?.to_owned(),
                server: item["server"].as_str()?.to_owned(),
                source: item["source"].as_str().unwrap_or("owner").to_owned(),
                status: item["status"].as_str().unwrap_or("starting").to_owned(),
                tool_count: item["tools"].as_array().map_or(0, Vec::len),
                error: item["error"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let errors = errors_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let sign_ins = sign_ins_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v["server"].as_str().map(str::to_owned))
        .collect();
    McpServersView {
        instances,
        errors,
        sign_ins,
    }
}

/// Decoded `p2.sharing` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareLinkView {
    pub mode: String,
    pub token: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareGuestView {
    pub id: String,
    pub name: String,
    pub mode: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharingView {
    pub env: Option<String>,
    pub links: Vec<ShareLinkView>,
    pub guests: Vec<ShareGuestView>,
    pub people_url: Option<String>,
}

pub fn decode_sharing_view(
    env_val: Option<&Value>,
    links_val: Option<&Value>,
    guests_val: Option<&Value>,
    people_url_val: Option<&Value>,
) -> SharingView {
    let env = env_val.and_then(Value::as_str).map(str::to_owned);
    let links = links_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(ShareLinkView {
                mode: l["mode"].as_str()?.to_owned(),
                token: l["token"].as_str()?.to_owned(),
                url: l["url"].as_str()?.to_owned(),
            })
        })
        .collect();
    let guests = guests_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| {
            Some(ShareGuestView {
                id: g["id"].as_str()?.to_owned(),
                name: g["name"].as_str()?.to_owned(),
                mode: g["mode"].as_str()?.to_owned(),
            })
        })
        .collect();
    let people_url = people_url_val.and_then(Value::as_str).map(str::to_owned);
    SharingView {
        env,
        links,
        guests,
        people_url,
    }
}

/// Decoded `p2.session-status` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStatusEntry {
    pub running: bool,
    pub busy_conversations: Vec<u64>,
    pub tasks: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStatusView {
    pub statuses: HashMap<String, SessionStatusEntry>,
}

pub fn decode_session_status_view(statuses_val: Option<&Value>) -> SessionStatusView {
    let mut statuses = HashMap::new();
    if let Some(obj) = statuses_val.and_then(Value::as_object) {
        for (sid, entry) in obj {
            let busy_conversations = entry["busy"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect();
            statuses.insert(
                sid.clone(),
                SessionStatusEntry {
                    running: entry["running"].as_bool().unwrap_or(false),
                    busy_conversations,
                    tasks: entry["tasks"].as_u64().unwrap_or(0),
                    error: entry["error"]
                        .as_str()
                        .or_else(|| entry["failure"]["reason"].as_str())
                        .map(str::to_owned),
                },
            );
        }
    }
    SessionStatusView { statuses }
}

/// Decodes `p2.chat-costs` `costs()` call result into `AssistantCost` rows for `CostPanel`.
pub fn decode_costs_view(value: &Value) -> Vec<AssistantCost> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(AssistantCost {
                entry_id: entry["entryId"].as_str()?.to_owned(),
                cost: entry["cost"].as_f64()?,
                cache_hit: entry["cacheHit"].as_f64(),
                model: entry["model"].as_str().unwrap_or("").to_owned(),
                stop_reason: entry["stopReason"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

/// Decoded `p2.artifacts` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactSummaryView {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub version: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactsView {
    pub items: Vec<ArtifactSummaryView>,
    pub selected_id: Option<String>,
    pub selected_content: Option<String>,
}

pub fn decode_artifacts_view(artifacts_val: Option<&Value>) -> Vec<ArtifactSummaryView> {
    artifacts_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(ArtifactSummaryView {
                id: a["id"].as_str()?.to_owned(),
                kind: a["kind"].as_str().unwrap_or("md").to_owned(),
                title: a["title"].as_str().unwrap_or("").to_owned(),
                version: a["version"].as_u64().unwrap_or(1),
            })
        })
        .collect()
}

pub fn decode_artifact_content(read_result: &Value) -> Option<(String, String)> {
    let id = read_result["summary"]["id"].as_str()?.to_owned();
    let content = read_result["content"].as_str()?.to_owned();
    Some((id, content))
}

/// Decoded `p2.tree` view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeBranchView {
    pub conversation: u64,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeNodeView {
    pub conversation: u64,
    pub entry: u64,
    pub parent: Option<u64>,
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeView {
    pub current: Option<u64>,
    pub main: u64,
    pub branches: Vec<TreeBranchView>,
    pub nodes: Vec<TreeNodeView>,
}

pub fn decode_tree_view(value: &Value) -> TreeView {
    let branches = value["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(TreeBranchView {
                conversation: b["conversation"].as_u64()?,
                label: b["label"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let nodes = value["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            Some(TreeNodeView {
                conversation: n["conversation"].as_u64().unwrap_or(1),
                entry: n["entry"].as_u64()?,
                parent: n["parent"].as_u64(),
                role: n["role"].as_str().unwrap_or("").to_owned(),
                text: n["text"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect();
    TreeView {
        current: value["current"].as_u64(),
        main: value["main"].as_u64().unwrap_or(1),
        branches,
        nodes,
    }
}

/// Decoded `p2.terminals` view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalsView {
    pub status: TerminalStatus,
    pub title: String,
    pub lines: Vec<String>,
    pub active_id: Option<String>,
    pub available: Vec<String>,
}

impl Default for TerminalsView {
    fn default() -> Self {
        Self {
            status: TerminalStatus::Closed,
            title: "Terminal".to_owned(),
            lines: Vec::new(),
            active_id: None,
            available: Vec::new(),
        }
    }
}

pub fn decode_terminals_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| item["id"].as_str())
                .or_else(|| item["terminalId"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

/// Decoded `p2.voice-mode` + `p2.voice-service` view.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceView {
    pub enabled: bool,
    pub engine: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f64>,
    pub available_engines: Vec<String>,
    pub failure: Option<String>,
    pub state: VoiceState,
}

impl Default for VoiceView {
    fn default() -> Self {
        Self {
            enabled: false,
            engine: None,
            voice: None,
            speed: None,
            available_engines: Vec::new(),
            failure: None,
            state: VoiceState::Idle,
        }
    }
}

pub fn decode_voice_view(
    enabled_val: Option<&Value>,
    voice_val: Option<&Value>,
    voices_val: Option<&Value>,
    failure_val: Option<&Value>,
) -> VoiceView {
    let enabled = enabled_val.and_then(Value::as_bool).unwrap_or(false);
    let engine = voice_val
        .and_then(|v| v["engine"].as_str())
        .map(str::to_owned);
    let voice = voice_val
        .and_then(|v| v["voice"].as_str())
        .map(str::to_owned);
    let speed = voice_val.and_then(|v| v["speed"].as_f64());
    let available_engines = voices_val
        .and_then(|v| v["engines"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| e["id"].as_str().map(str::to_owned))
        .collect();
    let failure = failure_val.and_then(Value::as_str).map(str::to_owned);
    let state = if enabled {
        VoiceState::Listening
    } else {
        VoiceState::Idle
    };
    VoiceView {
        enabled,
        engine,
        voice,
        speed,
        available_engines,
        failure,
        state,
    }
}

/// Composer attachment manager backed by `crate::state::composer_state` and `crate::state::attachments`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposerAttachments {
    pub files: Vec<PickedFile>,
    pub last_too_large: Option<String>,
    pub uploads: IndexMap<String, UploadEntry>,
    pub last_error: Option<String>,
}

impl ComposerAttachments {
    pub fn add_files<F, G>(&mut self, chosen: &[(String, u64, bool)], key_for: F, is_image: G)
    where
        F: Fn(usize) -> String,
        G: Fn(&str) -> bool,
    {
        let res = pick_files(chosen, key_for, is_image);
        self.last_too_large = res.too_large;
        self.files.extend(res.picked);
    }

    pub fn remove(&mut self, key: &str) {
        self.files.retain(|f| f.key != key);
        self.uploads.shift_remove(key);
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.last_too_large = None;
        self.uploads.clear();
        self.last_error = None;
    }

    pub fn referenced_for_draft(&self, draft: &str) -> Vec<PickedFile> {
        referenced_files(draft, &self.files)
    }
}

// ---------------------------------------------------------------------------
// Browser-side dynamic plugin hot-reload & draft preservation.
// ---------------------------------------------------------------------------

/// WebSocket close code sent by `p2` when the browser's web bundle is stale (`p2: stale page, reload`).
pub const RELOAD_CLOSE_CODE: u16 = 4001;
/// `localStorage` key for preserving the active composer draft across reloads.
pub const DRAFT_STORAGE_KEY: &str = "p2-draft";
/// `localStorage` key for preserving active session/panel/draft metadata across hot-reload.
pub const RELOAD_STATE_STORAGE_KEY: &str = "p2-reload-state";
/// Window `CustomEvent` name for dynamic browser plugin manifest hot-swaps.
pub const PLUGIN_RELOAD_EVENT: &str = "pi-plugin-reload";

/// Reason a browser reload or plugin hot-swap was triggered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadTrigger {
    None,
    CloseFrame { code: u16, reason: String },
    AssetRevisionChanged { current: String, remote: String },
    PluginRevisionChanged { previous: String, next: String },
}

/// Whether a WebSocket close code/reason signals a server-initiated reload.
pub fn is_reload_close_frame(code: u16, reason: &str) -> bool {
    if code == RELOAD_CLOSE_CODE {
        return true;
    }
    let lower = reason.to_ascii_lowercase();
    if (4000..=4999).contains(&code) && lower.contains("reload") {
        return true;
    }
    lower == "reload" || lower.contains("stale page")
}

/// Classifies a WebSocket close frame into a `ReloadTrigger`.
pub fn detect_close_reload(code: u16, reason: &str) -> ReloadTrigger {
    if is_reload_close_frame(code, reason) {
        ReloadTrigger::CloseFrame {
            code,
            reason: reason.to_owned(),
        }
    } else {
        ReloadTrigger::None
    }
}

/// Detects whether `/_chord/web/base` reports a different asset bundle hash than the loaded page.
pub fn detect_asset_revision_change(loaded_base: &str, remote_base: &str) -> ReloadTrigger {
    let loaded = loaded_base.trim();
    let remote = remote_base.trim().trim_matches('"');
    if !loaded.is_empty() && !remote.is_empty() && loaded != remote {
        ReloadTrigger::AssetRevisionChanged {
            current: loaded.to_owned(),
            remote: remote.to_owned(),
        }
    } else {
        ReloadTrigger::None
    }
}

/// Canonical draft storage key for `(session_id, conversation)`.
pub fn draft_key_for(session_id: &str, conversation: u64) -> String {
    format!("{session_id}#{conversation}")
}

/// State preserved across a browser-triggered reload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadSnapshot {
    pub session_id: Option<String>,
    pub conversation: Option<u64>,
    pub panel_id: Option<String>,
    pub draft_key: Option<String>,
    pub draft_text: String,
    pub asset_revision: String,
    pub plugin_revision: Option<String>,
}

pub fn encode_reload_snapshot(snapshot: &ReloadSnapshot) -> String {
    serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".into())
}

pub fn decode_reload_snapshot(raw: &str) -> Option<ReloadSnapshot> {
    serde_json::from_str::<ReloadSnapshot>(raw).ok()
}

/// Restores a saved `p2-draft` JSON string for `expected_key`, stripping stale attachment keys.
pub fn restore_draft_for_conversation(
    raw_draft: Option<&str>,
    expected_key: &str,
) -> Option<String> {
    let (key, text) = decode_draft(raw_draft?)?;
    if key != expected_key {
        return None;
    }
    Some(rename_references(&text))
}

/// Formats `p2-draft` JSON for `(session_id, conversation)` and `text`.
pub fn format_draft_storage(session_id: &str, conversation: u64, text: &str) -> String {
    encode_draft(&draft_key_for(session_id, conversation), text)
}

/// Dynamic descriptor for a browser plugin panel/command entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub title: String,
    pub revision: String,
    #[serde(default)]
    pub contract: Option<String>,
    #[serde(default)]
    pub panel: Option<String>,
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub section: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Outcome of applying a hot-swap plugin descriptor update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotSwapReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub revision_changed: bool,
    pub closed_active_panel: bool,
}

/// Registry of active browser plugin descriptors supporting live hot-swap updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRegistryState {
    pub revision: String,
    pub plugins: IndexMap<String, PluginDescriptor>,
}

impl Default for PluginRegistryState {
    fn default() -> Self {
        Self::builtin()
    }
}

impl PluginRegistryState {
    pub fn builtin() -> Self {
        let entries = [
            (
                "models",
                "Models",
                Some(P2_MODELS.name),
                "models",
                "panel-models",
                &["provider", "default"][..],
                "Settings",
            ),
            (
                "settings",
                "Settings",
                Some(P2_SETTINGS.name),
                "settings",
                "panel-settings",
                &["preferences", "config"][..],
                "Settings",
            ),
            (
                "mcp",
                "MCP Servers",
                Some(P2_MCP.name),
                "mcp",
                "panel-mcp",
                &["tools", "oauth"][..],
                "Settings",
            ),
            (
                "sharing",
                "Sharing",
                Some(P2_SHARING.name),
                "sharing",
                "panel-sharing",
                &["guests", "links"][..],
                "Session",
            ),
            (
                "cost",
                "Cost",
                Some(P2_CHAT_COSTS.name),
                "cost",
                "panel-cost",
                &["usage", "spend", "tokens"][..],
                "Panels",
            ),
            (
                "artifacts",
                "Artifacts",
                Some(P2_ARTIFACTS.name),
                "artifacts",
                "panel-artifacts",
                &["files", "preview"][..],
                "Panels",
            ),
            (
                "tree",
                "Session Tree",
                Some(P2_TREE.name),
                "tree",
                "panel-tree",
                &["branches", "navigate"][..],
                "Session",
            ),
            (
                "voice",
                "Voice",
                Some(P2_VOICE_MODE.name),
                "voice",
                "panel-voice",
                &["audio", "tts", "mic"][..],
                "Session",
            ),
        ];
        let mut plugins = IndexMap::new();
        for (id, title, contract, panel, cmd, kw, section) in entries {
            plugins.insert(
                id.to_owned(),
                PluginDescriptor {
                    id: id.to_owned(),
                    title: title.to_owned(),
                    revision: "builtin-1".to_owned(),
                    contract: contract.map(str::to_owned),
                    panel: Some(panel.to_owned()),
                    command_id: Some(cmd.to_owned()),
                    keywords: kw.iter().map(|s| (*s).to_owned()).collect(),
                    section: section.to_owned(),
                    enabled: true,
                },
            );
        }
        Self {
            revision: "builtin-1".to_owned(),
            plugins,
        }
    }

    /// Hot-swaps the active plugin descriptors to `descriptors` at `next_revision`.
    /// Preserves session selection and draft; only signals `closed_active_panel` if the
    /// currently open `active_panel_id` was removed or disabled.
    pub fn hot_swap(
        &mut self,
        next_revision: String,
        descriptors: Vec<PluginDescriptor>,
        active_panel_id: Option<&str>,
    ) -> HotSwapReport {
        let mut report = HotSwapReport {
            revision_changed: self.revision != next_revision,
            ..HotSwapReport::default()
        };
        let mut next = IndexMap::new();
        for desc in descriptors {
            if let Some(prev) = self.plugins.get(&desc.id) {
                if prev != &desc {
                    report.updated.push(desc.id.clone());
                }
            } else {
                report.added.push(desc.id.clone());
            }
            next.insert(desc.id.clone(), desc);
        }
        for id in self.plugins.keys() {
            if !next.contains_key(id) {
                report.removed.push(id.clone());
            }
        }
        if let Some(active) = active_panel_id {
            let still_open = next
                .values()
                .any(|d| d.enabled && d.panel.as_deref() == Some(active));
            report.closed_active_panel = !still_open;
        }
        self.revision = next_revision;
        self.plugins = next;
        report
    }

    /// Returns enabled plugin descriptors that expose a side panel.
    pub fn active_panels(&self) -> Vec<&PluginDescriptor> {
        self.plugins
            .values()
            .filter(|d| d.enabled && d.panel.is_some())
            .collect()
    }

    /// Returns command-menu items combining core navigation with currently active plugins.
    pub fn dynamic_commands(&self) -> Vec<CommandItem> {
        let mut items = vec![
            CommandItem {
                id: "new-session".into(),
                title: "New session".into(),
                keywords: vec!["create".into(), "chat".into()],
                section: "Sessions".into(),
            },
            CommandItem {
                id: "pane-chat".into(),
                title: "Show chat".into(),
                keywords: vec!["conversation".into(), "transcript".into()],
                section: "Panes".into(),
            },
            CommandItem {
                id: "pane-terminal".into(),
                title: "Open terminal".into(),
                keywords: vec!["shell".into(), "pty".into()],
                section: "Panes".into(),
            },
            CommandItem {
                id: "toggle-theme".into(),
                title: "Toggle theme".into(),
                keywords: vec!["light".into(), "dark".into(), "appearance".into()],
                section: "Appearance".into(),
            },
        ];
        for desc in self.plugins.values().filter(|d| d.enabled) {
            if let Some(cmd_id) = &desc.command_id {
                items.push(CommandItem {
                    id: cmd_id.clone(),
                    title: desc.title.clone(),
                    keywords: desc.keywords.clone(),
                    section: if desc.section.is_empty() {
                        "Plugins".into()
                    } else {
                        desc.section.clone()
                    },
                });
            }
        }
        items
    }
}

/// Decodes a `pi-plugin-reload` JSON manifest (`{ "revision": "...", "plugins": [...] }`).
pub fn decode_plugin_manifest(value: &Value) -> Option<(String, Vec<PluginDescriptor>)> {
    let revision = value.get("revision")?.as_str()?.to_owned();
    let plugins_val = value.get("plugins")?.as_array()?;
    let plugins = plugins_val
        .iter()
        .filter_map(|item| serde_json::from_value::<PluginDescriptor>(item.clone()).ok())
        .collect();
    Some((revision, plugins))
}

/// Lifecycle state of an active or failed Chord contract hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldStatus {
    Pending,
    Live,
    Unavailable(String),
}

/// Tracks core vs optional plugin holds for graceful fault isolation.
#[derive(Clone, Debug, Default)]
pub struct HoldTracker {
    holds: HashMap<u64, (&'static str, bool, HoldStatus)>,
}

impl HoldTracker {
    pub fn register(&mut self, hold_id: u64, contract_name: &'static str, required: bool) {
        self.holds
            .insert(hold_id, (contract_name, required, HoldStatus::Pending));
    }

    pub fn mark_live(&mut self, hold_id: u64) {
        if let Some(entry) = self.holds.get_mut(&hold_id) {
            entry.2 = HoldStatus::Live;
        }
    }

    /// Marks a hold as failed/gone. Returns `Some(reason)` only if the hold was required;
    /// optional plugin holds are isolated so core chat stays connected and writable.
    pub fn mark_failed(&mut self, hold_id: u64, reason: String) -> Option<String> {
        if let Some(entry) = self.holds.get_mut(&hold_id) {
            let required = entry.1;
            entry.2 = HoldStatus::Unavailable(reason.clone());
            if required {
                return Some(reason);
            }
        }
        None
    }

    pub fn unregister(&mut self, hold_id: u64) {
        self.holds.remove(&hold_id);
    }

    pub fn status_of(&self, contract_name: &str) -> Option<&HoldStatus> {
        self.holds
            .values()
            .find(|(name, _, _)| *name == contract_name)
            .map(|(_, _, status)| status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord_bridge::{decode_part, decode_varint};

    #[test]
    fn hold_frame_encodes_canonical_fingerprint_and_address() {
        let frame = hold_for_contract(7, &P2_MODELS, None, None, None);
        assert_eq!(frame[0], 1);
        let (hold_id, n1) = decode_varint(&frame[1..]).expect("hold_id");
        assert_eq!(hold_id, 7);
        let mut pos = 1 + n1;
        let (name_len, n2) = decode_varint(&frame[pos..]).expect("name_len");
        pos += n2;
        assert_eq!(&frame[pos..pos + name_len as usize], b"p2.models");
        pos += name_len as usize;
        let (ver, n3) = decode_varint(&frame[pos..]).expect("version");
        assert_eq!(ver, 2);
        pos += n3;
        let fp = u64::from_be_bytes(frame[pos..pos + 8].try_into().expect("fp bytes"));
        assert_eq!(fp, 0xb53f30ff1090f182);
        pos += 8;
        let (part_flags, n4) = decode_varint(&frame[pos..]).expect("part_flags");
        let part_len = (part_flags / 2) as usize;
        let (params_bytes, _) = decode_part(&frame[pos..pos + n4 + part_len]).expect("params");
        assert_eq!(params_bytes, b"{}");
        pos += n4 + part_len;
        let (addr_len, n5) = decode_varint(&frame[pos..]).expect("addr_len");
        pos += n5;
        assert_eq!(&frame[pos..pos + addr_len as usize], b"server");
    }

    #[test]
    fn session_contract_hold_includes_session_route() {
        let frame = hold_for_contract(9, &P2_TREE, Some("sess-1"), None, None);
        assert_eq!(frame[0], 1);
        let text = String::from_utf8_lossy(&frame);
        assert!(text.contains("p2.tree"));
        assert!(text.contains("sess-1"));
        assert!(text.contains("session"));
    }

    #[test]
    fn chat_projection_extracts_user_assistant_tools_and_live() {
        let view = json!({
            "entries": [
                {
                    "id": 1,
                    "data": { "author": { "kind": "owner" }, "attachments": [], "text": "Run tests" },
                    "model": []
                },
                {
                    "id": 2,
                    "data": {},
                    "model": [{
                        "role": "assistant",
                        "content": [
                            { "type": "text", "text": "Running now." },
                            { "type": "toolCall", "id": "tc-1", "name": "bash", "arguments": { "command": "cargo test" } }
                        ]
                    }]
                },
                {
                    "id": 3,
                    "data": {},
                    "model": [{
                        "role": "toolResult",
                        "toolCallId": "tc-1",
                        "toolName": "bash",
                        "isError": false,
                        "durationMs": 420,
                        "content": [{ "type": "text", "text": "ok" }]
                    }]
                }
            ],
            "docs": {
                "pi.live": {
                    "generation": {
                        "message": {
                            "role": "assistant",
                            "content": "Streaming..."
                        }
                    }
                }
            }
        });
        let rows = project_chat_transcript(&view, &ToolActivityTable::new());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].role, ProjectedRole::User);
        assert_eq!(rows[0].text, "Run tests");
        assert_eq!(rows[1].role, ProjectedRole::Assistant);
        assert_eq!(rows[1].text, "Running now.");
        assert_eq!(rows[1].activities.len(), 1);
        assert_eq!(rows[1].activities[0].step.name, "bash");
        assert_eq!(rows[1].activities[0].step.status, StepStatus::Done);
        assert_eq!(rows[1].activities[0].step.duration_ms, Some(420));
        assert_eq!(rows[2].id, "live");
        assert!(rows[2].streaming);
        assert_eq!(rows[2].text, "Streaming...");
    }

    #[test]
    fn decoders_parse_all_plugin_views() {
        let models = decode_models_view(
            Some(&json!([{
                "provider": "anthropic",
                "modelId": "claude-sonnet",
                "name": "Claude Sonnet",
                "contextWindow": 200000,
                "thinkingLevels": ["off", "high"]
            }])),
            Some(
                &json!({ "provider": "anthropic", "modelId": "claude-sonnet", "thinkingLevel": "high" }),
            ),
            Some(&json!(["anthropic/claude-sonnet"])),
            Some(&json!([])),
        );
        assert_eq!(models.models.len(), 1);
        assert_eq!(models.default_model_id.as_deref(), Some("claude-sonnet"));

        let settings = decode_settings_view(Some(&json!({"theme": "dark"})), None);
        assert_eq!(
            settings.entries.get("theme").map(String::as_str),
            Some("dark")
        );

        let mcp = decode_mcp_view(
            Some(
                &json!([{ "id": "m1", "server": "ctx", "source": "owner", "status": "connected", "tools": [{}] }]),
            ),
            None,
            None,
        );
        assert_eq!(mcp.instances.len(), 1);
        assert_eq!(mcp.instances[0].tool_count, 1);

        let sharing = decode_sharing_view(
            Some(&json!("local")),
            Some(&json!([{ "mode": "view", "token": "tok", "url": "https://x" }])),
            Some(&json!([{ "id": "g1", "name": "Guest", "mode": "view" }])),
            None,
        );
        assert_eq!(sharing.links.len(), 1);
        assert_eq!(sharing.guests.len(), 1);

        let costs = decode_costs_view(&json!([{
            "entryId": "e1",
            "cost": 0.012,
            "cacheHit": 0.5,
            "model": "sonnet",
            "provider": "anthropic",
            "stopReason": "stop",
            "timestamp": 1000
        }]));
        assert_eq!(costs.len(), 1);
        assert!((costs[0].cost - 0.012).abs() < f64::EPSILON);

        let artifacts = decode_artifacts_view(Some(&json!([{
            "id": "plan-1",
            "kind": "md",
            "title": "Plan",
            "version": 2
        }])));
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].title, "Plan");

        let tree = decode_tree_view(&json!({
            "current": 2,
            "main": 1,
            "branches": [{ "conversation": 1, "label": "main" }],
            "nodes": [{ "conversation": 1, "entry": 10, "parent": null, "role": "user", "text": "hi" }]
        }));
        assert_eq!(tree.branches.len(), 1);
        assert_eq!(tree.nodes.len(), 1);

        let voice = decode_voice_view(
            Some(&json!(true)),
            Some(&json!({ "engine": "kokoro", "voice": "af_heart", "speed": 1.0 })),
            Some(&json!({ "engines": [{ "id": "kokoro" }] })),
            None,
        );
        assert!(voice.enabled);
        assert_eq!(voice.state, VoiceState::Listening);
        assert_eq!(voice.available_engines, vec!["kokoro"]);
    }

    #[test]
    fn optional_hold_failure_is_isolated_from_core_chat() {
        let mut tracker = HoldTracker::default();
        tracker.register(1, P2_ACCESS.name, true);
        tracker.register(3, P2_MODELS.name, false);
        assert_eq!(
            tracker.mark_failed(3, "unavailable".into()),
            None,
            "optional plugin hold failure must not fail core connection"
        );
        assert_eq!(
            tracker.status_of(P2_MODELS.name),
            Some(&HoldStatus::Unavailable("unavailable".into()))
        );
        assert_eq!(
            tracker.mark_failed(1, "denied".into()),
            Some("denied".into()),
            "required core hold failure must propagate"
        );
    }

    #[test]
    fn reload_triggers_detect_close_frames_and_asset_revision_changes() {
        assert!(is_reload_close_frame(4001, ""));
        assert!(is_reload_close_frame(4002, "plugin reload required"));
        assert!(is_reload_close_frame(1000, "p2: stale page, reload"));
        assert!(!is_reload_close_frame(1000, "normal closure"));
        assert!(!is_reload_close_frame(1006, "abnormal"));

        assert_eq!(
            detect_close_reload(4001, "p2: stale page, reload"),
            ReloadTrigger::CloseFrame {
                code: 4001,
                reason: "p2: stale page, reload".into()
            }
        );
        assert_eq!(
            detect_asset_revision_change("abc", "def"),
            ReloadTrigger::AssetRevisionChanged {
                current: "abc".into(),
                remote: "def".into()
            }
        );
        assert_eq!(
            detect_asset_revision_change("abc", "\"abc\""),
            ReloadTrigger::None
        );
    }

    #[test]
    fn reload_snapshot_and_draft_preserve_text_across_reload() {
        let snap = ReloadSnapshot {
            session_id: Some("sess-1".into()),
            conversation: Some(2),
            panel_id: Some("models".into()),
            draft_key: Some(draft_key_for("sess-1", 2)),
            draft_text: "in progress".into(),
            asset_revision: "rev-1".into(),
            plugin_revision: Some("plug-1".into()),
        };
        let raw = encode_reload_snapshot(&snap);
        assert_eq!(decode_reload_snapshot(&raw), Some(snap));

        let saved_draft = format_draft_storage("sess-1", 2, "see [pic.png](attachment:k1) now");
        assert_eq!(
            restore_draft_for_conversation(Some(&saved_draft), "sess-1#2"),
            Some("see pic.png now".into())
        );
        assert_eq!(
            restore_draft_for_conversation(Some(&saved_draft), "sess-2#1"),
            None
        );
    }

    #[test]
    fn plugin_registry_hot_swap_updates_panels_and_commands() {
        let mut reg = PluginRegistryState::builtin();
        let mut next: Vec<PluginDescriptor> = reg.plugins.values().cloned().collect();
        // Update models title and add a new custom panel while keeping "models" open.
        next[0].title = "Models v2".into();
        next[0].revision = "rev-2".into();
        next.push(PluginDescriptor {
            id: "custom-metrics".into(),
            title: "Metrics".into(),
            revision: "rev-2".into(),
            contract: None,
            panel: Some("custom-metrics".into()),
            command_id: Some("panel-custom-metrics".into()),
            keywords: vec!["perf".into()],
            section: "Plugins".into(),
            enabled: true,
        });
        let report = reg.hot_swap("rev-2".into(), next.clone(), Some("models"));
        assert!(report.revision_changed);
        assert_eq!(report.added, vec!["custom-metrics"]);
        assert_eq!(report.updated, vec!["models"]);
        assert!(report.removed.is_empty());
        assert!(!report.closed_active_panel);
        assert!(
            reg.dynamic_commands()
                .iter()
                .any(|c| c.id == "panel-custom-metrics")
        );

        // Removing the currently open panel flags `closed_active_panel`.
        let without_custom: Vec<PluginDescriptor> = next
            .into_iter()
            .filter(|d| d.id != "custom-metrics")
            .collect();
        let report2 = reg.hot_swap("rev-3".into(), without_custom, Some("custom-metrics"));
        assert_eq!(report2.removed, vec!["custom-metrics"]);
        assert!(report2.closed_active_panel);

        let manifest = json!({
            "revision": "rev-4",
            "plugins": [{
                "id": "models",
                "title": "Models",
                "revision": "rev-4",
                "panel": "models",
                "command_id": "panel-models",
                "keywords": [],
                "section": "Settings",
                "enabled": true
            }]
        });
        let (rev, parsed) = decode_plugin_manifest(&manifest).expect("manifest");
        assert_eq!(rev, "rev-4");
        assert_eq!(parsed.len(), 1);
    }
}
