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
use crate::state::composer_state::{PickedFile, pick_files, referenced_files};
use crate::ui::command_menu::CommandItem;
use crate::ui::terminal::{TerminalStatus, VoiceState};
use indexmap::IndexMap;
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

/// Encodes a Chord `Call` frame (kind = 3).
pub fn encode_call_frame(call_id: u64, hold_id: u64, method: u64, args: &Value) -> Vec<u8> {
    let mut buf = vec![3];
    for field in [call_id, hold_id, method, 1] {
        encode_varint(field, &mut buf);
    }
    buf.extend(encode_part(args.to_string().as_bytes(), false));
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
        "p2.tree" => {
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

/// Composer attachment manager backed by `crate::state::composer_state`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposerAttachments {
    pub files: Vec<PickedFile>,
    pub last_too_large: Option<String>,
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
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.last_too_large = None;
    }

    pub fn referenced_for_draft(&self, draft: &str) -> Vec<PickedFile> {
        referenced_files(draft, &self.files)
    }
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
}
