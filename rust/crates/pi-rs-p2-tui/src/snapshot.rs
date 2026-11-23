use crate::components::transcript::TranscriptAnchor;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiSnapshot {
    pub version: u32,
    pub session: Option<String>,
    pub conversation: u32,
    pub screen: String,
    pub show_thinking: bool,
    pub thinking_setting: bool,
    pub expanded_tools: bool,
    pub drafts: Vec<(String, String)>,
    pub submissions: Vec<(String, Submission)>,
    pub anchors: Vec<(String, TranscriptAnchor)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub text: String,
    pub id: String,
    pub mode: SubmissionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionMode {
    Steer,
    FollowUp,
}

impl Default for TuiSnapshot {
    fn default() -> Self {
        Self {
            version: 2,
            session: None,
            conversation: 0,
            screen: "sessions".to_string(),
            show_thinking: false,
            thinking_setting: false,
            expanded_tools: false,
            drafts: Vec::new(),
            submissions: Vec::new(),
            anchors: Vec::new(),
        }
    }
}

pub fn snapshot_runtime(runtime: &TuiRuntime) -> TuiSnapshot {
    TuiSnapshot {
        version: 2,
        session: runtime.session.clone(),
        conversation: runtime.conversation,
        screen: runtime.screen.clone(),
        show_thinking: runtime.show_thinking,
        thinking_setting: runtime.thinking_setting,
        expanded_tools: runtime.expanded_tools,
        drafts: runtime
            .drafts
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        submissions: runtime
            .submissions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        anchors: runtime
            .anchors
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct TuiRuntime {
    pub session: Option<String>,
    pub conversation: u32,
    pub screen: String,
    pub show_thinking: bool,
    pub thinking_setting: bool,
    pub expanded_tools: bool,
    pub drafts: IndexMap<String, String>,
    pub submissions: IndexMap<String, Submission>,
    pub anchors: IndexMap<String, TranscriptAnchor>,
}

impl TuiRuntime {
    pub fn save_state(&mut self) {
        // State is already in the struct
    }
}

fn migrate_key(key: &str) -> String {
    if key.contains('#') {
        key.to_string()
    } else {
        format!("{}#1", key)
    }
}

pub fn restore_runtime(runtime: &mut TuiRuntime, state: &serde_json::Value) {
    if let Some(obj) = state.as_object() {
        if let Some(session) = obj.get("session").and_then(|v| v.as_str()) {
            runtime.session = Some(session.to_string());
        }
        if let Some(conv) = obj.get("conversation").and_then(|v| v.as_u64()) {
            runtime.conversation = conv as u32;
        }
        if let Some(screen) = obj.get("screen").and_then(|v| v.as_str()) {
            if screen == "sessions" || screen == "chat" {
                runtime.screen = screen.to_string();
            } else if screen == "models" {
                runtime.screen = if runtime.session.is_some() {
                    "chat".to_string()
                } else {
                    "sessions".to_string()
                };
            }
        }
        if let Some(show_thinking) = obj.get("showThinking").and_then(|v| v.as_bool()) {
            runtime.show_thinking = show_thinking;
        }
        if let Some(thinking_setting) = obj.get("thinkingSetting").and_then(|v| v.as_bool()) {
            runtime.thinking_setting = thinking_setting;
        }
        if let Some(expanded) = obj.get("expandedTools").and_then(|v| v.as_bool()) {
            runtime.expanded_tools = expanded;
        }
        if let Some(drafts) = obj.get("drafts").and_then(|v| v.as_array()) {
            for pair in drafts {
                if let Some(arr) = pair.as_array()
                    && arr.len() == 2
                    && let (Some(k), Some(v)) = (arr[0].as_str(), arr[1].as_str())
                {
                    runtime.drafts.insert(migrate_key(k), v.to_string());
                }
            }
        }
        if let Some(submissions) = obj.get("submissions").and_then(|v| v.as_array()) {
            for pair in submissions {
                if let Some(arr) = pair.as_array()
                    && arr.len() == 2
                    && let (Some(k), Some(v)) = (arr[0].as_str(), arr[1].as_object())
                    && let (Some(text), Some(id), Some(mode)) = (
                        v.get("text").and_then(|v| v.as_str()),
                        v.get("id").and_then(|v| v.as_str()),
                        v.get("mode").and_then(|v| v.as_str()),
                    )
                {
                    let mode = match mode {
                        "steer" => SubmissionMode::Steer,
                        "followUp" => SubmissionMode::FollowUp,
                        _ => continue,
                    };
                    runtime.submissions.insert(
                        migrate_key(k),
                        Submission {
                            text: text.to_string(),
                            id: id.to_string(),
                            mode,
                        },
                    );
                }
            }
        }
        if let Some(anchors) = obj.get("anchors").and_then(|v| v.as_array()) {
            for pair in anchors {
                if let Some(arr) = pair.as_array()
                    && arr.len() == 2
                    && let (Some(k), Some(v)) = (arr[0].as_str(), arr[1].as_object())
                    && let (Some(key), Some(offset)) = (
                        v.get("key").and_then(|v| v.as_str()),
                        v.get("offset").and_then(|v| v.as_i64()),
                    )
                {
                    let entry_id = v.get("entryId").and_then(|v| v.as_u64());
                    runtime.anchors.insert(
                        k.to_string(),
                        TranscriptAnchor {
                            key: key.to_string(),
                            offset,
                            entry_id,
                        },
                    );
                }
            }
        }
    }
}
