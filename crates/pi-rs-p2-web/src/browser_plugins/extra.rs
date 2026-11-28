//! Extended built-in plugin contracts, UTF-16 member indexes, and state decoders
//! for Wave 1.6c (`login`, `devices`, `discord`, `envs`, `ports`, `logs`, `telemetry`,
//! `workspace`, `views`, `scope-prompt`, `subagents`, `chat-mode`, `familiar`, `prompt`,
//! `history`, `sessions`, `projects`, `search`, `file-index`, `session-metrics`).

use super::{ContractAddressKind, ContractSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const P2_PROVIDER_LOGIN: ContractSpec = ContractSpec {
    name: "p2.provider-login",
    version: 1,
    fingerprint: 0x0fbfa1c37104f4c0,
    address_kind: ContractAddressKind::Server,
};

pub const P2_DEVICES: ContractSpec = ContractSpec {
    name: "p2.devices",
    version: 1,
    fingerprint: 0x67513be9d3d68404,
    address_kind: ContractAddressKind::Server,
};

pub const P2_DISCORD: ContractSpec = ContractSpec {
    name: "p2.discord",
    version: 3,
    fingerprint: 0x0e433f69c3d62b82,
    address_kind: ContractAddressKind::Server,
};

pub const P2_ENVS: ContractSpec = ContractSpec {
    name: "p2.envs",
    version: 3,
    fingerprint: 0x1f435a0a4cef241d,
    address_kind: ContractAddressKind::Server,
};

pub const P2_PORTS: ContractSpec = ContractSpec {
    name: "p2.ports",
    version: 2,
    fingerprint: 0x658d83155e75806e,
    address_kind: ContractAddressKind::Server,
};

pub const P2_LOGS: ContractSpec = ContractSpec {
    name: "p2.logs",
    version: 1,
    fingerprint: 0xe09c6d49e669d734,
    address_kind: ContractAddressKind::Server,
};

pub const P2_LOG_NOTICES: ContractSpec = ContractSpec {
    name: "p2.log-notices",
    version: 1,
    fingerprint: 0xec03a2acb7b4e983,
    address_kind: ContractAddressKind::Server,
};

pub const P2_TRACES: ContractSpec = ContractSpec {
    name: "p2.traces",
    version: 1,
    fingerprint: 0x4d82a836ce81dc4b,
    address_kind: ContractAddressKind::Server,
};

pub const P2_WORKSPACE: ContractSpec = ContractSpec {
    name: "p2.workspace",
    version: 1,
    fingerprint: 0x7b0b2c2e4b1e097f,
    address_kind: ContractAddressKind::Session,
};

pub const P2_VIEWS: ContractSpec = ContractSpec {
    name: "p2.views",
    version: 4,
    fingerprint: 0x56b1f203f0303398,
    address_kind: ContractAddressKind::Session,
};

pub const P2_SCOPE_STAGE: ContractSpec = ContractSpec {
    name: "p2.scope-stage",
    version: 1,
    fingerprint: 0xd3712070c8b0791f,
    address_kind: ContractAddressKind::Session,
};

pub const P2_SUBAGENTS: ContractSpec = ContractSpec {
    name: "p2.subagents",
    version: 3,
    fingerprint: 0x27892dad9b032dc4,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SUBAGENT_CONTROL: ContractSpec = ContractSpec {
    name: "p2.subagent-control",
    version: 1,
    fingerprint: 0x3cfeee6e948fb4a1,
    address_kind: ContractAddressKind::Session,
};

pub const P2_CHAT_MODE: ContractSpec = ContractSpec {
    name: "p2.chat-mode",
    version: 2,
    fingerprint: 0xbc2026a74dd934c6,
    address_kind: ContractAddressKind::Session,
};

pub const P2_FAMILIAR: ContractSpec = ContractSpec {
    name: "p2.familiar",
    version: 1,
    fingerprint: 0x57a1e9d72a02da7e,
    address_kind: ContractAddressKind::Server,
};

pub const P2_PROMPT_RESOURCES: ContractSpec = ContractSpec {
    name: "p2.prompt-resources",
    version: 2,
    fingerprint: 0xd040ba46c25830af,
    address_kind: ContractAddressKind::Session,
};

pub const P2_PROJECTS: ContractSpec = ContractSpec {
    name: "p2.projects",
    version: 3,
    fingerprint: 0x9ef1f334cf214683,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SIDEBAR_ROLLUPS: ContractSpec = ContractSpec {
    name: "p2.sidebar-rollups",
    version: 1,
    fingerprint: 0x5df92e770748f83e,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SIDEBAR_SETTINGS: ContractSpec = ContractSpec {
    name: "p2.sidebar-settings",
    version: 1,
    fingerprint: 0x2d89a937178c64ac,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SEARCH: ContractSpec = ContractSpec {
    name: "p2.search",
    version: 3,
    fingerprint: 0x9fc4d520e35f5aa1,
    address_kind: ContractAddressKind::Server,
};

pub const P2_FILE_INDEX: ContractSpec = ContractSpec {
    name: "p2.file-index",
    version: 1,
    fingerprint: 0xa86e894e1bb58c17,
    address_kind: ContractAddressKind::Session,
};

pub const P2_SESSION_METRICS: ContractSpec = ContractSpec {
    name: "p2.session-metrics",
    version: 1,
    fingerprint: 0xbbd1027258098ec1,
    address_kind: ContractAddressKind::Server,
};

pub const P2_SENT_FILES: ContractSpec = ContractSpec {
    name: "p2.sent-files",
    version: 1,
    fingerprint: 0x3cf6e69cd2bc59c9,
    address_kind: ContractAddressKind::Session,
};

/// Exact UTF-16 sorted state & method indexes verified against `p2_contracts/*.json`.
pub mod extra_indexes {
    pub mod provider_login {
        pub const STATE_LOGIN: u64 = 0;
        pub const STATE_PROVIDERS: u64 = 1;
        pub const METHOD_ANSWER: u64 = 0;
        pub const METHOD_CANCEL: u64 = 1;
        pub const METHOD_DISMISS: u64 = 2;
        pub const METHOD_LOGOUT: u64 = 3;
        pub const METHOD_REFRESH: u64 = 4;
        pub const METHOD_START: u64 = 5;
    }

    pub mod devices {
        pub const STATE_CAN_MINT: u64 = 0;
        pub const METHOD_TOKEN: u64 = 0;
    }

    pub mod discord {
        pub const STATE_BINDINGS: u64 = 0;
        pub const STATE_BOTS: u64 = 1;
        pub const METHOD_ADD_BOT: u64 = 0;
        pub const METHOD_LINK: u64 = 1;
        pub const METHOD_MCP_SERVERS: u64 = 2;
        pub const METHOD_REMOVE_BOT: u64 = 3;
        pub const METHOD_SERVER: u64 = 4;
        pub const METHOD_SET_BOT_TOKEN: u64 = 5;
        pub const METHOD_UNLINK: u64 = 6;
        pub const METHOD_UPDATE_LINK: u64 = 7;
    }

    pub mod envs {
        pub const STATE_ENVS: u64 = 0;
        pub const STATE_FAILURE: u64 = 1;
        pub const METHOD_ACCEPT_HOST_KEY: u64 = 0;
        pub const METHOD_DELETE: u64 = 1;
        pub const METHOD_FORGET_HOST_KEY: u64 = 2;
        pub const METHOD_GENERATE_KEY: u64 = 3;
        pub const METHOD_LIST: u64 = 4;
        pub const METHOD_LIST_DIRECTORY: u64 = 5;
        pub const METHOD_PUBLIC_KEY: u64 = 6;
        pub const METHOD_SANDBOX_RESET_DISK: u64 = 7;
        pub const METHOD_SANDBOX_START: u64 = 8;
        pub const METHOD_SANDBOX_STATUS: u64 = 9;
        pub const METHOD_SANDBOX_STOP: u64 = 10;
        pub const METHOD_SAVE: u64 = 11;
        pub const METHOD_TEST: u64 = 12;
        pub const METHOD_UPLOAD_KEY: u64 = 13;
        pub const METHOD_WARM: u64 = 14;
    }

    pub mod ports {
        pub const STATE_FORWARDS: u64 = 0;
        pub const STATE_RADIUS_ORIGIN: u64 = 1;
        pub const METHOD_ADD: u64 = 0;
        pub const METHOD_REMOVE: u64 = 1;
        pub const METHOD_SET_PUBLIC: u64 = 2;
    }

    pub mod logs {
        pub const STATE_LINES: u64 = 0;
        pub const STATE_SOURCES: u64 = 1;
    }

    pub mod log_notices {
        pub const STATE_NOTICES: u64 = 0;
        pub const METHOD_DISMISS: u64 = 0;
    }

    pub mod traces {
        pub const METHOD_FIND_METRICS: u64 = 0;
        pub const METHOD_FIND_TRACES: u64 = 1;
        pub const METHOD_GET_TRACE: u64 = 2;
    }

    pub mod workspace {
        pub const STATE_CHANGES: u64 = 0;
        pub const METHOD_QUERY: u64 = 0;
    }

    pub mod views {
        pub const STATE_ANNOTATIONS: u64 = 0;
        pub const STATE_ARRANGEMENT: u64 = 1;
        pub const STATE_INLINE: u64 = 2;
        pub const STATE_VIEWS: u64 = 3;
        pub const METHOD_ANNOTATE: u64 = 0;
        pub const METHOD_CLOSE: u64 = 1;
        pub const METHOD_DISCARD: u64 = 2;
        pub const METHOD_OPEN: u64 = 3;
        pub const METHOD_SELECT: u64 = 4;
        pub const METHOD_SELECT_INLINE: u64 = 5;
        pub const METHOD_UNANNOTATE: u64 = 6;
    }

    pub mod scope_stage {
        pub const METHOD_STAGE: u64 = 0;
    }

    pub mod subagents {
        pub const STATE_CHILDREN: u64 = 0;
    }

    pub mod subagent_control {
        pub const METHOD_ARCHIVE: u64 = 0;
    }

    pub mod chat_mode {
        pub const STATE_ENABLED: u64 = 0;
        pub const STATE_NAME: u64 = 1;
        pub const STATE_WAITING: u64 = 2;
        pub const METHOD_RENAME: u64 = 0;
        pub const METHOD_SET: u64 = 1;
    }

    pub mod familiar {
        pub const STATE_SESSION: u64 = 0;
        pub const METHOD_OPEN: u64 = 0;
    }

    pub mod prompt_resources {
        pub const STATE_SKILLS: u64 = 0;
        pub const STATE_TEMPLATES: u64 = 1;
        pub const METHOD_EXPAND: u64 = 0;
    }

    pub mod projects {
        pub const STATE_ARCHIVED: u64 = 0;
        pub const STATE_LOCAL: u64 = 1;
        pub const STATE_PROJECTS: u64 = 2;
        pub const STATE_SESSIONS: u64 = 3;
        pub const METHOD_ARCHIVE: u64 = 0;
        pub const METHOD_ASSIGN: u64 = 1;
        pub const METHOD_CREATE: u64 = 2;
        pub const METHOD_LOCATE: u64 = 3;
        pub const METHOD_MOVE: u64 = 4;
        pub const METHOD_REMOVE: u64 = 5;
        pub const METHOD_RENAME: u64 = 6;
        pub const METHOD_RESTORE: u64 = 7;
    }

    pub mod sidebar_rollups {
        pub const STATE_ROLLUPS: u64 = 0;
        pub const METHOD_PREVIEW: u64 = 0;
        pub const METHOD_REFRESH: u64 = 1;
        pub const METHOD_REMOVE: u64 = 2;
        pub const METHOD_SAVE: u64 = 3;
    }

    pub mod sidebar_settings {
        pub const STATE_COLLAPSED: u64 = 0;
        pub const STATE_ORDER: u64 = 1;
        pub const STATE_WINDOWS: u64 = 2;
        pub const METHOD_SET_COLLAPSED: u64 = 0;
        pub const METHOD_SET_ORDER: u64 = 1;
        pub const METHOD_SET_WINDOW: u64 = 2;
    }

    pub mod search {
        pub const STATE_REBUILDS: u64 = 0;
        pub const METHOD_REBUILD_INDEX: u64 = 0;
        pub const METHOD_SEARCH: u64 = 1;
    }

    pub mod file_index {
        pub const STATE_CHANGES: u64 = 0;
        pub const METHOD_COMPLETE: u64 = 0;
        pub const METHOD_LIST: u64 = 1;
    }

    pub mod session_metrics {
        pub const STATE_CLIENTS: u64 = 0;
        pub const STATE_USAGE: u64 = 1;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginMethodView {
    pub auth_type: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub signed_in: Option<String>,
    pub methods: Vec<LoginMethodView>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveLoginAttempt {
    pub attempt_id: String,
    pub provider_id: String,
    pub auth_type: String,
    pub status: String,
    pub message: String,
    pub prompt_id: Option<String>,
    pub prompt_label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginView {
    pub providers: Vec<ProviderView>,
    pub active: Option<ActiveLoginAttempt>,
}

pub fn decode_login_view(providers_val: Option<&Value>, login_val: Option<&Value>) -> LoginView {
    let providers = providers_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let methods = p["loginMethods"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    Some(LoginMethodView {
                        auth_type: m["type"].as_str()?.to_owned(),
                        label: m["label"].as_str().unwrap_or("").to_owned(),
                    })
                })
                .collect();
            Some(ProviderView {
                id: p["id"].as_str()?.to_owned(),
                name: p["name"].as_str().unwrap_or("").to_owned(),
                signed_in: p["signedIn"].as_str().map(str::to_owned),
                methods,
            })
        })
        .collect();
    let active = login_val.filter(|v| v.is_object()).and_then(|l| {
        Some(ActiveLoginAttempt {
            attempt_id: l["attemptId"].as_str()?.to_owned(),
            provider_id: l["providerId"].as_str()?.to_owned(),
            auth_type: l["authType"].as_str().unwrap_or("oauth").to_owned(),
            status: l["status"].as_str().unwrap_or("working").to_owned(),
            message: l["message"].as_str().unwrap_or("").to_owned(),
            prompt_id: l["prompt"]["id"].as_str().map(str::to_owned),
            prompt_label: l["prompt"]["message"].as_str().map(str::to_owned),
        })
    });
    LoginView { providers, active }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevicesView {
    pub can_mint: bool,
    pub minted_token: Option<String>,
    pub minted_url: Option<String>,
}

pub fn decode_devices_view(can_mint_val: Option<&Value>) -> DevicesView {
    DevicesView {
        can_mint: can_mint_val.and_then(Value::as_bool).unwrap_or(false),
        minted_token: None,
        minted_url: None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscordBotView {
    pub id: String,
    pub label: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscordBindingView {
    pub session: String,
    pub bot: String,
    pub guild: String,
    pub channel: String,
    pub wake: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscordView {
    pub bots: Vec<DiscordBotView>,
    pub bindings: Vec<DiscordBindingView>,
}

pub fn decode_discord_view(bots_val: Option<&Value>, bindings_val: Option<&Value>) -> DiscordView {
    let bots = bots_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(DiscordBotView {
                id: b["id"].as_str().or_else(|| b["name"].as_str())?.to_owned(),
                label: b["name"]
                    .as_str()
                    .or_else(|| b["username"].as_str())
                    .unwrap_or("bot")
                    .to_owned(),
                status: b["status"].as_str().unwrap_or("ready").to_owned(),
            })
        })
        .collect();
    let bindings = bindings_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(DiscordBindingView {
                session: b["session"].as_str()?.to_owned(),
                bot: b["bot"].as_str().unwrap_or("").to_owned(),
                guild: b["guild"].as_str().unwrap_or("").to_owned(),
                channel: b["channel"].as_str().unwrap_or("").to_owned(),
                wake: b["wake"].as_str().unwrap_or("mentions").to_owned(),
            })
        })
        .collect();
    DiscordView { bots, bindings }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvSummaryView {
    pub name: String,
    pub kind: String,
    pub host: Option<String>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvsView {
    pub envs: Vec<EnvSummaryView>,
    pub failure: Option<String>,
    pub last_result: Option<String>,
}

pub fn decode_envs_view(envs_val: Option<&Value>, failure_val: Option<&Value>) -> EnvsView {
    let envs = envs_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(EnvSummaryView {
                name: e["name"].as_str()?.to_owned(),
                kind: e["kind"]
                    .as_str()
                    .or_else(|| e["type"].as_str())
                    .unwrap_or("ssh")
                    .to_owned(),
                host: e["host"].as_str().map(str::to_owned),
                status: e["status"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let failure = failure_val.and_then(Value::as_str).map(str::to_owned);
    EnvsView {
        envs,
        failure,
        last_result: None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortForwardView {
    pub id: String,
    pub port: u16,
    pub name: String,
    pub public: bool,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortsView {
    pub forwards: Vec<PortForwardView>,
    pub radius_origin: Option<String>,
}

pub fn decode_ports_view(forwards_val: Option<&Value>, origin_val: Option<&Value>) -> PortsView {
    let forwards = forwards_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(PortForwardView {
                id: f["id"].as_str()?.to_owned(),
                port: f["port"].as_u64()? as u16,
                name: f["name"].as_str().unwrap_or("").to_owned(),
                public: f["public"].as_bool().unwrap_or(false),
                url: f["url"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let radius_origin = origin_val.and_then(Value::as_str).map(str::to_owned);
    PortsView {
        forwards,
        radius_origin,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogLineView {
    pub time: u64,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogsView {
    pub source: String,
    pub sources: Vec<String>,
    pub lines: Vec<LogLineView>,
}

pub fn decode_logs_view(
    source: &str,
    sources_val: Option<&Value>,
    lines_val: Option<&Value>,
) -> LogsView {
    let sources = sources_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    let lines = lines_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| {
            if let Some(text) = l.as_str() {
                return Some(LogLineView {
                    time: 0,
                    level: "info".to_owned(),
                    message: text.to_owned(),
                });
            }
            Some(LogLineView {
                time: l["time"].as_u64().unwrap_or(0),
                level: l["level"].as_str().unwrap_or("info").to_owned(),
                message: l["message"]
                    .as_str()
                    .or_else(|| l["line"].as_str())?
                    .to_owned(),
            })
        })
        .collect();
    LogsView {
        source: source.to_owned(),
        sources,
        lines,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogNoticeView {
    pub id: String,
    pub time: u64,
    pub source: String,
    pub message: String,
}

pub fn decode_log_notices_view(notices_val: Option<&Value>) -> Vec<LogNoticeView> {
    notices_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let id = n["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| n["id"].as_u64().map(|v| v.to_string()))?;
            Some(LogNoticeView {
                id,
                time: n["time"].as_u64().unwrap_or(0),
                source: n["source"].as_str().unwrap_or("server").to_owned(),
                message: n["message"].as_str()?.to_owned(),
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceSummaryView {
    pub trace_id: String,
    pub name: String,
    pub duration_ms: u64,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TracesView {
    pub traces: Vec<TraceSummaryView>,
    pub selected_trace: Option<String>,
}

pub fn decode_traces_result(val: &Value) -> Vec<TraceSummaryView> {
    val["traces"]
        .as_array()
        .or_else(|| val.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(TraceSummaryView {
                trace_id: t["traceId"].as_str()?.to_owned(),
                name: t["name"].as_str().unwrap_or("span").to_owned(),
                duration_ms: t["durationMs"].as_u64().unwrap_or(0),
                status: t["status"].as_str().unwrap_or("ok").to_owned(),
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceChangeView {
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceView {
    pub changes: Vec<WorkspaceChangeView>,
    pub files: Vec<(String, bool)>,
    pub selected_path: Option<String>,
    pub selected_content: Option<String>,
    pub selected_before: Option<String>,
}

pub fn decode_workspace_changes(val: Option<&Value>) -> Vec<WorkspaceChangeView> {
    val.and_then(|v| v["changes"].as_array().or_else(|| v.as_array()))
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(WorkspaceChangeView {
                path: c["path"].as_str()?.to_owned(),
                status: c["status"].as_str().unwrap_or("modified").to_owned(),
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewItemDescriptor {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ArrangementNode {
    Leaf {
        tabs: Vec<String>,
        active: Option<String>,
    },
    Split {
        direction: String,
        ratio: u8,
        first: Box<ArrangementNode>,
        second: Box<ArrangementNode>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewsLayoutView {
    pub views: Vec<ViewItemDescriptor>,
    pub arrangement: Option<ArrangementNode>,
    pub inline_id: Option<String>,
    pub annotations: Vec<(String, String)>,
}

pub fn decode_views_state(
    views_val: Option<&Value>,
    arrangement_val: Option<&Value>,
    inline_val: Option<&Value>,
    annotations_val: Option<&Value>,
) -> ViewsLayoutView {
    let views = views_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some(ViewItemDescriptor {
                id: v["id"].as_str()?.to_owned(),
                kind: v["kind"].as_str().unwrap_or("panel").to_owned(),
                title: v["title"].as_str().unwrap_or("").to_owned(),
                target: v["target"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let arrangement = arrangement_val.and_then(|v| serde_json::from_value(v.clone()).ok());
    let inline_id = inline_val.and_then(Value::as_str).map(str::to_owned);
    let annotations = annotations_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                a["id"].as_str()?.to_owned(),
                a["text"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect();
    ViewsLayoutView {
        views,
        arrangement,
        inline_id,
        annotations,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubagentChildView {
    pub session_id: String,
    pub conversation_id: u64,
    pub title: String,
    pub parent_session: String,
    pub status: String,
    pub one_shot: bool,
    pub archived: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubagentsView {
    pub children: Vec<SubagentChildView>,
}

pub fn decode_subagents_view(children_val: Option<&Value>) -> SubagentsView {
    let mut children = Vec::new();
    if let Some(obj) = children_val.and_then(Value::as_object) {
        for (parent, list) in obj {
            for item in list.as_array().into_iter().flatten() {
                if let Some(sid) = item["sessionId"].as_str().or_else(|| item["id"].as_str()) {
                    children.push(SubagentChildView {
                        session_id: sid.to_owned(),
                        conversation_id: item["conversationId"].as_u64().unwrap_or(1),
                        title: item["title"].as_str().unwrap_or(sid).to_owned(),
                        parent_session: parent.clone(),
                        status: item["status"].as_str().unwrap_or("active").to_owned(),
                        one_shot: item["oneShot"].as_bool().unwrap_or(false),
                        archived: item["archived"]
                            .as_bool()
                            .unwrap_or(!item["archivedAt"].is_null()),
                    });
                }
            }
        }
    }
    SubagentsView { children }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatModeView {
    pub enabled: bool,
    pub name: String,
    pub waiting: u64,
}

pub fn decode_chat_mode_view(
    enabled_val: Option<&Value>,
    name_val: Option<&Value>,
    waiting_val: Option<&Value>,
) -> ChatModeView {
    ChatModeView {
        enabled: enabled_val.and_then(Value::as_bool).unwrap_or(false),
        name: name_val.and_then(Value::as_str).unwrap_or("Pi").to_owned(),
        waiting: waiting_val.and_then(Value::as_u64).unwrap_or(0),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FamiliarView {
    pub session_id: Option<String>,
}

pub fn decode_familiar_view(session_val: Option<&Value>) -> FamiliarView {
    FamiliarView {
        session_id: session_val
            .and_then(|v| v.as_str().or_else(|| v["id"].as_str()))
            .map(str::to_owned),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptResourceItem {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptResourcesView {
    pub skills: Vec<PromptResourceItem>,
    pub templates: Vec<PromptResourceItem>,
}

pub fn decode_prompt_resources_view(
    skills_val: Option<&Value>,
    templates_val: Option<&Value>,
) -> PromptResourcesView {
    let parse_items = |val: Option<&Value>| -> Vec<PromptResourceItem> {
        val.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(PromptResourceItem {
                    name: item["name"].as_str()?.to_owned(),
                    description: item["description"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect()
    };
    PromptResourcesView {
        skills: parse_items(skills_val),
        templates: parse_items(templates_val),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    pub archived: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectsView {
    pub projects: Vec<ProjectView>,
    pub assignments: Vec<(String, String)>,
}

pub fn decode_projects_view(
    projects_val: Option<&Value>,
    sessions_val: Option<&Value>,
) -> ProjectsView {
    let projects = projects_val
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(ProjectView {
                id: p["id"].as_str()?.to_owned(),
                name: p["name"].as_str().unwrap_or("").to_owned(),
                path: p["path"].as_str().map(str::to_owned),
                archived: !p["archivedAt"].is_null(),
            })
        })
        .collect();
    let mut assignments = Vec::new();
    if let Some(obj) = sessions_val.and_then(Value::as_object) {
        for (sid, pid) in obj {
            if let Some(project) = pid.as_str() {
                assignments.push((sid.clone(), project.to_owned()));
            }
        }
    }
    ProjectsView {
        projects,
        assignments,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RollupView {
    pub key: String,
    pub name: String,
    pub session_ids: Vec<String>,
}

pub fn decode_rollups_view(val: Option<&Value>) -> Vec<RollupView> {
    val.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(RollupView {
                key: r["key"].as_str()?.to_owned(),
                name: r["name"].as_str().unwrap_or("").to_owned(),
                session_ids: r["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect(),
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionMetricsView {
    pub clients: usize,
    pub total_cost: f64,
    pub context_used: Option<u64>,
    pub context_limit: Option<u64>,
    pub cache_hit_rate: Option<f64>,
}

pub fn decode_session_metrics_view(
    clients_val: Option<&Value>,
    usage_val: Option<&Value>,
) -> SessionMetricsView {
    let clients = clients_val
        .and_then(|v| {
            v.as_u64()
                .map(|n| n as usize)
                .or_else(|| v.as_array().map(Vec::len))
        })
        .unwrap_or(0);
    let total_cost = usage_val
        .and_then(|u| u["cost"]["total"].as_f64().or_else(|| u["cost"].as_f64()))
        .unwrap_or(0.0);
    let context_used = usage_val.and_then(|u| u["contextUsed"].as_u64());
    let context_limit = usage_val.and_then(|u| u["contextLimit"].as_u64());
    let cache_hit_rate = usage_val.and_then(|u| {
        let input = u["input"].as_f64().unwrap_or(0.0);
        let read = u["cacheRead"].as_f64().unwrap_or(0.0);
        let write = u["cacheWrite"].as_f64().unwrap_or(0.0);
        let denom = input + read + write;
        (denom > 0.0).then_some(read / denom)
    });
    SessionMetricsView {
        clients,
        total_cost,
        context_used,
        context_limit,
        cache_hit_rate,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchHitView {
    pub session_id: String,
    pub title: String,
    pub snippet: String,
}

pub fn decode_search_results(val: &Value) -> Vec<SearchHitView> {
    val["results"]
        .as_array()
        .or_else(|| val.as_array())
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(SearchHitView {
                session_id: h["sessionId"]
                    .as_str()
                    .or_else(|| h["session"].as_str())?
                    .to_owned(),
                title: h["title"].as_str().unwrap_or("").to_owned(),
                snippet: h["snippet"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decoders_parse_wave_1_6c_plugin_states() {
        let login = decode_login_view(
            Some(
                &json!([{"id":"anthropic","name":"Anthropic","signedIn":"oauth","loginMethods":[{"type":"oauth","label":"Sign in"}]}]),
            ),
            None,
        );
        assert_eq!(login.providers[0].signed_in.as_deref(), Some("oauth"));

        let ports = decode_ports_view(
            Some(&json!([{"id":"f1","port":3000,"name":"web","public":true,"url":"https://x"}])),
            Some(&json!("https://radius.example")),
        );
        assert_eq!(ports.forwards[0].port, 3000);
        assert!(ports.forwards[0].public);

        let sub = decode_subagents_view(Some(&json!({
            "parent-1": [{"sessionId":"child-1","title":"Worker","status":"running"}]
        })));
        assert_eq!(sub.children.len(), 1);
        assert_eq!(sub.children[0].session_id, "child-1");

        let mode =
            decode_chat_mode_view(Some(&json!(true)), Some(&json!("Clotho")), Some(&json!(2)));
        assert!(mode.enabled);
        assert_eq!(mode.name, "Clotho");
        assert_eq!(mode.waiting, 2);

        let metrics = decode_session_metrics_view(
            Some(&json!(3)),
            Some(
                &json!({"cost":{"total":1.25},"input":100,"cacheRead":300,"cacheWrite":0,"contextUsed":5000,"contextLimit":200000}),
            ),
        );
        assert_eq!(metrics.clients, 3);
        assert!((metrics.total_cost - 1.25).abs() < f64::EPSILON);
        assert_eq!(metrics.cache_hit_rate, Some(0.75));
    }
}
