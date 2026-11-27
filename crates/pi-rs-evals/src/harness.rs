//! Pi eval harness ported onto `pi-rs-ai` and `pi-rs-coding-agent`
//! (port of `harness.ts`).
//!
//! Native runs isolate a child Pi process and keep provider credentials in the
//! parent. A disk-persisted SDK session can also be supplied by a test.

mod broker;
mod native;
#[cfg(test)]
mod tests;
pub use native::{EvalWorkspace, create_pi_documentation_eval_harness, run_pi_coding_agent};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_rs_ai::{
    AssistantContent, Context, InputContent, Message, Model, StopReason, StreamOptions,
    StringOrBlocks, ToolCall as AiToolCall, TranscriptContext,
};
use pi_rs_coding_agent::core::agent_session::{AgentSession, PromptDisposition, PromptOptions};
use pi_rs_coding_agent::core::agent_session_services::DiskSession;
use pi_rs_coding_agent::core::messages::input_content_to_text;
use pi_rs_coding_agent::core::sdk::{PiSdk, PiSdkOptions, create_sdk_session};
use pi_rs_coding_agent::core::session_manager::SessionManager as DiskSessionManager;
use pi_rs_coding_agent::core::system_prompt::{SystemPromptOptions, build_system_prompt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{EvalError, EvalResult};
use crate::plan::DocumentationVariant;
use crate::report::PI_SESSION_SNAPSHOT_ARTIFACT;

/// JSON-safe value used by harness artifacts and normalized runs.
pub type JsonValue = Value;

/// Shared artifact bag populated during a harness run.
pub type ArtifactBag = Arc<Mutex<BTreeMap<String, JsonValue>>>;

/// Tools allowed for documentation evals (no shell or unrestricted network).
pub const DOCUMENTATION_EVAL_TOOLS: [&str; 6] = ["read", "write", "edit", "grep", "find", "ls"];

/// Model selection used by a harness run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiCodingAgentModelSelection {
    pub provider: String,
    pub id: String,
}

/// Input accepted by a harness run: a single prompt or a sequence of steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiCodingAgentInput {
    Text(String),
    Steps(Vec<HarnessStep>),
}

impl From<String> for PiCodingAgentInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for PiCodingAgentInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

/// One step in a multi-step harness input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessStep {
    Prompt(String),
    Reload,
}

/// Options controlling a Pi coding-agent harness.
#[derive(Default)]
pub struct PiCodingAgentHarnessOptions {
    pub name: Option<String>,
    pub model: Option<PiCodingAgentModelSelection>,
    pub workspace_files: BTreeMap<String, String>,
    pub expected_pi_documentation: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub extensions: Vec<PathBuf>,
    /// Explicit fixture values supplied by the evaluator.
    pub environment: BTreeMap<String, String>,
}

/// Transcript event captured from an agent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    Message {
        role: TranscriptRole,
        content: JsonValue,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: JsonValue,
    },
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        name: String,
        content: JsonValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<JsonValue>,
    },
}

/// Roles emitted in transcript message events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptRole {
    System,
    User,
    Assistant,
}

/// Token and tool usage for one harness run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(rename = "inputTokens")]
    pub input_tokens: Option<f64>,
    #[serde(rename = "outputTokens")]
    pub output_tokens: Option<f64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: Option<f64>,
    #[serde(rename = "toolCalls")]
    pub tool_calls: Option<f64>,
    pub metadata: Map<String, JsonValue>,
}

/// Wall-clock timing for one harness run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingSummary {
    #[serde(rename = "totalMs")]
    pub total_ms: f64,
}

/// Lightweight normalized harness result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleHarnessResult<T> {
    pub output: T,
    pub events: Vec<TranscriptEvent>,
    pub usage: UsageSummary,
    pub timings: TimingSummary,
    pub artifacts: BTreeMap<String, JsonValue>,
    pub metadata: Map<String, JsonValue>,
    pub errors: Vec<JsonValue>,
}

/// Error carrying an optional partial run, matching `attachHarnessRunToError`.
#[derive(Debug)]
pub struct HarnessError {
    pub message: String,
    pub partial_run: Option<Box<SimpleHarnessResult<JsonValue>>>,
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HarnessError {}

/// The result of isolating process environment state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedEnvironment {
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    /// `PI_EVAL_*` variables that upstream deletes from `process.env`.
    pub removed_eval_vars: Vec<String>,
}

/// Compute the isolated environment for a harness run.
pub fn apply_isolated_environment(
    home: impl Into<PathBuf>,
    agent_dir: impl Into<PathBuf>,
    environment: &HashMap<String, String>,
) -> IsolatedEnvironment {
    let mut removed_eval_vars: Vec<String> = environment
        .keys()
        .filter(|key| key.starts_with("PI_EVAL_"))
        .cloned()
        .collect();
    removed_eval_vars.sort();
    IsolatedEnvironment {
        home: home.into(),
        agent_dir: agent_dir.into(),
        removed_eval_vars,
    }
}

/// Resolve model selection from explicit values or environment defaults.
pub fn resolve_model_selection(
    explicit: Option<&PiCodingAgentModelSelection>,
    provider_env: Option<&str>,
    model_env: Option<&str>,
) -> EvalResult<PiCodingAgentModelSelection> {
    let provider = explicit
        .map(|selection| selection.provider.as_str())
        .or(provider_env)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let id = explicit
        .map(|selection| selection.id.as_str())
        .or(model_env)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (provider, id) {
        (Some(provider), Some(id)) => Ok(PiCodingAgentModelSelection {
            provider: provider.to_owned(),
            id: id.to_owned(),
        }),
        _ => Err(EvalError::message(
            "Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults.",
        )),
    }
}

/// Sandbox UID/GID pair from the eval container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxIdentity {
    pub uid: u32,
    pub gid: u32,
}

fn parse_sandbox_id(name: &str, value: Option<&str>) -> EvalResult<Option<u32>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let id: u64 = value
        .parse()
        .map_err(|_| EvalError::message(format!("{name} must be a positive integer.")))?;
    if id == 0 || id > u32::MAX as u64 {
        return Err(EvalError::message(format!(
            "{name} must be a positive integer."
        )));
    }
    Ok(Some(id as u32))
}

/// Validate the sandbox identity environment variables.
pub fn resolve_sandbox_identity(
    environment: &HashMap<String, String>,
) -> EvalResult<Option<SandboxIdentity>> {
    let uid = parse_sandbox_id(
        "PI_EVAL_SANDBOX_UID",
        environment.get("PI_EVAL_SANDBOX_UID").map(String::as_str),
    )?;
    let gid = parse_sandbox_id(
        "PI_EVAL_SANDBOX_GID",
        environment.get("PI_EVAL_SANDBOX_GID").map(String::as_str),
    )?;
    match (uid, gid) {
        (None, None) => Ok(None),
        (Some(uid), Some(gid)) => Ok(Some(SandboxIdentity { uid, gid })),
        _ => Err(EvalError::message(
            "Set both PI_EVAL_SANDBOX_UID and PI_EVAL_SANDBOX_GID, or neither.",
        )),
    }
}

/// Validate a documentation variant value.
pub fn resolve_documentation_variant(value: Option<&str>) -> EvalResult<DocumentationVariant> {
    match value {
        Some("without_docs") => Ok(DocumentationVariant::WithoutDocs),
        Some("with_docs") => Ok(DocumentationVariant::WithDocs),
        _ => Err(EvalError::message(
            "PI_EVAL_VARIANT must be \"without_docs\" or \"with_docs\".",
        )),
    }
}

/// Strip only the documentation routing section from a default Pi system prompt.
pub fn exclude_pi_documentation(default_prompt: &str) -> EvalResult<String> {
    let start_marker = "\n<docs>\n";
    let end_marker = "\n</docs>";
    let Some(start) = default_prompt.find(start_marker) else {
        return Err(EvalError::message(
            "Default Pi system prompt has no Pi documentation section.",
        ));
    };
    let search_from = start + start_marker.len();
    let Some(end_relative) = default_prompt[search_from..].find(end_marker) else {
        return Err(EvalError::message(
            "Default Pi system prompt has no complete Pi documentation section.",
        ));
    };
    let end = search_from + end_relative + end_marker.len();
    let Some(cwd) = default_prompt.rfind("\n<cwd>\n") else {
        return Err(EvalError::message(
            "Default Pi system prompt has no working-directory section.",
        ));
    };
    if cwd < end {
        return Err(EvalError::message(
            "Default Pi system prompt has no working-directory section.",
        ));
    }
    let mut result = String::with_capacity(default_prompt.len());
    result.push_str(&default_prompt[..start]);
    result.push_str(&default_prompt[end..]);
    Ok(result)
}

/// Verify that the prompt actually sent matches the requested doc variant.
pub fn verify_system_prompt(
    system_prompt: &str,
    name: Option<&str>,
    expected_pi_documentation: Option<bool>,
) -> EvalResult<String> {
    let Some(expected) = expected_pi_documentation else {
        return Ok(system_prompt.to_owned());
    };
    let label = name.unwrap_or("eval");
    if !system_prompt.contains("\n<rules>\n") {
        return Err(EvalError::message(format!(
            "Pi system prompt lost its rules in the {label} eval variant."
        )));
    }
    let has_documentation = system_prompt.contains("\n<docs>\nPi documentation (read only");
    if has_documentation != expected {
        return Err(EvalError::message(format!(
            "Pi system prompt does not match the {label} eval variant."
        )));
    }
    Ok(system_prompt.to_owned())
}

/// Safely seed a temporary workspace with fixture files.
pub fn seed_workspace(workspace: &Path, files: &BTreeMap<String, String>) -> EvalResult<()> {
    for (name, content) in files {
        if name.is_empty() || Path::new(name).is_absolute() {
            return Err(EvalError::message(format!(
                "Invalid workspace fixture path: {name}"
            )));
        }
        let path = lexical_join(workspace, name);
        if !path.starts_with(workspace) {
            return Err(EvalError::message(format!(
                "Workspace fixture escapes the workspace: {name}"
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&path, content.as_bytes())?;
    }
    Ok(())
}

fn lexical_join(base: &Path, relative: &str) -> PathBuf {
    let mut result = base.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> EvalResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> EvalResult<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

fn assistant_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_call_event(call: &AiToolCall) -> TranscriptEvent {
    TranscriptEvent::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: Value::Object(call.arguments.clone()),
    }
}

/// Convert LLM messages from a session into deterministic transcript events.
pub fn to_transcript_events(messages: impl IntoIterator<Item = Message>) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    StringOrBlocks::String(text) => Value::String(text.clone()),
                    StringOrBlocks::Blocks(blocks) => Value::String(input_content_to_text(blocks)),
                };
                events.push(TranscriptEvent::Message {
                    role: TranscriptRole::User,
                    content,
                });
            }
            Message::Assistant(assistant) => {
                let text = assistant_text(&assistant.content);
                if !text.is_empty() {
                    events.push(TranscriptEvent::Message {
                        role: TranscriptRole::Assistant,
                        content: Value::String(text),
                    });
                }
                for part in &assistant.content {
                    if let AssistantContent::ToolCall(call) = part {
                        events.push(tool_call_event(call));
                    }
                }
            }
            Message::ToolResult(result) => {
                let all_text = result
                    .content
                    .iter()
                    .all(|part| matches!(part, InputContent::Text(_)));
                let content = if all_text {
                    Value::String(input_content_to_text(&result.content))
                } else {
                    serde_json::to_value(&result.content).unwrap_or(Value::Null)
                };
                let error = result.is_error.then(|| {
                    let mut object = Map::new();
                    let message = content
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .unwrap_or("Tool failed");
                    object.insert("message".to_owned(), Value::String(message.to_owned()));
                    Value::Object(object)
                });
                events.push(TranscriptEvent::ToolResult {
                    tool_call_id: result.tool_call_id.clone(),
                    name: result.tool_name.clone(),
                    content,
                    error,
                });
            }
            Message::System(_) => {}
        }
    }
    events
}

fn llm_messages(session: &AgentSession) -> Vec<Message> {
    session
        .messages()
        .iter()
        .filter_map(|message| message.to_llm())
        .collect()
}

fn assistant_response(messages: &[Message]) -> EvalResult<String> {
    let assistant = messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(a) => Some(a),
            _ => None,
        })
        .ok_or_else(|| EvalError::message("Agent run completed without an assistant message."))?;
    if !matches!(
        assistant.stop_reason,
        StopReason::Stop | StopReason::ToolUse
    ) {
        return Err(EvalError::message(
            assistant.error_message.clone().unwrap_or_else(|| {
                format!(
                    "Agent run ended with unexpected stop reason: {}.",
                    assistant.stop_reason.as_str()
                )
            }),
        ));
    }
    let text = assistant_text(&assistant.content);
    if text.is_empty() && assistant.stop_reason == StopReason::Stop {
        return Err(EvalError::message("Agent run produced no assistant text."));
    }
    Ok(text)
}

fn snapshot(path: Option<&Path>) -> EvalResult<String> {
    let missing = || EvalError::message("Pi eval produced no session file.");
    let path = path.ok_or_else(missing)?;
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(missing()),
        other => Ok(other?),
    }
}

/// Build the default Pi system prompt for a workspace and tool selection.
pub fn build_default_system_prompt(workspace: &Path, tools: &[String]) -> EvalResult<String> {
    build_system_prompt(&SystemPromptOptions {
        cwd: workspace.to_path_buf(),
        selected_tools: Some(tools.to_vec()),
        ..SystemPromptOptions::default()
    })
    .map_err(EvalError::message)
}

/// Construct an offline faux-backed, disk-persisted SDK session.
///
/// The stream function is built inline so the closure is coerced directly to
/// the SDK's `StreamFn` type without naming a cross-crate type alias.
pub fn create_faux_harness_session(
    workspace: &Path,
    sessions_directory: &Path,
    model: Model,
    models: pi_rs_ai::Models,
) -> EvalResult<(PiSdk, Model, ArtifactBag)> {
    std::fs::create_dir_all(sessions_directory)?;
    let manager = DiskSessionManager::create_in(workspace, sessions_directory, None)
        .map_err(EvalError::message)?;
    let session_manager: Arc<dyn pi_rs_coding_agent::core::agent_session::SessionManager> =
        Arc::new(DiskSession::new(manager));
    let stream_fn = Arc::new(
        move |model: Model, context: TranscriptContext, options: StreamOptions| {
            let models = models.clone();
            let future: std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = std::result::Result<pi_rs_ai::AssistantMessageEventStream, _>,
                        > + Send,
                >,
            > = Box::pin(async move {
                Ok(models.stream_simple(
                    model,
                    Context {
                        messages: context.into_messages(),
                        ..Context::default()
                    },
                    options,
                ))
            });
            future
        },
    );
    let sdk = create_sdk_session(PiSdkOptions {
        cwd: workspace.to_string_lossy().into_owned(),
        model: Some(model.clone()),
        session_manager: Some(session_manager),
        stream_fn: Some(stream_fn),
        ..PiSdkOptions::default()
    })
    .map_err(|error| EvalError::message(format!("{error}")))?;
    Ok((sdk, model, Arc::new(Mutex::new(BTreeMap::new()))))
}

/// Execute one prepared SDK session and collect normalized diagnostics.
pub async fn run_prepared_session(
    sdk: &PiSdk,
    model: &Model,
    input: &PiCodingAgentInput,
    options: &PiCodingAgentHarnessOptions,
    artifacts: &Mutex<BTreeMap<String, JsonValue>>,
) -> EvalResult<SimpleHarnessResult<JsonValue>> {
    let started = Instant::now();
    let steps: Vec<HarnessStep> = match input {
        PiCodingAgentInput::Text(text) => vec![HarnessStep::Prompt(text.clone())],
        PiCodingAgentInput::Steps(steps) => steps.clone(),
    };
    let mut response: Option<String> = None;
    for step in steps {
        match step {
            HarnessStep::Reload => sdk
                .session()
                .reload()
                .await
                .map_err(|error| EvalError::message(format!("{error}")))?,
            HarnessStep::Prompt(text) => {
                let previous = llm_messages(sdk.session()).len();
                let disposition = sdk
                    .prompt(text, PromptOptions::default())
                    .await
                    .map_err(|error| EvalError::message(format!("{error}")))?;
                if disposition == PromptDisposition::Queued {
                    return Err(EvalError::message(
                        "Agent run was queued instead of started.",
                    ));
                }
                response = Some(assistant_response(
                    llm_messages(sdk.session())
                        .get(previous..)
                        .unwrap_or_default(),
                )?);
            }
        }
    }
    let response = response.ok_or_else(|| {
        EvalError::message("Pi eval input must include at least one prompt step.")
    })?;

    let system_prompt = sdk.session().system_prompt();
    let verified = verify_system_prompt(
        &system_prompt,
        options.name.as_deref(),
        options.expected_pi_documentation,
    )?;
    let stats = sdk.stats();
    let mut metadata = Map::new();
    metadata.insert(
        "systemPromptSha256".to_owned(),
        Value::String(sha256_hex(verified.as_bytes())),
    );
    let mut metrics_metadata = Map::new();
    metrics_metadata.insert(
        "cacheReadTokens".to_owned(),
        json_number(stats.tokens.cache_read),
    );
    metrics_metadata.insert(
        "cacheWriteTokens".to_owned(),
        json_number(stats.tokens.cache_write),
    );
    if has_pricing(model) {
        metrics_metadata.insert("estimatedCostUsd".to_owned(), json_number(stats.cost));
    }
    let usage = UsageSummary {
        provider: Some(model.provider.clone()),
        model: Some(model.id.clone()),
        input_tokens: Some(stats.tokens.input),
        output_tokens: Some(stats.tokens.output),
        total_tokens: Some(stats.tokens.total),
        tool_calls: Some(stats.tool_calls as f64),
        metadata: metrics_metadata,
    };
    let events = to_transcript_events(llm_messages(sdk.session()));

    {
        let mut bag = artifacts.lock().unwrap_or_else(|error| error.into_inner());
        bag.insert(
            "runId".to_owned(),
            Value::String(sdk.session().session_id()),
        );
        let path = sdk.session().session_file().map(PathBuf::from);
        let text = snapshot(path.as_deref())?;
        bag.insert(PI_SESSION_SNAPSHOT_ARTIFACT.to_owned(), Value::String(text));
    }
    let result_artifacts = artifacts
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();

    Ok(SimpleHarnessResult {
        output: Value::String(response),
        events,
        usage,
        timings: TimingSummary {
            total_ms: started.elapsed().as_secs_f64() * 1000.0,
        },
        artifacts: result_artifacts,
        metadata,
        errors: Vec::new(),
    })
}

fn has_pricing(model: &Model) -> bool {
    [&model.cost.rates]
        .into_iter()
        .chain(model.cost.tiers.iter().map(|tier| &tier.rates))
        .any(|rates| {
            rates.input > 0.0
                || rates.output > 0.0
                || rates.cache_read > 0.0
                || rates.cache_write > 0.0
        })
}

fn json_number(value: f64) -> Value {
    Value::Number(
        serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0)),
    )
}

/// A named Pi coding-agent harness.
pub struct PiCodingAgentHarness {
    pub name: String,
    pub options: PiCodingAgentHarnessOptions,
}

impl PiCodingAgentHarness {
    /// Construct a harness from options.
    pub fn new(options: PiCodingAgentHarnessOptions) -> Self {
        let name = options
            .name
            .clone()
            .unwrap_or_else(|| "pi-coding-agent".to_owned());
        Self { name, options }
    }

    /// Run the harness against a prepared SDK session.
    pub async fn run(
        &self,
        sdk: &PiSdk,
        model: &Model,
        input: &PiCodingAgentInput,
        artifacts: &Mutex<BTreeMap<String, JsonValue>>,
    ) -> EvalResult<SimpleHarnessResult<JsonValue>> {
        run_prepared_session(sdk, model, input, &self.options, artifacts).await
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Deterministically normalize a lightweight harness result.
pub fn normalize_harness_run(
    mut result: SimpleHarnessResult<JsonValue>,
) -> SimpleHarnessResult<JsonValue> {
    result.events.sort_by(|left, right| {
        serde_json::to_string(left)
            .unwrap_or_default()
            .cmp(&serde_json::to_string(right).unwrap_or_default())
    });
    result.artifacts.retain(|_, value| !value.is_null());
    result
}

/// Collect distinct outcome markers used by the comparison `flaky` flag.
pub fn outcome_set(results: &[SimpleHarnessResult<JsonValue>]) -> HashSet<String> {
    results
        .iter()
        .map(|result| (result.output.as_str().is_some()).to_string())
        .collect()
}
