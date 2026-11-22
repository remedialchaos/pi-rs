//! Strict JSONL transport for embedding the headless coding agent.

mod client;
mod context;
mod jsonl;
pub(in crate::modes) mod session;
mod types;
mod ui;

pub use client::{RpcClient, RpcClientOptions};
pub use jsonl::{JsonlReader, serialize_json_line};
pub use types::*;

use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use pi_rs_agent::{AgentMessage, CancellationToken};
use pi_rs_ai::ImageContent;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{Mutex, Notify, mpsc, oneshot},
};
use uuid::Uuid;

use crate::core::{
    agent_session::{
        AgentSession, ExtensionRunner as SessionExtensions, ForkPosition, PromptDisposition,
        PromptOptions, SessionReason, StreamingBehavior,
    },
    agent_session_runtime::AgentSessionRuntime,
    bash_executor::{BashExecutor, BashExecutorOptions, BashOutput},
    event_bus::ExtensionEvent,
    extensions::types::ExtensionMode,
    prompt_templates::expand_prompt_template,
    skills::{SourceScope, create_synthetic_source_info},
};

use super::{diagnostics_stderr::write_diagnostics_to_stderr, json_event::to_json_event};
use ui::RpcUi;

type Reply = Result<Option<Value>, String>;
type Frame = io::Result<Option<Vec<u8>>>;

#[derive(Clone)]
struct Output(mpsc::UnboundedSender<Frame>, Arc<Watermark>);

const MAX_QUEUED_STDOUT_BYTES: usize = 128 * 1024;

#[derive(Default)]
struct Watermark {
    queued: AtomicUsize,
    drained: Notify,
}

impl Watermark {
    async fn wait(&self) {
        while self.queued.load(Ordering::Acquire) > MAX_QUEUED_STDOUT_BYTES {
            self.drained.notified().await;
        }
    }

    fn wrote(&self, count: usize) {
        let before = self.queued.fetch_sub(count, Ordering::AcqRel);
        if before > MAX_QUEUED_STDOUT_BYTES && before - count <= MAX_QUEUED_STDOUT_BYTES {
            self.drained.notify_one();
        }
    }
}

impl Output {
    fn send(&self, value: &impl Serialize) {
        let frame = serialize_json_line(value)
            .map(Some)
            .map_err(io::Error::from);
        let size = frame
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map_or(0, Vec::len);
        self.1.queued.fetch_add(size, Ordering::AcqRel);
        if self.0.send(frame).is_err() {
            self.1.wrote(size);
        }
    }

    fn session_event(&self, event: &crate::core::agent_session::AgentSessionEvent) {
        match to_json_event(event) {
            Ok(event) => self.send(&event),
            Err(error) => {
                let _ = self.0.send(Err(error.into()));
            }
        }
    }

    fn extension_error(&self, path: &str, event: &str, error: &str) {
        self.send(&json!({ "type": "extension_error", "extensionPath": path, "event": event, "error": error }));
    }
}

/// Run until EOF, shutdown requested by an extension, or a termination signal.
pub async fn run(runtime: &mut AgentSessionRuntime) -> io::Result<u8> {
    let terminate = super::termination_signal()?;
    run_until(runtime, tokio::io::stdin(), tokio::io::stdout(), terminate).await
}

/// Run RPC over an arbitrary async transport. Requests and UI responses use the same input.
pub async fn run_stream(
    runtime: &mut AgentSessionRuntime,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    run_until(runtime, input, output, std::future::pending())
        .await
        .map(|_| ())
}

async fn run_until(
    runtime: &mut AgentSessionRuntime,
    input: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    terminate: impl Future<Output = u8>,
) -> io::Result<u8> {
    let (output, mut frames) = mpsc::unbounded_channel();
    let (requests, mut transitions) = mpsc::unbounded_channel();
    let output = Output(output, Arc::new(Watermark::default()));
    let server = Server {
        active: std::sync::Mutex::new(runtime.session().clone()),
        runtime: Mutex::new(runtime),
        ui: Arc::new(RpcUi::new(output.clone())),
        output,
        transitions: requests,
        shutdown: Arc::new(Notify::new()),
        bash: std::sync::Mutex::new(HashMap::new()),
    };
    let drive = async {
        let mut input = JsonlReader::new(input);
        let mut work: FuturesUnordered<BoxFuture<'_, ()>> = FuturesUnordered::new();
        let mut terminate = std::pin::pin!(terminate);
        let mut eof = false;
        let mut shutdown = false;
        let mut exit_code = 0;
        work.push(Box::pin(server.start()));
        loop {
            if work.is_empty() && (eof || (shutdown && server.active_session().is_idle())) {
                break;
            }
            tokio::select! {
                biased;
                code = &mut terminate => {
                    exit_code = code;
                    server.ui.close();
                    server.abort_bash();
                    break;
                }
                _ = work.next(), if !work.is_empty() => {}
                () = server.shutdown.notified(), if !shutdown => { shutdown = true; }
                Some((transition, response)) = transitions.recv() => {
                    work.push(Box::pin(server.internal_transition(transition, response)));
                }
                line = input.next_line(), if !eof => match line? {
                    Some(line) => server.receive(&line, &mut work),
                    None => {
                        eof = true;
                        server.ui.close();
                        server.abort_bash();
                        if server.active_session().is_streaming() {
                            work.push(Box::pin(async { server.active_session().abort().await }));
                        }
                    }
                }
            }
        }
        drop(work);
        server.close().await;
        let _ = server.output.0.send(Ok(None));
        Ok::<_, io::Error>(exit_code)
    };
    let flush = async {
        while let Some(frame) = frames.recv().await {
            let Some(bytes) = frame? else { break };
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            server.output.1.wrote(bytes.len());
        }
        Ok::<_, io::Error>(())
    };
    let result = tokio::try_join!(drive, flush).map(|(code, ())| code);
    if result.is_err() {
        server.close().await;
    }
    result
}

enum Transition {
    New(Option<String>),
    Switch(String),
    Fork(String, ForkPosition),
    Clone,
    Reload,
}

type TransitionMessage = (Transition, oneshot::Sender<Reply>);

struct Server<'a> {
    runtime: Mutex<&'a mut AgentSessionRuntime>,
    active: std::sync::Mutex<AgentSession>,
    ui: Arc<RpcUi>,
    output: Output,
    transitions: mpsc::UnboundedSender<TransitionMessage>,
    shutdown: Arc<Notify>,
    bash: std::sync::Mutex<HashMap<Uuid, CancellationToken>>,
}

struct BoundSession {
    session: AgentSession,
    extensions: Arc<dyn SessionExtensions>,
    agent_dir: Option<String>,
}

impl BoundSession {
    fn new(runtime: &AgentSessionRuntime) -> Self {
        Self {
            session: runtime.session().clone(),
            extensions: runtime.services().extensions.clone(),
            agent_dir: runtime.services().agent_dir.clone(),
        }
    }
}

impl Server<'_> {
    fn active_session(&self) -> AgentSession {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    async fn start(&self) {
        let runtime = self.runtime.lock().await;
        self.bind(&runtime, SessionReason::Startup, None).await;
    }

    async fn bind(
        &self,
        runtime: &AgentSessionRuntime,
        reason: SessionReason,
        previous_file: Option<String>,
    ) {
        let session = runtime.session();
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = session.clone();
        let output = self.output.clone();
        session.subscribe(Arc::new(move |event| output.session_event(event)));
        let watermark = self.output.1.clone();
        session.set_output_backpressure(Arc::new(move || {
            let watermark = watermark.clone();
            Box::pin(async move { watermark.wait().await })
        }));
        if let Some(native) = runtime.services().extensions.native() {
            native.set_ui(ExtensionMode::Rpc, true);
            let output = self.output.clone();
            native.host.on_error(Arc::new(move |error| {
                output.extension_error(&error.extension_path, &error.event, &error.error);
            }));
            let mut event = json!({ "type": "session_start", "reason": reason.as_str() });
            if let Some(file) = previous_file {
                event["previousSessionFile"] = json!(file);
            }
            native.emit(ExtensionEvent::SessionStart, event).await;
        }
        write_diagnostics_to_stderr(&runtime.take_unreported_diagnostics());
    }

    async fn close(&self) {
        self.ui.close();
        self.abort_bash();
        let mut runtime = self.runtime.lock().await;
        runtime.session().abort().await;
        if let Some(native) = runtime.services().extensions.native() {
            native
                .emit(
                    ExtensionEvent::SessionShutdown,
                    json!({ "type": "session_shutdown", "reason": "quit" }),
                )
                .await;
            native.invalidate();
        }
        runtime.dispose().await;
    }

    fn receive<'a>(&'a self, line: &str, work: &mut FuturesUnordered<BoxFuture<'a, ()>>) {
        let parsed = match serde_json::from_str::<Value>(line) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.output.send(&RpcResponse::error(
                    None,
                    "parse",
                    format!("Failed to parse command: {error}"),
                ));
                return;
            }
        };
        let name = parsed.get("type").and_then(Value::as_str);
        if name == Some("extension_ui_response") {
            if let Ok(response) = serde_json::from_value(parsed) {
                self.ui.answer(response);
            }
            return;
        }
        let id = || parsed.get("id").and_then(Value::as_str).map(str::to_owned);
        let Some(name) = name else {
            self.output.send(&RpcResponse::error(
                id(),
                "parse",
                "Command must have a string type".into(),
            ));
            return;
        };
        if !RpcCommand::is_known(name) {
            self.output.send(&RpcResponse::error(
                id(),
                name,
                format!("Unknown command: {name}"),
            ));
            return;
        }
        let error_id = id();
        let name = name.to_owned();
        match serde_json::from_value::<RpcRequest>(parsed) {
            Ok(RpcRequest { id, command }) => work.push(Box::pin(self.respond(id, command))),
            Err(error) => self
                .output
                .send(&RpcResponse::error(error_id, &name, error.to_string())),
        }
    }

    async fn respond(&self, id: Option<String>, command: RpcCommand) {
        let name = command.name();
        let result = match command {
            RpcCommand::Abort => {
                self.active_session().abort().await;
                Ok(None)
            }
            RpcCommand::AbortRetry => {
                self.active_session().abort_retry();
                Ok(None)
            }
            RpcCommand::AbortBash => {
                self.abort_bash();
                Ok(None)
            }
            RpcCommand::NewSession { parent_session } => {
                self.transition(Transition::New(parent_session)).await
            }
            RpcCommand::SwitchSession { session_path } => {
                self.transition(Transition::Switch(session_path)).await
            }
            RpcCommand::Fork { entry_id } => {
                self.transition(Transition::Fork(entry_id, ForkPosition::Before))
                    .await
            }
            RpcCommand::Clone => self.transition(Transition::Clone).await,
            command => {
                let bound = {
                    let runtime = self.runtime.lock().await;
                    BoundSession::new(&runtime)
                };
                if let RpcCommand::Prompt {
                    message,
                    images,
                    streaming_behavior,
                } = command
                {
                    return self
                        .prompt(&bound, id, message, images, streaming_behavior)
                        .await;
                }
                self.execute(&bound, &id, command).await
            }
        };
        let response = match result {
            Ok(data) => RpcResponse::ok(id, name, data),
            Err(error) => RpcResponse::error(id, name, error),
        };
        self.output.send(&response);
    }

    async fn execute(
        &self,
        bound: &BoundSession,
        id: &Option<String>,
        command: RpcCommand,
    ) -> Reply {
        use RpcCommand as Cmd;
        let session = &bound.session;
        let data = match command {
            Cmd::Steer { message, images } => {
                self.queue(bound, message, images, RpcStreamingBehavior::Steer)
                    .await?
            }
            Cmd::FollowUp { message, images } => {
                self.queue(bound, message, images, RpcStreamingBehavior::FollowUp)
                    .await?
            }
            Cmd::ClearQueue => {
                let data = json!({ "steering": session.peek_steering(), "followUp": session.peek_follow_up() });
                session.clear_queue();
                data
            }
            Cmd::GetState => json!(session::state(session)),
            Cmd::SetModel { provider, model_id } => {
                let model = session
                    .available_models()
                    .into_iter()
                    .find(|model| model.provider == provider && model.id == model_id)
                    .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
                session
                    .set_model(model.clone())
                    .await
                    .map_err(|error| error.to_string())?;
                let effective = model.clamp_thinking_level(session.thinking_level());
                if effective != session.thinking_level() {
                    session.set_thinking_level(effective);
                }
                json!(model)
            }
            Cmd::CycleModel => Self::cycle_model(session).await?,
            Cmd::GetAvailableModels => json!({ "models": session.available_models() }),
            Cmd::SetThinkingLevel { level } => {
                let effective = session.model().clamp_thinking_level(level);
                if effective != session.thinking_level() {
                    session.set_thinking_level(effective);
                }
                return Ok(None);
            }
            Cmd::CycleThinkingLevel => {
                let model = session.model();
                if model.reasoning {
                    let levels: Vec<_> = model.supported_thinking_levels().collect();
                    let level = levels
                        .iter()
                        .position(|level| *level == session.thinking_level())
                        .and_then(|index| levels.get(index + 1))
                        .or_else(|| levels.first());
                    if let Some(&level) = level {
                        session.set_thinking_level(level);
                        json!({ "level": level })
                    } else {
                        Value::Null
                    }
                } else {
                    Value::Null
                }
            }
            Cmd::GetAvailableThinkingLevels => {
                json!({ "levels": session.model().supported_thinking_levels().collect::<Vec<_>>() })
            }
            Cmd::SetSteeringMode { mode } => {
                session.set_steering_mode(mode);
                return Ok(None);
            }
            Cmd::SetFollowUpMode { mode } => {
                session.set_follow_up_mode(mode);
                return Ok(None);
            }
            Cmd::Compact {
                custom_instructions,
            } => {
                let result = session
                    .compact(custom_instructions.as_deref())
                    .await
                    .map_err(|error| error.to_string())?;
                serde_json::to_value(result).map_err(|error| error.to_string())?
            }
            Cmd::SetAutoCompaction { enabled } => {
                session.set_auto_compaction_enabled(enabled);
                return Ok(None);
            }
            Cmd::SetAutoRetry { enabled } => {
                session.set_auto_retry_enabled(enabled);
                return Ok(None);
            }
            Cmd::Bash {
                command,
                exclude_from_context,
            } => self.bash(bound, id, command, exclude_from_context).await?,
            Cmd::GetSessionStats => session::stats(session),
            Cmd::ExportHtml { output_path } => session::export_html(session, output_path)?,
            Cmd::GetForkMessages => json!({ "messages": session::fork_messages(session) }),
            Cmd::GetEntries { since } => session::entries(session, since.as_deref())?,
            Cmd::GetTree => session::tree(session),
            Cmd::GetLastAssistantText => session::last_assistant_text(session)
                .map_or_else(|| json!({}), |text| json!({ "text": text })),
            Cmd::SetSessionName { name } => {
                let name = session::set_name(session, &name)?;
                let mut event = json!({ "type": "session_info_changed" });
                if let Some(name) = name {
                    event["name"] = json!(name);
                }
                self.output.send(&event);
                session.emit_runtime_event(event);
                return Ok(None);
            }
            Cmd::GetMessages => json!({ "messages": session.messages() }),
            Cmd::GetCommands => json!({ "commands": self.get_commands(bound) }),
            Cmd::Prompt { .. }
            | Cmd::Abort
            | Cmd::AbortRetry
            | Cmd::AbortBash
            | Cmd::NewSession { .. }
            | Cmd::SwitchSession { .. }
            | Cmd::Fork { .. }
            | Cmd::Clone => return Err("Command must be dispatched directly".into()),
        };
        Ok(Some(data))
    }

    async fn cycle_model(session: &AgentSession) -> Result<Value, String> {
        let current = session.model();
        let available = session.available_models();
        let scoped = session.scoped_models();
        let is_scoped = !scoped.is_empty();
        let models: Vec<_> = if is_scoped {
            scoped
                .into_iter()
                .filter(|(model, _)| {
                    available
                        .iter()
                        .any(|candidate| same_model(candidate, model))
                })
                .collect()
        } else {
            available.into_iter().map(|model| (model, None)).collect()
        };
        let next = match models
            .iter()
            .position(|(model, _)| same_model(model, &current))
        {
            Some(_) if models.len() == 1 => return Ok(Value::Null),
            Some(index) => models.get((index + 1) % models.len()),
            None => models.first(),
        };
        let Some((model, level)) = next else {
            return Ok(Value::Null);
        };
        session
            .set_model(model.clone())
            .await
            .map_err(|error| error.to_string())?;
        let effective =
            model.clamp_thinking_level(level.unwrap_or_else(|| session.thinking_level()));
        if effective != session.thinking_level() {
            session.set_thinking_level(effective);
        }
        Ok(
            json!({ "model": model, "thinkingLevel": session.thinking_level(), "isScoped": is_scoped }),
        )
    }

    async fn prompt(
        &self,
        bound: &BoundSession,
        id: Option<String>,
        message: String,
        images: Vec<ImageContent>,
        behavior: Option<RpcStreamingBehavior>,
    ) {
        let result = self
            .prompt_inner(bound, &id, message, images, behavior)
            .await;
        if let Some(response) = match result {
            Ok(Some(disposition)) => Some(RpcResponse::ok(
                id,
                "prompt",
                Some(json!({ "disposition": disposition.as_str() })),
            )),
            Ok(None) => None,
            Err(error) => Some(RpcResponse::error(id, "prompt", error)),
        } {
            self.output.send(&response);
        }
    }

    async fn prompt_inner(
        &self,
        bound: &BoundSession,
        id: &Option<String>,
        message: String,
        images: Vec<ImageContent>,
        behavior: Option<RpcStreamingBehavior>,
    ) -> Result<Option<PromptDisposition>, String> {
        if let Some(native) = bound.extensions.native()
            && let Some((name, args)) = extension_command(&message)
            && native.get_command(name).is_some()
        {
            let context = self.command_context(bound, native);
            if let Err(error) = native.execute_command(name, args, &context).await {
                self.output
                    .extension_error(&format!("command:{name}"), "command", &error);
            }
            return Ok(Some(PromptDisposition::Handled));
        }
        let Some((text, images)) = self.prepare_input(bound, message, images, behavior).await?
        else {
            return Ok(Some(PromptDisposition::Handled));
        };
        let mut accepted = false;
        let result = bound
            .session
            .prompt_with_preflight(
                text,
                PromptOptions {
                    streaming_behavior: behavior.map(|behavior| match behavior {
                        RpcStreamingBehavior::Steer => StreamingBehavior::Steer,
                        RpcStreamingBehavior::FollowUp => StreamingBehavior::FollowUp,
                    }),
                    expand_prompt_templates: true,
                    source: "rpc".into(),
                    images,
                },
                |disposition| {
                    accepted = true;
                    self.output.send(&RpcResponse::ok(
                        id.clone(),
                        "prompt",
                        Some(json!({ "disposition": disposition.as_str() })),
                    ));
                },
            )
            .await;
        if accepted {
            Ok(None)
        } else {
            result.map(Some).map_err(|error| error.to_string())
        }
    }

    async fn queue(
        &self,
        bound: &BoundSession,
        message: String,
        images: Vec<ImageContent>,
        behavior: RpcStreamingBehavior,
    ) -> Result<Value, String> {
        if let Some(native) = bound.extensions.native()
            && let Some((name, _)) = extension_command(&message)
            && native.get_command(name).is_some()
        {
            return Err(format!(
                "Extension command \"/{name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            ));
        }
        let disposition = match self
            .prepare_input(bound, message, images, Some(behavior))
            .await?
        {
            Some((text, images)) => match behavior {
                RpcStreamingBehavior::Steer => bound.session.steer_with_images(text, images).await,
                RpcStreamingBehavior::FollowUp => {
                    bound.session.follow_up_with_images(text, images).await
                }
            }
            .map_err(|error| error.to_string())?,
            None => PromptDisposition::Handled,
        };
        Ok(json!({ "disposition": disposition.as_str() }))
    }

    async fn prepare_input(
        &self,
        bound: &BoundSession,
        mut text: String,
        mut images: Vec<ImageContent>,
        behavior: Option<RpcStreamingBehavior>,
    ) -> Result<Option<(String, Vec<ImageContent>)>, String> {
        if let Some(native) = bound.extensions.native()
            && native.has_handlers(ExtensionEvent::Input)
        {
            let wire_images = (!images.is_empty()).then(|| json!(images));
            let behavior = behavior
                .filter(|_| bound.session.is_streaming())
                .map(|behavior| match behavior {
                    RpcStreamingBehavior::Steer => "steer",
                    RpcStreamingBehavior::FollowUp => "followUp",
                });
            let mut result = native.emit_input(&text, wire_images, "rpc", behavior).await;
            if result.get("action").and_then(Value::as_str) == Some("handled") {
                return Ok(None);
            }
            if let Some(transformed) = result.get("text").and_then(Value::as_str) {
                text = transformed.to_owned();
            }
            if let Some(value) = result
                .as_object_mut()
                .and_then(|result| result.shift_remove("images"))
            {
                images = serde_json::from_value(value).map_err(|error| error.to_string())?;
            }
        }
        if let Some(resources) = bound.extensions.resources() {
            if let Some(skill) = text.strip_prefix("/skill:") {
                let (name, args) = skill.split_once(' ').unwrap_or((skill, ""));
                if let Some(skill) = resources.skills.iter().find(|skill| skill.name == name) {
                    match std::fs::read_to_string(&skill.file_path) {
                        Ok(content) => {
                            let body = crate::utils::frontmatter::strip_frontmatter(&content)
                                .unwrap_or(content);
                            let prefix = format!(
                                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                                skill.name,
                                skill.file_path.display(),
                                skill.base_dir.display(),
                                session::js_trim(&body)
                            );
                            let args = session::js_trim(args);
                            text = if args.is_empty() {
                                prefix
                            } else {
                                format!("{prefix}\n\n{args}")
                            };
                        }
                        Err(error) => self.output.extension_error(
                            &skill.file_path.to_string_lossy(),
                            "skill_expansion",
                            &error.to_string(),
                        ),
                    }
                }
            }
            if text.starts_with('/') && !resources.prompts.is_empty() {
                text = expand_prompt_template(&text, &resources.prompts);
            }
        }
        Ok(Some((text, images)))
    }

    fn get_commands(&self, bound: &BoundSession) -> Vec<RpcSlashCommand> {
        let mut commands = Vec::new();
        if let Some(native) = bound.extensions.native() {
            commands.extend(native.get_registered_commands().into_iter().map(|command| {
                let path = Path::new(&command.extension_path);
                let (source, scope, base) = if command.extension_path.starts_with("builtin:") {
                    ("builtin", None, None)
                } else if let Some(synthetic) = command
                    .extension_path
                    .strip_prefix('<')
                    .and_then(|path| path.strip_suffix('>'))
                {
                    let source = synthetic
                        .split(':')
                        .next()
                        .filter(|source| !source.is_empty())
                        .unwrap_or("temporary");
                    (source, None, None)
                } else if let Some(agent_dir) = bound
                    .agent_dir
                    .as_deref()
                    .filter(|agent_dir| path.starts_with(agent_dir))
                {
                    ("local", Some(SourceScope::User), Some(Path::new(agent_dir)))
                } else if path.starts_with(bound.session.cwd()) {
                    ("local", Some(SourceScope::Project), path.parent())
                } else {
                    ("extension", None, path.parent())
                };
                RpcSlashCommand {
                    name: command.invocation_name,
                    description: command.definition.description,
                    source: RpcSlashCommandSource::Extension,
                    source_info: create_synthetic_source_info(path, source, scope, None, base),
                }
            }));
        }
        if let Some(resources) = bound.extensions.resources() {
            commands.extend(resources.prompts.iter().map(|template| RpcSlashCommand {
                name: template.name.clone(),
                description: Some(template.description.clone()),
                source: RpcSlashCommandSource::Prompt,
                source_info: template.source_info.clone(),
            }));
            commands.extend(resources.skills.iter().map(|skill| RpcSlashCommand {
                name: format!("skill:{}", skill.name),
                description: Some(skill.description.clone()),
                source: RpcSlashCommandSource::Skill,
                source_info: skill.source_info.clone(),
            }));
        }
        commands
    }

    fn abort_bash(&self) {
        for token in self
            .bash
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            token.cancel();
        }
    }

    async fn bash(
        &self,
        bound: &BoundSession,
        id: &Option<String>,
        command: String,
        exclude: Option<bool>,
    ) -> Result<Value, String> {
        let cwd = bound.session.cwd();
        let overridden = match bound.extensions.native() {
            Some(native) if native.has_handlers(ExtensionEvent::UserBash) => native.emit_user_bash(json!({ "type": "user_bash", "command": command, "excludeFromContext": exclude.unwrap_or(false), "cwd": cwd })).await,
            _ => None,
        };
        if overridden
            .as_ref()
            .is_some_and(|override_result| override_result.get("operations").is_some())
        {
            return Err(
                "Custom extension Bash operations are not supported by the native host".into(),
            );
        }
        let data = if let Some(result) = overridden.and_then(|mut result| {
            result
                .as_object_mut()
                .and_then(|result| result.shift_remove("result"))
        }) {
            result
        } else {
            let key = Uuid::new_v4();
            let cancel = CancellationToken::new();
            self.bash
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key, cancel.clone());
            let output = self.output.clone();
            let id = id.clone();
            let options = BashExecutorOptions {
                cancel: Some(cancel),
                shell_path: bound.session.settings().shell_path(),
                on_chunk: Some(Arc::new(move |delta| {
                    let mut event = json!({ "type": "bash_execution_update", "delta": delta });
                    if let Some(id) = &id {
                        event["id"] = json!(id);
                    }
                    output.send(&event);
                })),
                ..BashExecutorOptions::default()
            };
            let settings = bound.session.settings();
            let prefixed = settings
                .shell_command_prefix()
                .filter(|prefix| !prefix.is_empty())
                .map(|prefix| format!("{prefix}\n{command}"));
            let result = BashExecutor::new()
                .execute(
                    prefixed.as_deref().unwrap_or(&command),
                    Path::new(&cwd),
                    &options,
                )
                .await;
            self.bash
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&key);
            let BashOutput {
                output,
                exit_code,
                cancelled,
                truncated,
                full_output_path,
            } = result?;
            let mut data = json!({ "output": output });
            if let Some(exit_code) = exit_code {
                data["exitCode"] = json!(exit_code);
            }
            data["cancelled"] = json!(cancelled);
            data["truncated"] = json!(truncated);
            if let Some(path) = full_output_path {
                data["fullOutputPath"] = json!(path);
            }
            data
        };
        let mut fields = data
            .as_object()
            .cloned()
            .ok_or("Invalid extension Bash result")?;
        fields.insert("command".into(), json!(command));
        fields.insert(
            "timestamp".into(),
            json!(chrono::Utc::now().timestamp_millis()),
        );
        if let Some(exclude) = exclude {
            fields.insert("excludeFromContext".into(), json!(exclude));
        }
        let message =
            AgentMessage::custom("bashExecution", fields).map_err(|error| error.to_string())?;
        bound
            .session
            .record_bash_message(message)
            .map_err(|error| error.to_string())?;
        Ok(data)
    }
}

fn extension_command(message: &str) -> Option<(&str, &str)> {
    let message = message.strip_prefix('/')?;
    Some(message.split_once(' ').unwrap_or((message, "")))
}

fn same_model(a: &pi_rs_ai::Model, b: &pi_rs_ai::Model) -> bool {
    a.id == b.id && a.provider == b.provider
}
