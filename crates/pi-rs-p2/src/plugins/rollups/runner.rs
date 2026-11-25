//! Running a rollup's script in `CodemodeSandbox`, tracking reads, and fingerprinting inputs.

use std::collections::HashMap;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use indexmap::IndexMap;
use pi_rs_chord::data::Validator;
use pi_rs_codemode::{CodemodeSandbox, ExecuteOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const RUN_TIMEOUT_MS: u64 = 5_000;
pub const RUN_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const MENTIONS_GLOBAL: &str = "__p2Mentions";
pub const MENTIONS_READ: &str = "mentions:";

const INPUT_KEY: &str = "p2.rollup-input";
const RUN_KEY: &str = "p2.rollup-run";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupSession {
    pub id: String,
    pub title: String,
    pub env: Option<String>,
    pub cwd: String,
    pub created_at: f64,
    pub updated_at: f64,
    pub archived_at: Option<f64>,
    pub pinned: bool,
    pub model: String,
    pub project: Option<String>,
    pub state: String,
    pub error: Option<String>,
    pub unread: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RollupFacts {
    pub sessions: Vec<Value>,
    pub statuses: Value,
    pub answers: Value,
    pub reads: Value,
    pub projects: Vec<Value>,
    pub memberships: Value,
    pub familiar: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupInput {
    pub sessions: Vec<RollupSession>,
    pub projects: Vec<Value>,
    pub now: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionCall {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum PreviewResult {
    #[serde(rename = "true")]
    Ok { output: Value, reads: Vec<String> },
    #[serde(rename = "false")]
    Err { error: String },
}

pub fn preview_to_json(preview: &PreviewResult) -> Value {
    match preview {
        PreviewResult::Ok { output, reads } => json!({
            "ok": true,
            "output": output,
            "reads": reads,
        }),
        PreviewResult::Err { error } => json!({
            "ok": false,
            "error": error,
        }),
    }
}

pub fn input_of(facts: &RollupFacts, now: u64) -> RollupInput {
    let mut sessions: Vec<RollupSession> = facts
        .sessions
        .iter()
        .filter(|session| {
            let id = session.get("id").and_then(Value::as_str);
            id.is_some() && id != facts.familiar.as_deref()
        })
        .map(|session| {
            let id = session["id"].as_str().unwrap_or_default().to_owned();
            let status = facts.statuses.get(&id);
            let running = status
                .and_then(|s| s.get("running"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let busy = status
                .and_then(|s| s.get("busy"))
                .and_then(Value::as_array)
                .is_some_and(|arr| !arr.is_empty());
            let state = if !running {
                "stopped"
            } else if busy {
                "working"
            } else {
                "idle"
            }
            .to_owned();
            let error = status
                .and_then(|s| s.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let answer_seq = facts
                .answers
                .get(&id)
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let read_seq = facts.reads.get(&id).and_then(Value::as_f64).unwrap_or(0.0);
            let provider = session["model"]["provider"].as_str().unwrap_or_default();
            let model_id = session["model"]["modelId"].as_str().unwrap_or_default();
            RollupSession {
                id: id.clone(),
                title: session["title"].as_str().unwrap_or_default().to_owned(),
                env: session
                    .get("env")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                cwd: session["cwd"].as_str().unwrap_or_default().to_owned(),
                created_at: session["createdAt"].as_f64().unwrap_or(0.0),
                updated_at: session["updatedAt"].as_f64().unwrap_or(0.0),
                archived_at: session.get("archivedAt").and_then(Value::as_f64),
                pinned: session
                    .get("pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                model: format!("{provider}/{model_id}"),
                project: facts
                    .memberships
                    .get(&id)
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                state,
                error,
                unread: answer_seq > read_seq,
            }
        })
        .collect();
    sessions.sort_by(|a, b| {
        a.created_at
            .total_cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    RollupInput {
        sessions,
        projects: facts.projects.clone(),
        now,
    }
}

pub fn fingerprint_of(
    input: &RollupInput,
    reads: &[String],
    mentions: &HashMap<String, Value>,
) -> String {
    let all = reads.iter().any(|r| r == "sessions.*");
    let fields: Vec<&str> = reads
        .iter()
        .filter_map(|path| path.strip_prefix("sessions.").filter(|f| *f != "*"))
        .collect();
    let sessions: Vec<Value> = input
        .sessions
        .iter()
        .map(|session| {
            let val = serde_json::to_value(session).unwrap_or(Value::Null);
            if all {
                return val;
            }
            let mut row = Vec::with_capacity(1 + fields.len());
            row.push(json!(session.id));
            for field in &fields {
                row.push(val.get(*field).cloned().unwrap_or(Value::Null));
            }
            Value::Array(row)
        })
        .collect();
    let projects_fact = if reads.iter().any(|r| r == "projects") {
        json!(input.projects)
    } else {
        Value::Null
    };
    let now_fact = if reads.iter().any(|r| r == "now") {
        json!(input.now)
    } else {
        Value::Null
    };
    let mention_facts: Vec<Value> = reads
        .iter()
        .filter(|path| path.starts_with(MENTIONS_READ))
        .map(|path| {
            let answer = mentions.get(path).cloned().unwrap_or(Value::Null);
            json!([path, answer])
        })
        .collect();
    let facts = json!([sessions, projects_fact, now_fact, mention_facts]);
    let bytes = serde_json::to_vec(&facts).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn mention_read(query: &str, since: Option<u64>) -> String {
    let payload = match since {
        Some(since) => json!({ "query": query, "since": since }),
        None => json!({ "query": query }),
    };
    format!("{MENTIONS_READ}{payload}")
}

pub fn mention_of(path: &str) -> Option<MentionCall> {
    let rest = path.strip_prefix(MENTIONS_READ)?;
    let parsed: MentionCall = serde_json::from_str(rest).ok()?;
    let len = parsed.query.chars().count();
    (1..=500).contains(&len).then_some(parsed)
}

pub async fn run_rollup(
    sandbox: &CodemodeSandbox,
    code: &str,
    input: &RollupInput,
    run_token: &str,
    output_validator: &Validator,
) -> PreviewResult {
    let mut store = IndexMap::new();
    store.insert(
        INPUT_KEY.to_owned(),
        serde_json::to_value(input).unwrap_or(Value::Null),
    );
    store.insert(RUN_KEY.to_owned(), Value::String(run_token.to_owned()));

    let outcome = match sandbox
        .execute(
            &wrap_code(code),
            ExecuteOptions {
                timeout: Some(Some(Duration::from_millis(RUN_TIMEOUT_MS))),
                store,
                ..Default::default()
            },
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            return PreviewResult::Err {
                error: err.to_string(),
            };
        }
    };

    if let Some(error) = outcome.error() {
        let prefix = error.name.as_deref().unwrap_or(match error.kind {
            pi_rs_codemode::ErrorKind::Script => "Script",
            pi_rs_codemode::ErrorKind::Timeout => "Timeout",
            pi_rs_codemode::ErrorKind::Aborted => "Aborted",
            pi_rs_codemode::ErrorKind::Sandbox => "Sandbox",
        });
        return PreviewResult::Err {
            error: format!("{prefix}: {}", error.message),
        };
    }

    let Some(val) = outcome.value() else {
        return PreviewResult::Err {
            error: "The script returned nothing: return the rollup.".to_owned(),
        };
    };

    #[derive(Deserialize)]
    struct Wrapped {
        output: Option<Value>,
        reads: Vec<String>,
    }
    let Ok(wrapped) = serde_json::from_value::<Wrapped>(val.clone()) else {
        return PreviewResult::Err {
            error: "The script returned nothing: return the rollup.".to_owned(),
        };
    };
    let Some(output) = wrapped.output else {
        return PreviewResult::Err {
            error: "The script returned nothing: return the rollup.".to_owned(),
        };
    };
    if let Err(err) = output_validator.check(&output, "rollup") {
        return PreviewResult::Err {
            error: format!("The returned rollup does not match its schema: {err}"),
        };
    }
    PreviewResult::Ok {
        output,
        reads: wrapped.reads,
    }
}

fn wrap_code(code: &str) -> String {
    let input_key =
        serde_json::to_string(INPUT_KEY).unwrap_or_else(|_| "\"p2.rollup-input\"".into());
    let run_key = serde_json::to_string(RUN_KEY).unwrap_or_else(|_| "\"p2.rollup-run\"".into());
    let mentions_read =
        serde_json::to_string(MENTIONS_READ).unwrap_or_else(|_| "\"mentions:\"".into());
    format!(
        r#"const __reads = new Set();
const __input = load({input_key});
const __field = (key) => {{ if (typeof key === "string") __reads.add("sessions." + key); }};
const sessions = __input.sessions.map((row) => new Proxy(row, {{
	get(target, key, receiver) {{ __field(key); return Reflect.get(target, key, receiver); }},
	has(target, key) {{ __field(key); return Reflect.has(target, key); }},
	getOwnPropertyDescriptor(target, key) {{ __field(key); return Reflect.getOwnPropertyDescriptor(target, key); }},
	ownKeys(target) {{ __reads.add("sessions.*"); return Reflect.ownKeys(target); }},
}}));
const projects = new Proxy(__input.projects, {{
	get(target, key, receiver) {{ __reads.add("projects"); return Reflect.get(target, key, receiver); }},
	ownKeys(target) {{ __reads.add("projects"); return Reflect.ownKeys(target); }},
}});
const now = () => {{ __reads.add("now"); return __input.now; }};
const __run = load({run_key});
const mentions = async (query, options = {{}}) => {{
	const call = options.since === undefined ? {{ query }} : {{ query, since: options.since }};
	__reads.add({mentions_read} + JSON.stringify(call));
	return {MENTIONS_GLOBAL}({{ run: __run, ...call }});
}};
const __output = await (async () => {{
{code}
}})();
return {{ output: __output, reads: [...__reads] }};"#
    )
}
