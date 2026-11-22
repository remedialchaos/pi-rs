use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use pi_rs_ai::ImageContent;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc::UnboundedSender, oneshot};

use crate::{
    core::{
        agent_session::{
            AgentSession, ExtensionRunner as SessionExtensions, ForkPosition, PromptOptions,
            SessionReason, StreamingBehavior,
        },
        agent_session_runtime::AgentSessionRuntime,
        agent_session_services::DiskSession,
        event_bus::ExtensionEvent,
        extensions::{
            runner::ExtensionRunner,
            types::{
                ContextUsage, ExtensionCommandContext, ExtensionContext, ExtensionMode, ExtensionUi,
            },
        },
        prompt_templates::expand_prompt_template,
        session_manager::{SessionEntry, SessionManager},
    },
    modes::rpc::session,
    utils::frontmatter::strip_frontmatter,
};

use super::{UiEvent, extension_ui::UiHost};

#[derive(Clone)]
pub(super) struct Bound {
    pub(super) session: AgentSession,
    pub(super) extensions: Arc<dyn SessionExtensions>,
    pub(super) generation: u64,
    pub(super) trusted_project: Option<bool>,
    pub(super) catalog: Option<Arc<super::utilities::ModelCatalogRefresh>>,
}

impl Bound {
    fn new(runtime: &AgentSessionRuntime, generation: u64) -> Self {
        Self {
            session: runtime.session().clone(),
            extensions: runtime.services().extensions.clone(),
            generation,
            trusted_project: runtime.services().settings.trusted_project(),
            catalog: runtime
                .services()
                .models
                .model_catalog()
                .map(super::utilities::ModelCatalogRefresh::new)
                .map(Arc::new),
        }
    }

    pub(super) fn entries(&self) -> Vec<SessionEntry> {
        let manager = self.session.session_manager();
        if let Some(disk) = manager.disk() {
            disk.lock().get_branch(None).into_iter().cloned().collect()
        } else {
            manager
                .entries()
                .into_iter()
                .filter_map(|entry| serde_json::from_value(entry.data).ok())
                .collect()
        }
    }

    pub(super) fn refresh_catalog(
        &self,
        work: &super::Work,
        events: UnboundedSender<UiEvent>,
        offline: bool,
    ) {
        if offline {
            return;
        }
        let Some(catalog) = self.catalog.clone() else {
            return;
        };
        let generation = self.generation;
        let session = self.session.clone();
        work.push(Box::pin(async move {
            let signal = pi_rs_agent::CancellationToken::new();
            if tokio::time::timeout(
                std::time::Duration::from_secs(15),
                catalog.refresh(true, &signal),
            )
            .await
            .is_ok_and(|result| result.is_ok())
                && let Ok(models) = catalog.models().get_available(None, &signal).await
            {
                let existing = session.available_models();
                let refreshed = models
                    .into_iter()
                    .filter(|model| {
                        existing
                            .iter()
                            .any(|previous| previous.provider == model.provider)
                    })
                    .collect();
                let _ = events.send(UiEvent::Models(generation, refreshed));
            }
        }));
    }
}

#[derive(Debug)]
pub(super) enum Transition {
    New(Option<String>),
    Resume(PathBuf),
    Fork(String, ForkPosition),
    Clone,
    Reload,
    Import(PathBuf),
}

pub(super) struct Controller {
    runtime: Mutex<AgentSessionRuntime>,
    active: RefCell<(Bound, Option<usize>)>,
    pub(super) events: UnboundedSender<UiEvent>,
}

impl Controller {
    pub(super) fn new(runtime: AgentSessionRuntime, events: UnboundedSender<UiEvent>) -> Rc<Self> {
        let active = RefCell::new((Bound::new(&runtime, 0), None));
        Rc::new(Self {
            runtime: Mutex::new(runtime),
            active,
            events,
        })
    }

    pub(super) fn active(&self) -> Bound {
        self.active.borrow().0.clone()
    }

    pub(super) fn host(&self, generation: Option<u64>) -> Arc<UiHost> {
        Arc::new(UiHost {
            generation,
            events: self.events.clone(),
        })
    }

    pub(super) fn notice(&self, text: impl Into<String>, color: &'static str) {
        let _ = self.events.send(UiEvent::Notice(text.into(), color));
    }

    fn stop_subscribing(&self) {
        let mut old = self.active.borrow_mut();
        if let Some(listener) = old.1.take() {
            old.0.session.unsubscribe(listener);
        }
    }

    pub(super) async fn bind(&self, bound: Bound, reason: &str, previous_file: Option<String>) {
        self.stop_subscribing();
        let events = self.events.clone();
        let generation = bound.generation;
        let subscribed = bound.session.subscribe(Arc::new(move |event| {
            let _ = events.send(UiEvent::Session(generation, Box::new(event.clone())));
        }));
        *self.active.borrow_mut() = (bound.clone(), Some(subscribed));
        if let Some(native) = bound.extensions.native() {
            native.set_ui(ExtensionMode::Tui, true);
            let events = self.events.clone();
            native.host.on_error(Arc::new(move |error| {
                let _ = events.send(UiEvent::Notice(
                    format!(
                        "Extension {} ({}): {}",
                        error.extension_path, error.event, error.error
                    ),
                    "error",
                ));
            }));
            let mut data = json!({ "type": "session_start", "reason": reason });
            if let Some(path) = previous_file {
                data["previousSessionFile"] = json!(path);
            }
            native.emit(ExtensionEvent::SessionStart, data).await;
            native.report_unhandled_mcp_servers();
        }
    }

    pub(super) async fn close(&self) {
        let mut runtime = self.runtime.lock().await;
        self.stop_subscribing();
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

    pub(super) async fn transition(&self, transition: Transition) -> Result<Value, String> {
        let mut runtime = self.runtime.lock().await;
        let old = self.active();
        let session = &old.session;
        let native = old.extensions.native();
        let previous_file = session.session_file();
        let cancelled = || json!({ "cancelled": true });
        if matches!(transition, Transition::Reload) && !session.is_idle() {
            return Err(
                "Wait for the current response or compaction to finish before reloading.".into(),
            );
        }
        let flags = if matches!(transition, Transition::Reload) {
            native.map(|runner| runner.get_flag_values())
        } else {
            None
        };
        let (manager, reason, editor) = match transition {
            Transition::New(parent) => {
                if before_switch(native, "new", None).await {
                    return Ok(cancelled());
                }
                (
                    session::new_session(session, parent.as_deref())?,
                    SessionReason::New,
                    None,
                )
            }
            Transition::Resume(path) => {
                let path = path.to_str().ok_or("Session path is not valid UTF-8")?;
                if before_switch(native, "resume", Some(path)).await {
                    return Ok(cancelled());
                }
                (
                    session::switch_session(session, path)?,
                    SessionReason::Resume,
                    None,
                )
            }
            Transition::Import(path) => {
                let mut manager = SessionManager::open(&path)?;
                if before_switch(native, "resume", path.to_str()).await {
                    return Ok(cancelled());
                }
                let store = session.session_manager();
                let current_dir = store
                    .disk()
                    .map(|disk| disk.lock().get_session_dir().to_path_buf());
                if let Some(current_dir) = current_dir
                    && path.parent() != Some(current_dir.as_path())
                {
                    let name = path.file_name().ok_or("Invalid session filename")?;
                    let destination = current_dir.join(name);
                    std::fs::create_dir_all(&current_dir).map_err(|error| error.to_string())?;
                    let mut create = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&destination)
                        .map_err(|error| error.to_string())?;
                    let mut source =
                        std::fs::File::open(&path).map_err(|error| error.to_string())?;
                    if let Err(error) = std::io::copy(&mut source, &mut create) {
                        drop(create);
                        let _ = std::fs::remove_file(destination);
                        return Err(error.to_string());
                    }
                    manager = SessionManager::open_with(destination, Some(current_dir), None)?;
                }
                (
                    Arc::new(DiskSession::new(manager))
                        as Arc<dyn crate::core::agent_session::SessionManager>,
                    SessionReason::Resume,
                    None,
                )
            }
            fork @ (Transition::Fork(..) | Transition::Clone) => {
                let (id, position) = match fork {
                    Transition::Fork(id, position) => (id, position),
                    _ => (
                        session::leaf_id(session)
                            .ok_or("Cannot clone session: no current entry selected")?,
                        ForkPosition::At,
                    ),
                };
                if let Some(native) = native {
                    let event = json!({ "type": "session_before_fork", "entryId": id, "position": if position == ForkPosition::At { "at" } else { "before" } });
                    if native
                        .bus
                        .emit_first(ExtensionEvent::SessionBeforeFork, event, is_cancelled)
                        .await
                        .is_some()
                    {
                        return Ok(cancelled());
                    }
                }
                session.abort().await;
                let (manager, text) = session::fork_session(session, &id, position)?;
                (manager, SessionReason::Fork, text)
            }
            Transition::Reload => (session.session_manager(), SessionReason::Reload, None),
        };
        session.abort().await;
        if let Some(native) = native {
            let mut data = json!({ "type": "session_shutdown", "reason": if reason == SessionReason::Reload { "reload" } else { "other" } });
            if let Some(path) = manager.session_file() {
                data["targetSessionFile"] = json!(path);
            }
            native.emit(ExtensionEvent::SessionShutdown, data).await;
            native.invalidate();
        }
        runtime
            .replace_session(manager, reason)
            .await
            .map_err(|error| error.to_string())?;
        let bound = Bound::new(&runtime, old.generation + 1);
        let diagnostics = runtime.take_unreported_diagnostics();
        if let Some(native) = bound.extensions.native() {
            if let Some(flags) = &flags {
                native.host.restore_flag_values(flags);
            }
            native.bind_provider_registrations();
            native.bind_virtual_models();
            native.refresh_tools();
        }
        drop(runtime);
        let _ = self.events.send(UiEvent::Bound(bound.clone(), diagnostics));
        if let Some(editor) = &editor {
            let _ = self.events.send(UiEvent::ExtensionUpdate(
                None,
                crate::core::extensions::types::UiUpdate::EditorText(editor.clone()),
            ));
        }
        self.bind(bound.clone(), reason.as_str(), previous_file)
            .await;
        if reason == SessionReason::Reload
            && let Some(native) = bound.extensions.native()
        {
            native.emit_resources_discover("reload").await;
        }
        if reason == SessionReason::Reload {
            self.notice(
                "Reloaded keybindings, extensions, skills, prompts, themes, and context files",
                "muted",
            );
        }
        if reason == SessionReason::New {
            self.notice("Started a new session", "muted");
        }
        Ok(json!({ "cancelled": false, "text": editor }))
    }

    pub(super) async fn prompt(
        &self,
        bound: Bound,
        text: String,
        images: Vec<ImageContent>,
        follow_up: bool,
    ) {
        if let Err(error) = self.try_prompt(bound, text, images, follow_up).await {
            self.notice(error, "error");
        }
    }

    async fn try_prompt(
        &self,
        bound: Bound,
        mut text: String,
        mut images: Vec<ImageContent>,
        follow_up: bool,
    ) -> Result<(), String> {
        let session = &bound.session;
        if let Some(native) = bound.extensions.native() {
            if let Some(command) = crate::core::slash_commands::parse_slash_command(&text)
                && native.get_command(&command.name).is_some()
            {
                return native
                    .execute_command(
                        &command.name,
                        &command.args,
                        &self.command_context(&bound, native),
                    )
                    .await;
            }
            if native.has_handlers(ExtensionEvent::Input) {
                let wire_images = (!images.is_empty()).then(|| json!(images));
                let behavior =
                    session
                        .is_streaming()
                        .then_some(if follow_up { "followUp" } else { "steer" });
                let mut result = native
                    .emit_input(&text, wire_images, "interactive", behavior)
                    .await;
                if result["action"].as_str() == Some("handled") {
                    return Ok(());
                }
                if let Some(replacement) = result["text"].as_str() {
                    text = replacement.to_owned();
                }
                if let Some(value) = result
                    .as_object_mut()
                    .and_then(|result| result.shift_remove("images"))
                {
                    images = serde_json::from_value(value).map_err(|error| error.to_string())?;
                }
            }
        }
        if let Some(resources) = bound.extensions.resources() {
            if let Some(skill) = text.strip_prefix("/skill:") {
                let (name, args) = skill.split_once(char::is_whitespace).unwrap_or((skill, ""));
                if let Some(skill) = resources.skills.iter().find(|skill| skill.name == name) {
                    let content = std::fs::read_to_string(&skill.file_path)
                        .map_err(|error| error.to_string())?;
                    let body = strip_frontmatter(&content).unwrap_or(content);
                    text = format!(
                        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>\n\n{}",
                        skill.name,
                        skill.file_path.display(),
                        skill.base_dir.display(),
                        body.trim(),
                        args.trim()
                    );
                }
            } else if text.starts_with('/') && !resources.prompts.is_empty() {
                text = expand_prompt_template(&text, &resources.prompts);
            }
        }
        let behavior = if follow_up {
            StreamingBehavior::FollowUp
        } else {
            StreamingBehavior::Steer
        };
        session
            .prompt(
                text,
                PromptOptions {
                    streaming_behavior: Some(behavior),
                    expand_prompt_templates: true,
                    source: "interactive".into(),
                    images,
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(super) fn command_context(
        &self,
        bound: &Bound,
        native: &ExtensionRunner,
    ) -> ExtensionCommandContext {
        let session = &bound.session;
        let idle = session.clone();
        let pending = session.clone();
        let usage = session.clone();
        let system = session.clone();
        let abort = session.clone();
        let compact = session.clone();
        let wait = session.clone();
        let navigate = session.clone();
        let shutdown = self.events.clone();
        let new_tx = self.events.clone();
        let fork_tx = self.events.clone();
        let resume_tx = self.events.clone();
        let reload_tx = self.events.clone();
        let refresh = self.events.clone();
        let generation = bound.generation;
        let cwd = session.cwd();
        ExtensionCommandContext {
            base: ExtensionContext {
                host: native.host.clone(),
                cwd,
                mode: ExtensionMode::Tui,
                ui: ExtensionUi::with_host(ExtensionMode::Tui, self.host(Some(generation))),
                bus: Some(native.bus.clone()),
                is_idle: Arc::new(move || idle.is_idle()),
                is_project_trusted: {
                    let trusted = bound.trusted_project.unwrap_or(false);
                    Arc::new(move || trusted)
                },
                has_pending_messages: Arc::new(move || {
                    !pending.peek_steering().is_empty() || !pending.peek_follow_up().is_empty()
                }),
                get_context_usage: Arc::new(move || {
                    let usage = usage.stats().context_usage?;
                    Some(ContextUsage {
                        tokens: usage.tokens.and_then(|n| i64::try_from(n).ok()),
                        context_window: i64::try_from(usage.context_window).ok()?,
                        percent: usage.percent,
                    })
                }),
                get_system_prompt: Arc::new(move || system.system_prompt()),
                abort: Arc::new(move || {
                    let session = abort.clone();
                    tokio::spawn(async move {
                        session.abort().await;
                    });
                }),
                shutdown: Arc::new(move || {
                    let _ = shutdown.send(UiEvent::Quit);
                }),
                compact: Arc::new(move |options| {
                    let session = compact.clone();
                    tokio::spawn(async move {
                        let _ = session
                            .compact(
                                options
                                    .as_ref()
                                    .and_then(|o| o["customInstructions"].as_str()),
                            )
                            .await;
                    });
                }),
            },
            wait_for_idle: Arc::new(move || {
                let session = wait.clone();
                Box::pin(async move { session.wait_for_idle().await })
            }),
            new_session: Arc::new(move |options| {
                let parent = options
                    .as_ref()
                    .and_then(|o| o["parentSession"].as_str())
                    .map(str::to_owned);
                Box::pin(request(new_tx.clone(), Transition::New(parent)))
            }),
            fork: Arc::new(move |id, options| {
                let position =
                    if options.as_ref().and_then(|o| o["position"].as_str()) == Some("at") {
                        ForkPosition::At
                    } else {
                        ForkPosition::Before
                    };
                Box::pin(request(fork_tx.clone(), Transition::Fork(id, position)))
            }),
            navigate_tree: Arc::new(move |id, _| {
                let session = navigate.clone();
                let refresh = refresh.clone();
                Box::pin(async move {
                    session.abort().await;
                    session
                        .navigate_tree(&id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let _ = refresh.send(UiEvent::Restore(generation));
                    Ok(json!({ "cancelled": false }))
                })
            }),
            switch_session: Arc::new(move |path, _| {
                Box::pin(request(resume_tx.clone(), Transition::Resume(path.into())))
            }),
            reload: Arc::new(move || {
                let this = reload_tx.clone();
                Box::pin(async move { request(this, Transition::Reload).await.map(|_| ()) })
            }),
        }
    }
}

async fn request(
    events: UnboundedSender<UiEvent>,
    transition: Transition,
) -> Result<Value, String> {
    let (answer, receive) = oneshot::channel();
    events
        .send(UiEvent::Transition(transition, Some(answer)))
        .map_err(|_| "Interactive session is closed".to_owned())?;
    receive
        .await
        .map_err(|_| "Interactive session is closed".to_owned())?
}

fn is_cancelled(value: &Value) -> bool {
    value["cancel"].as_bool() == Some(true)
}

async fn before_switch(
    native: Option<&Arc<ExtensionRunner>>,
    reason: &str,
    path: Option<&str>,
) -> bool {
    let Some(native) = native else {
        return false;
    };
    let mut data = json!({ "type": "session_before_switch", "reason": reason });
    if let Some(path) = path {
        data["targetSessionFile"] = json!(path);
    }
    native
        .emit_session_before_switch(data)
        .await
        .is_some_and(|value| is_cancelled(&value))
}

pub(super) fn resolve_path(path: &str, bound: &Bound) -> Result<PathBuf, String> {
    crate::config::resolve_path(path, Path::new(&bound.session.cwd()), true)
}
