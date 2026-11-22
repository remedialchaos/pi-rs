use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use pi_rs_agent::{AgentMessage, CancellationToken, QueueMode};
use pi_rs_ai::auth::{AuthCheck, AuthType};
use pi_rs_ai::{Model, ThinkingLevel};
use pi_rs_tui::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    SlashCommand,
    autocomplete::{AppliedCompletion, AutocompleteOptions},
};
use serde_json::{Value, json};

use crate::{
    cli::{auth_command, session_picker},
    core::{
        agent_session::ForkPosition,
        agent_session_services::export_to_html,
        auth_storage::AuthStorage,
        bash_executor::{BashExecutor, BashExecutorOptions, BashOutput},
        event_bus::ExtensionEvent,
        extensions::types::{ExtensionUiHost, UiAnswer, UiDialog, UiDialogOptions},
        keybindings::AppKeybinding,
        model_resolver::{parse_thinking_level, resolve_model_scope_from_models},
        project_trust::{
            ProjectTrustStore, format_project_trust_prompt, get_project_trust_options,
        },
        session_manager::SessionManager,
        slash_commands::{BUILTIN_SLASH_COMMANDS, parse_slash_command},
    },
    modes::rpc::session,
};
use pi_rs_tui::{EditorComponent, EditorHistory};

use super::{
    UiEvent, Work,
    controller::{Bound, Controller, Transition, resolve_path},
    frontend::Frontend,
    screen,
    selectors::{
        self, ModelPicker, ModelPickerOptions, ModelSelection, Picker, ScopedModelsAction,
        ScopedModelsPicker, SessionPicker, SettingChange, SettingsPicker,
    },
    utilities::{self, BugReportOptions, ShareDestination},
};

pub(super) enum Choice {
    Model(ModelSelection, Arc<Vec<Model>>),
    Scoped(ScopedModelsAction),
    Setting(Option<SettingChange>),
    Session(Option<PathBuf>),
    Tree(selectors::TreeAction),
    Fork(Option<String>),
    Thinking(Option<ThinkingLevel>),
    Theme(Option<String>),
    Trust(Option<usize>),
    Auth(Option<String>, bool),
}

const EXTRA_COMMANDS: &[(&str, &str)] = &[
    ("help", "Show commands"),
    ("theme", "Change the color theme"),
    ("clear", "Clear the displayed transcript"),
    ("exit", "Quit pi"),
    ("debug", "Show interactive diagnostics"),
    ("arminsayshi", "Show Armin"),
    ("dementedelves", "Explore Demented Elves"),
];

pub(super) fn autocomplete(bound: &Bound) -> impl AutocompleteProvider + use<> {
    let make = |name, description, argument_hint| SlashCommand {
        name,
        description,
        argument_hint,
        get_argument_completions: None,
    };
    let mut commands: Vec<_> = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|cmd| {
            make(
                cmd.name.into(),
                Some(cmd.description.into()),
                cmd.argument_hint.map(str::to_owned),
            )
        })
        .collect();
    commands.extend(
        EXTRA_COMMANDS
            .iter()
            .map(|&(name, description)| make(name.into(), Some(description.into()), None)),
    );
    if let Some(native) = bound.extensions.native() {
        commands.extend(
            native
                .get_registered_commands()
                .into_iter()
                .filter(|cmd| !is_builtin(&cmd.invocation_name))
                .map(|cmd| make(cmd.invocation_name, cmd.definition.description, None)),
        );
    }
    if let Some(resources) = bound.extensions.resources() {
        commands.extend(
            resources
                .prompts
                .iter()
                .filter(|prompt| !is_builtin(&prompt.name))
                .map(|prompt| make(prompt.name.clone(), Some(prompt.description.clone()), None)),
        );
        commands.extend(resources.skills.iter().map(|skill| {
            make(
                format!("skill:{}", skill.name),
                Some(skill.description.clone()),
                None,
            )
        }));
    }
    EditorAutocomplete(CombinedAutocompleteProvider::new(
        commands,
        bound.session.cwd().into(),
        None,
    ))
}

struct EditorAutocomplete(CombinedAutocompleteProvider);

impl AutocompleteProvider for EditorAutocomplete {
    fn get_suggestions(
        &self,
        lines: &[String],
        row: usize,
        col: usize,
        options: AutocompleteOptions<'_>,
    ) -> Option<AutocompleteSuggestions> {
        self.0.get_suggestions(lines, row, col, options)
    }

    fn apply_completion(
        &self,
        lines: Vec<String>,
        row: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion {
        // A completion is asynchronous. Enter can arrive after more input but
        // before the old menu is replaced. Keep the submitted text in that case.
        if lines
            .get(row)
            .and_then(|line| line.get(..col))
            .is_some_and(|before| before.ends_with(prefix))
        {
            self.0.apply_completion(lines, row, col, item, prefix)
        } else {
            AppliedCompletion {
                lines,
                cursor_line: row,
                cursor_col: col,
            }
        }
    }

    fn should_trigger_file_completion(&self, lines: &[String], row: usize, col: usize) -> bool {
        self.0.should_trigger_file_completion(lines, row, col)
    }
}

fn is_builtin(name: &str) -> bool {
    BUILTIN_SLASH_COMMANDS.iter().any(|cmd| cmd.name == name)
        || EXTRA_COMMANDS.iter().any(|&(cmd, _)| cmd == name)
}

pub(super) fn submit(
    ui: &mut Frontend,
    text: String,
    follow: bool,
    controller: &Rc<Controller>,
    work: &Work,
) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() && ui.images.is_empty() {
        return Ok(());
    }
    if text == "/quit" || text == "/exit" {
        return send(ui, UiEvent::Quit);
    }
    if ui.transitioning {
        ui.editor.borrow_mut().editor.set_text(text);
        return Err("Wait for the session change to finish.".into());
    }
    if let Some(command) = parse_slash_command(text) {
        let arg = command.args.as_str();
        match command.name.as_str() {
            "help" => {
                let text = BUILTIN_SLASH_COMMANDS
                    .iter()
                    .map(|cmd| (cmd.name, cmd.description))
                    .chain(EXTRA_COMMANDS.iter().copied())
                    .map(|(name, description)| format!("/{name}  {description}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.notice(text, "text");
                return Ok(());
            }
            "settings" => show_settings(ui),
            "scoped-models" => show_scoped(ui),
            "model" => show_model(ui, arg, controller, work),
            "thinking" => show_thinking(ui, arg)?,
            "theme" => show_theme(ui, arg)?,
            "session" => show_session(ui),
            "resume" => show_resume(ui)?,
            "new" => send(ui, UiEvent::Transition(Transition::New(None), None))?,
            "fork" => show_fork(ui),
            "tree" => show_tree(ui),
            "clone" => send(ui, UiEvent::Transition(Transition::Clone, None))?,
            "compact" => {
                if !ui.bound.session.is_idle() {
                    return Err("Wait for the current response or compaction to finish.".into());
                }
                let session = ui.bound.session.clone();
                let sender = ui.send.clone();
                let arg = (!arg.is_empty()).then(|| arg.to_owned());
                work.push(Box::pin(async move {
                    if let Err(error) = session.compact(arg.as_deref()).await {
                        let _ = sender.send(UiEvent::Notice(error.to_string(), "error"));
                    }
                }));
            }
            "reload" => {
                if !ui.bound.session.is_idle() {
                    return Err(
                        "Wait for the current response or compaction to finish before reloading."
                            .into(),
                    );
                }
                ui.status.borrow_mut().show("Reloading keybindings, extensions, skills, prompts, themes, and context files...", false);
                ui.render();
                send(ui, UiEvent::Transition(Transition::Reload, None))?;
            }
            "export" => export(ui, path_arg(arg))?,
            "import" => {
                let filename = path_arg(arg).ok_or("Usage: /import <path.jsonl>")?;
                let path = resolve_path(filename, &ui.bound)?;
                let ui_host = controller.host(None);
                let send = ui.send.clone();
                work.push(Box::pin(async move {
                    if confirm(
                        ui_host.as_ref(),
                        "Import session",
                        &format!("Replace current session with {}?", path.display()),
                    )
                    .await
                    {
                        let _ = send.send(UiEvent::Transition(Transition::Import(path), None));
                    }
                }));
            }
            "share" => share(ui, controller, work),
            "bug" => bug(ui, arg, controller, work),
            "copy" => copy(ui)?,
            "clear" => {
                ui.transcript.borrow_mut().clear();
                ui.render();
            }
            "login" => {
                if arg.is_empty() {
                    show_auth(ui, false)?;
                } else {
                    start_auth(ui, arg, false, controller, work)?;
                }
            }
            "logout" => {
                if arg.is_empty() {
                    show_auth(ui, true)?;
                } else {
                    start_auth(ui, arg, true, controller, work)?;
                }
            }
            "trust" => show_trust(ui),
            "hotkeys" => show_hotkeys(ui),
            "name" => {
                let name = session::set_name(&ui.bound.session, arg)?;
                send(
                    ui,
                    UiEvent::Session(
                        ui.bound.generation,
                        Box::new(
                            crate::core::agent_session::AgentSessionEvent::SessionInfoChanged {
                                name: name.clone(),
                            },
                        ),
                    ),
                )?;
                if let Some(name) = name {
                    ui.notice(format!("Session name: {name}"), "muted");
                }
            }
            "changelog" => {
                let text = include_str!("../../../../../../packages/coding-agent/CHANGELOG.md");
                let start = text.find("\n## ").map_or(0, |start| start + 1);
                let end = text[start..]
                    .match_indices("\n## ")
                    .nth(2)
                    .map_or(text.len(), |(end, _)| start + end);
                ui.notice(text[start..end].into(), "text");
            }
            "debug" => {
                let native = ui.bound.extensions.native();
                ui.notice(
                    format!(
                        "Theme: {}\nSession: {}\nExtensions: {}\n",
                        ui.theme.name(),
                        ui.bound.session.session_id(),
                        native.map_or(0, |runner| runner.extension_paths().len())
                    ),
                    "muted",
                );
            }
            "arminsayshi" => ui.show_armin(),
            "dementedelves" => {
                ui.show_decoration(utilities::EarendilAnnouncement::new(ui.theme.clone()))
            }
            _ => {
                start_prompt(ui, text, follow, controller, work);
                return Ok(());
            }
        }
        return Ok(());
    }
    if let Some(command) = text.strip_prefix('!') {
        let exclude = command.starts_with('!');
        let command = if exclude {
            command[1..].trim()
        } else {
            command.trim()
        };
        if !command.is_empty() {
            return bash(ui, command, exclude, work);
        }
    }
    start_prompt(ui, text, follow, controller, work);
    Ok(())
}

fn start_prompt(
    ui: &mut Frontend,
    text: &str,
    follow: bool,
    controller: &Rc<Controller>,
    work: &Work,
) {
    let mut editor = ui.editor.borrow_mut();
    editor.editor.add_to_history(text);
    let text = text.to_owned();
    let images = std::mem::take(&mut ui.images);
    let bound = ui.bound.clone();
    let controller = controller.clone();
    if bound.session.is_compacting()
        && !bound.extensions.native().is_some_and(|runner| {
            parse_slash_command(&text).is_some_and(|cmd| runner.get_command(&cmd.name).is_some())
        })
    {
        work.push(Box::pin(async move {
            while bound.session.is_compacting() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            controller.prompt(bound, text, images, follow).await;
        }));
    } else {
        work.push(Box::pin(async move {
            controller.prompt(bound, text, images, follow).await;
        }));
    }
}

pub(super) fn key(
    ui: &mut Frontend,
    action: AppKeybinding,
    controller: &Rc<Controller>,
    work: &Work,
) -> Result<(), String> {
    use AppKeybinding as Key;
    match action {
        Key::Interrupt => {
            if ui.bash.is_some() {
                ui.cancel_bash();
            } else if ui.auth.is_some() {
                ui.cancel_auth();
            } else if !ui.bound.session.is_idle() {
                restore_queue(ui);
                let session = ui.bound.session.clone();
                work.push(Box::pin(async move {
                    session.abort().await;
                }));
            } else if ui.editor.borrow().editor.text().is_empty() {
                let now = Instant::now();
                if ui
                    .last_escape
                    .replace(now)
                    .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
                {
                    match ui.settings.get()["doubleEscapeAction"].as_str() {
                        Some("tree") => show_tree(ui),
                        Some("fork") => show_fork(ui),
                        _ => {}
                    }
                    ui.last_escape = None;
                }
            }
        }
        Key::Clear => {
            let now = Instant::now();
            if ui.editor.borrow().editor.text().is_empty()
                && ui
                    .last_clear
                    .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
            {
                return send(ui, UiEvent::Quit);
            }
            ui.last_clear = Some(now);
            ui.editor.borrow_mut().editor.set_text("");
            ui.images.clear();
        }
        Key::Exit => send(ui, UiEvent::Quit)?,
        Key::Suspend => send(ui, UiEvent::Suspend)?,
        Key::ToolsExpand => {
            ui.transcript.borrow_mut().toggle_tools();
        }
        Key::ThinkingToggle => {
            let visible = ui.transcript.borrow_mut().toggle_thinking();
            ui.settings
                .set_value(&["hideThinkingBlock".into()], json!(!visible))?;
        }
        Key::ThinkingCycle => {
            let model = ui.bound.session.model();
            let levels: Vec<_> = model.supported_thinking_levels().collect();
            if !levels.is_empty() {
                let current = levels
                    .iter()
                    .position(|&it| it == ui.bound.session.thinking_level());
                let next = current.map_or(0, |index| (index + 1) % levels.len());
                ui.bound.session.set_thinking_level(levels[next]);
            }
        }
        Key::ThinkingSave => ui.settings.set_value(
            &["defaultThinkingLevel".into()],
            json!(ui.bound.session.thinking_level()),
        )?,
        Key::ModelCycleForward | Key::ModelCycleBackward => {
            let session = &ui.bound.session;
            let available = ui.available_models();
            let scoped = scope(ui, &available);
            let current = session.model();
            let models: Vec<_> = if scoped.is_empty() {
                available.into_iter().map(|model| (model, None)).collect()
            } else {
                scoped
                    .into_iter()
                    .map(|entry| (entry.model, entry.thinking_level))
                    .collect()
            };
            if !models.is_empty() {
                let len = models.len();
                let index = models
                    .iter()
                    .position(|(model, _)| same_model(model, &current))
                    .map_or(0, |index| {
                        (index
                            + if action == Key::ModelCycleForward {
                                1
                            } else {
                                len - 1
                            })
                            % len
                    });
                if let Some((model, level)) = models.into_iter().nth(index) {
                    set_model(ui, model, level, controller, work);
                }
            }
        }
        Key::ModelSelect => show_model(ui, "", controller, work),
        Key::EditorExternal => {
            let text = ui.editor.borrow().editor.expanded_text().into_owned();
            let command = ui.settings.get()["editor"].as_str().map(str::to_owned);
            ui.pause()?;
            let send = ui.send.clone();
            work.push(Box::pin(async move {
                let result = if let Some(command) = command {
                    utilities::external_editor_with_command(&command, &text).await
                } else {
                    utilities::external_editor(&text).await
                };
                let _ = send.send(UiEvent::EditorFinished(result));
            }));
        }
        Key::MessageCopy => copy(ui)?,
        Key::MessageFollowUp => {
            let mut editor = ui.editor.borrow_mut();
            let text = editor.editor.expanded_text().into_owned();
            editor.editor.set_text("");
            drop(editor);
            submit(ui, text, true, controller, work)?;
        }
        Key::MessageDequeue => restore_queue(ui),
        Key::ClipboardPasteImage => send(ui, UiEvent::Paste)?,
        Key::SessionNew => send(ui, UiEvent::Transition(Transition::New(None), None))?,
        Key::SessionTree => show_tree(ui),
        Key::SessionFork => show_fork(ui),
        Key::SessionResume => show_resume(ui)?,
        _ => {}
    }
    ui.render();
    Ok(())
}

fn restore_queue(ui: &mut Frontend) {
    let session = &ui.bound.session;
    let mut pending = session.peek_steering();
    pending.extend(session.peek_follow_up());
    if !pending.is_empty() {
        session.clear_queue();
        let mut editor = ui.editor.borrow_mut();
        if !editor.editor.text().is_empty() {
            pending.push(editor.editor.expanded_text().into_owned());
        }
        editor.editor.set_text(&pending.join("\n\n"));
    }
}

pub(super) fn shortcut(ui: &Frontend, key: String, controller: &Rc<Controller>, work: &Work) {
    let Some(native) = ui.bound.extensions.native().cloned() else {
        return;
    };
    let context = controller.command_context(&ui.bound, &native);
    let resolved = ui
        .editor
        .borrow()
        .keys
        .get_effective_config()
        .into_iter()
        .map(|(key, value)| (key, value.keys().to_vec()))
        .collect();
    let Some(shortcut) = native.get_shortcuts(&resolved).shift_remove(&key) else {
        return;
    };
    let controller = controller.clone();
    work.push(Box::pin(async move {
        if let Err(error) = native.execute_shortcut(&shortcut, &context).await {
            controller.notice(error, "error");
        }
    }));
}

fn send(ui: &Frontend, event: UiEvent) -> Result<(), String> {
    ui.send
        .send(event)
        .map_err(|_| "Interactive UI is closed".into())
}

fn show_settings(ui: &mut Frontend) {
    let send = ui.send.clone();
    let current = ui.bound.session.model();
    let models = ui.available_models();
    let picker = SettingsPicker::new(&ui.settings, ui.theme.clone(), move |change| {
        let _ = send.send(UiEvent::Choice(Choice::Setting(change)));
    })
    .with_models(models, Some((&current.provider, &current.id)));
    ui.show_component(picker);
}

fn show_scoped(ui: &mut Frontend) {
    let send = ui.send.clone();
    let models = ui.available_models();
    let enabled =
        serde_json::from_value::<Vec<String>>(ui.settings.get()["enabledModels"].clone()).ok();
    ui.show_component(ScopedModelsPicker::new(
        models,
        enabled,
        ui.theme.clone(),
        move |action| {
            let _ = send.send(UiEvent::Choice(Choice::Scoped(action)));
        },
    ));
}

fn scope(ui: &Frontend, models: &[Model]) -> Vec<crate::core::model_resolver::ScopedModel> {
    if let Ok(patterns) =
        serde_json::from_value::<Vec<String>>(ui.settings.get()["enabledModels"].clone())
    {
        return resolve_model_scope_from_models(&patterns, models).scoped_models;
    }
    ui.bound
        .session
        .scoped_models()
        .into_iter()
        .map(
            |(model, thinking_level)| crate::core::model_resolver::ScopedModel {
                model,
                thinking_level,
            },
        )
        .collect()
}

fn same_model(a: &Model, b: &Model) -> bool {
    a.id == b.id && a.provider == b.provider
}

fn show_model(ui: &mut Frontend, query: &str, controller: &Rc<Controller>, work: &Work) {
    let models = Arc::new(ui.available_models());
    let current = ui.bound.session.model();
    if !query.is_empty() {
        let mut matches = models.iter().filter(|model| {
            model.id.eq_ignore_ascii_case(query)
                || format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(query)
        });
        if let Some(model) = matches.next()
            && matches.next().is_none()
        {
            set_model(ui, model.clone(), None, controller, work);
            return;
        }
    }
    let scoped = scope(ui, &models)
        .iter()
        .filter_map(|entry| {
            models
                .iter()
                .position(|model| same_model(model, &entry.model))
        })
        .collect::<Vec<_>>();
    let send = ui.send.clone();
    let default_provider = ui.settings.get_default_provider();
    let default_model = ui.settings.get_default_model();
    let options = ModelPickerOptions {
        scoped: &scoped,
        current: Some(&current),
        default: default_provider.as_deref().zip(default_model.as_deref()),
        query,
    };
    let shared = models.clone();
    ui.show_component(ModelPicker::new(
        &models,
        options,
        ui.theme.clone(),
        move |choice| {
            let _ = send.send(UiEvent::Choice(Choice::Model(choice, shared.clone())));
        },
    ));
}

fn set_model(
    ui: &Frontend,
    model: Model,
    level: Option<ThinkingLevel>,
    controller: &Rc<Controller>,
    work: &Work,
) {
    let session = ui.bound.session.clone();
    let controller = controller.clone();
    work.push(Box::pin(async move {
        let label = format!("{}/{}", model.provider, model.id);
        let level = level.map(|level| model.clamp_thinking_level(level));
        match session.set_model(model).await {
            Ok(()) => {
                if let Some(level) = level {
                    session.set_thinking_level(level);
                }
                controller.notice(format!("Model: {label}"), "success");
            }
            Err(error) => controller.notice(error.to_string(), "error"),
        }
    }));
}

fn show_thinking(ui: &mut Frontend, arg: &str) -> Result<(), String> {
    let model = ui.bound.session.model();
    let levels: Vec<_> = model.supported_thinking_levels().collect();
    if !arg.is_empty() {
        let level = parse_thinking_level(&arg.to_ascii_lowercase())
            .ok_or_else(|| format!("Unknown thinking level: {arg}"))?;
        if !levels.contains(&level) {
            return Err(format!(
                "Model '{}' does not support thinking level {}",
                model.id,
                level.as_str()
            ));
        }
        return send(ui, UiEvent::Choice(Choice::Thinking(Some(level))));
    }
    let current = ui.bound.session.thinking_level();
    let saved = ui.settings.get()["defaultThinkingLevel"]
        .as_str()
        .and_then(parse_thinking_level);
    let send = ui.send.clone();
    let items = selectors::thinking_items(&levels, current, saved);
    let selected = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    ui.show_component(
        Picker::with_items(
            "Select thinking level".into(),
            items,
            ui.theme.clone(),
            move |level| {
                let _ = send.send(UiEvent::Choice(Choice::Thinking(level.copied())));
            },
        )
        .without_filter()
        .preselect(selected),
    );
    Ok(())
}

fn show_theme(ui: &mut Frontend, arg: &str) -> Result<(), String> {
    if !arg.is_empty() {
        return send(ui, UiEvent::Choice(Choice::Theme(Some(arg.to_owned()))));
    }
    let send = ui.send.clone();
    ui.show_component(Picker::with_items(
        "Select theme".into(),
        selectors::theme_items(&ui.theme.names(), &ui.theme.name()),
        ui.theme.clone(),
        move |theme| {
            let _ = send.send(UiEvent::Choice(Choice::Theme(theme.cloned())));
        },
    ));
    Ok(())
}

fn session_directory(ui: &Frontend) -> Option<PathBuf> {
    ui.settings.get_session_dir().map(PathBuf::from)
}

fn show_resume(ui: &mut Frontend) -> Result<(), String> {
    let cwd = PathBuf::from(ui.bound.session.cwd());
    let dir = session_directory(ui);
    let agent_dir = ui.options.agent_dir.clone();
    let current = session_picker::list_current_sessions(&cwd, dir.clone())?;
    let current_file = ui.bound.session.session_file();
    let send = ui.send.clone();
    ui.show_component(
        SessionPicker::new(
            current,
            move || session_picker::list_all_sessions(&agent_dir, dir.as_deref()),
            ui.theme.clone(),
            move |session| {
                let _ = send.send(UiEvent::Choice(Choice::Session(
                    session.map(|session| session.path.clone()),
                )));
            },
        )
        .with_current_path(current_file.as_deref().map(Path::new))
        .with_rename(|path, name| {
            SessionManager::open(path)
                .and_then(|mut manager| manager.append_session_info(name).map(|_| ()))
        }),
    );
    Ok(())
}

fn all_entries(bound: &Bound) -> Vec<crate::core::session_manager::SessionEntry> {
    let store = bound.session.session_manager();
    store.disk().map_or_else(
        || bound.entries(),
        |disk| disk.lock().get_entries().to_vec(),
    )
}

fn show_tree(ui: &mut Frontend) {
    let send = ui.send.clone();
    let active = session::leaf_id(&ui.bound.session);
    let filter = selectors::TreeFilter::from_setting(
        ui.settings.get()["treeFilterMode"]
            .as_str()
            .unwrap_or("default"),
    );
    ui.show_component(selectors::TreePicker::new(
        &all_entries(&ui.bound),
        active.as_deref(),
        filter,
        ui.theme.clone(),
        move |action| {
            let _ = send.send(UiEvent::Choice(Choice::Tree(action)));
        },
    ));
}

fn show_fork(ui: &mut Frontend) {
    let send = ui.send.clone();
    ui.show_component(Picker::with_items(
        "Fork from a previous user message".into(),
        selectors::user_message_rows(ui.bound.entries().iter()),
        ui.theme.clone(),
        move |id| {
            let _ = send.send(UiEvent::Choice(Choice::Fork(id.cloned())));
        },
    ));
}

fn show_session(ui: &mut Frontend) {
    let s = ui.bound.session.stats();
    let name = super::chrome::session_name(&ui.bound.session)
        .map_or_else(String::new, |name| format!("\nName: {name}"));
    let path = s.session_file.as_deref().unwrap_or("(not saved)");
    ui.notice(format!("Session: {}{name}\nFile: {path}\nUser messages: {}\nAssistant messages: {}\nTool calls: {}\nTool results: {}\nTokens: {} input, {} output\nCost: ${:.3}", s.session_id, s.user_messages, s.assistant_messages, s.tool_calls, s.tool_results, s.tokens.input, s.tokens.output, s.cost), "text");
}

fn show_hotkeys(ui: &mut Frontend) {
    let keys = ui.editor.borrow().keys.get_effective_config();
    let lines = keys
        .iter()
        .filter(|(_, keys)| !keys.keys().is_empty())
        .map(|(action, keys)| format!("{}  {action}", keys.keys().join(" / ")))
        .collect::<Vec<_>>()
        .join("\n");
    ui.notice(lines, "text");
}

fn show_trust(ui: &mut Frontend) {
    let cwd = PathBuf::from(ui.bound.session.cwd());
    let labels = get_project_trust_options(&cwd, false)
        .into_iter()
        .map(|option| option.label)
        .collect();
    let send = ui.send.clone();
    ui.show_component(
        Picker::new(
            format_project_trust_prompt(&cwd),
            labels,
            ui.theme.clone(),
            move |choice| {
                let _ = send.send(UiEvent::Choice(Choice::Trust(choice)));
            },
        )
        .without_filter(),
    );
}

fn show_auth(ui: &mut Frontend, logout: bool) -> Result<(), String> {
    let credentials = AuthStorage::create(ui.options.agent_dir.join("auth.json")).list();
    let items = if logout {
        let mut credentials = credentials;
        credentials.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        credentials
            .into_iter()
            .map(|credential| {
                selectors::PickerItem::new(
                    format!(
                        "{} · {}",
                        credential.provider_id,
                        selectors::auth_provider_type(credential.auth_type, None)
                    ),
                    credential.provider_id,
                )
            })
            .collect::<Vec<_>>()
    } else {
        let mut providers = auth_command::auth_providers(&ui.options.agent_dir)?;
        providers.sort_by(|a, b| a.name.cmp(&b.name));
        let configured: std::collections::HashMap<_, _> = credentials
            .into_iter()
            .map(|credential| {
                (
                    credential.provider_id,
                    AuthCheck {
                        source: Some("stored credential".into()),
                        auth_type: credential.auth_type,
                    },
                )
            })
            .collect();
        let mut methods = Vec::new();
        for provider in &providers {
            let status = configured.get(&provider.id);
            if let Some(method) = &provider.auth.oauth {
                methods.push(selectors::AuthPickerProvider {
                    id: &provider.id,
                    name: &provider.name,
                    method_name: Some(method.name()),
                    auth_type: AuthType::OAuth,
                    status,
                    subscription: Some(method.is_subscription()),
                });
            }
            if let Some(method) = &provider.auth.api_key
                && method.interactive()
            {
                methods.push(selectors::AuthPickerProvider {
                    id: &provider.id,
                    name: &provider.name,
                    method_name: Some(method.name()),
                    auth_type: AuthType::ApiKey,
                    status,
                    subscription: None,
                });
            }
        }
        selectors::auth_provider_items(&methods)
            .into_iter()
            .map(|row| selectors::PickerItem {
                label: row.label,
                search: row.search,
                value: match row.value {
                    (id, AuthType::OAuth) => id,
                    (id, AuthType::ApiKey) => format!("{id} --api-key"),
                },
            })
            .collect()
    };
    if items.is_empty() {
        return Err("No configured providers.".into());
    }
    let send = ui.send.clone();
    ui.show_component(Picker::with_items(
        if logout {
            "Log out of provider".into()
        } else {
            "Log in to provider".into()
        },
        items,
        ui.theme.clone(),
        move |provider| {
            let _ = send.send(UiEvent::Choice(Choice::Auth(provider.cloned(), logout)));
        },
    ));
    Ok(())
}

fn start_auth(
    ui: &mut Frontend,
    provider: &str,
    logout: bool,
    controller: &Rc<Controller>,
    work: &Work,
) -> Result<(), String> {
    if ui.auth.is_some() || !ui.bound.session.is_idle() {
        return Err("Wait for the current operation before changing authentication.".into());
    }
    let cwd = PathBuf::from(ui.bound.session.cwd());
    let dir = ui.options.agent_dir.clone();
    let send = ui.send.clone();
    let provider = provider.to_owned();
    let offline = ui.options.offline;
    let signal = CancellationToken::new();
    ui.auth = Some(signal.clone());
    let login_ui =
        super::auth_ui::LoginUi(controller.host(Some(ui.bound.generation)).as_ref().clone());
    if logout {
        ui.pause()?;
    }
    work.push(Box::pin(async move {
        let code = if logout {
            let argv = vec!["auth".into(), "logout".into(), provider];
            auth_command::run(&argv, &cwd, &dir).await.unwrap_or(1)
        } else {
            let args: Vec<_> = provider.split_whitespace().collect();
            crate::cli::auth_login::run_with_terminal(&args, &cwd, &dir, offline, signal, &login_ui)
                .await
        };
        let _ = send.send(UiEvent::AuthFinished(code));
    }));
    Ok(())
}

pub(super) fn chosen(
    ui: &mut Frontend,
    choice: Choice,
    controller: &Rc<Controller>,
    work: &Work,
) -> Result<(), String> {
    match choice {
        Choice::Model(ModelSelection::Select(index), models) => {
            ui.close_selector();
            if let Some(model) = models.get(index) {
                set_model(ui, model.clone(), None, controller, work);
            }
        }
        Choice::Model(ModelSelection::SetDefault(index), models) => {
            if let Some(model) = models.get(index) {
                ui.settings
                    .set_value(&["defaultProvider".into()], json!(model.provider))?;
                ui.settings
                    .set_value(&["defaultModel".into()], json!(model.id))?;
                ui.notice(
                    format!("Default model: {}/{}", model.provider, model.id),
                    "muted",
                );
            }
        }
        Choice::Model(ModelSelection::Cancel, _)
        | Choice::Scoped(ScopedModelsAction::Cancel)
        | Choice::Setting(None) => ui.close_selector(),
        Choice::Scoped(ScopedModelsAction::Change(patterns)) => ui
            .settings
            .apply_overrides(json!({ "enabledModels": patterns })),
        Choice::Scoped(ScopedModelsAction::Persist(patterns)) => {
            ui.settings
                .set_value(&["enabledModels".into()], json!(patterns))?;
            ui.notice("Scoped model selection saved".into(), "success");
        }
        Choice::Setting(Some(SettingChange { path, value })) => {
            if path == ["theme"]
                && let Some(theme) = value.as_str()
            {
                ui.theme.select(theme).map_err(|error| error.to_string())?;
            }
            ui.settings.set_value(&path, value.clone())?;
            match path
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["compaction", "enabled"] => ui
                    .bound
                    .session
                    .set_auto_compaction_enabled(value.as_bool().unwrap_or(true)),
                ["retry", "enabled"] => ui
                    .bound
                    .session
                    .set_auto_retry_enabled(value.as_bool().unwrap_or(true)),
                ["steeringMode"] => ui.bound.session.set_steering_mode(queue_mode(&value)),
                ["followUpMode"] => ui.bound.session.set_follow_up_mode(queue_mode(&value)),
                _ => {}
            }
            ui.apply_settings()?;
        }
        Choice::Thinking(level) => {
            ui.close_selector();
            if let Some(level) = level {
                ui.bound.session.set_thinking_level(level);
                ui.settings
                    .set_value(&["defaultThinkingLevel".into()], json!(level))?;
                ui.notice(format!("Thinking level: {}", level.as_str()), "success");
            }
        }
        Choice::Theme(theme) => {
            ui.close_selector();
            if let Some(theme) = theme {
                ui.theme.select(&theme).map_err(|error| error.to_string())?;
                ui.settings.set_value(&["theme".into()], json!(theme))?;
                ui.transcript.borrow_mut().invalidate();
            }
        }
        Choice::Session(path) => {
            ui.close_selector();
            if let Some(path) = path {
                send(ui, UiEvent::Transition(Transition::Resume(path), None))?;
            }
        }
        Choice::Tree(selectors::TreeAction::Cancel) => ui.close_selector(),
        Choice::Tree(selectors::TreeAction::Copy(text)) => screen::copy(&text)?,
        Choice::Tree(selectors::TreeAction::Label(id, label)) => {
            let store = ui.bound.session.session_manager();
            let disk = store.disk().ok_or("Session does not support labels")?;
            disk.lock().append_label_change(&id, label.as_deref())?;
            ui.notice("Updated entry label".into(), "muted");
        }
        Choice::Tree(selectors::TreeAction::Select(id)) => {
            ui.close_selector();
            {
                let session = ui.bound.session.clone();
                let send = ui.send.clone();
                let generation = ui.bound.generation;
                work.push(Box::pin(async move {
                    session.abort().await;
                    let event = match session.navigate_tree(&id).await {
                        Ok(()) => UiEvent::Restore(generation),
                        Err(error) => UiEvent::Notice(error.to_string(), "error"),
                    };
                    let _ = send.send(event);
                }));
            }
        }
        Choice::Fork(id) => {
            ui.close_selector();
            if let Some(id) = id {
                send(
                    ui,
                    UiEvent::Transition(Transition::Fork(id, ForkPosition::Before), None),
                )?;
            }
        }
        Choice::Trust(index) => {
            ui.close_selector();
            let cwd = PathBuf::from(ui.bound.session.cwd());
            if let Some(option) = index.and_then(|index| {
                get_project_trust_options(&cwd, false)
                    .into_iter()
                    .nth(index)
            }) {
                ProjectTrustStore::new(&ui.options.agent_dir)
                    .set_many(&option.updates)
                    .map_err(|error| error.to_string())?;
                ui.notice("Project trust decision saved".into(), "success");
                send(ui, UiEvent::Transition(Transition::Reload, None))?;
            }
        }
        Choice::Auth(provider, logout) => {
            ui.close_selector();
            if let Some(provider) = provider {
                start_auth(ui, &provider, logout, controller, work)?;
            }
        }
    }
    ui.render();
    Ok(())
}

fn queue_mode(value: &Value) -> QueueMode {
    if value.as_str() == Some("all") {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

fn path_arg(arg: &str) -> Option<&str> {
    if arg.starts_with(['\'', '"']) {
        let quote = arg.as_bytes()[0] as char;
        arg[1..].split_once(quote).map(|(path, _)| path)
    } else {
        arg.split_whitespace().next()
    }
}

fn export(ui: &mut Frontend, output: Option<&str>) -> Result<(), String> {
    let current = &ui.bound.session;
    let store = current.session_manager();
    let disk = store.disk().ok_or("Session has no persisted history")?;
    let manager = disk.lock();
    let path = if let Some(output) = output.filter(|path| path.ends_with(".jsonl")) {
        let path = resolve_path(output, &ui.bound)?;
        let mut bytes =
            serde_json::to_vec(manager.get_header()).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        for entry in manager.get_entries() {
            serde_json::to_writer(&mut bytes, entry).map_err(|error| error.to_string())?;
            bytes.push(b'\n');
        }
        if manager.get_session_file() == Some(path.as_path()) {
            return Err("Cannot overwrite the active session with its export.".into());
        }
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        path.display().to_string()
    } else {
        let output = output
            .map(|path| resolve_path(path, &ui.bound))
            .transpose()?;
        export_to_html(
            &manager,
            output.as_ref().and_then(|path| path.to_str()),
            Some(current.system_prompt()),
        )?
    };
    ui.notice(format!("Session exported to: {path}"), "success");
    Ok(())
}

fn copy(ui: &mut Frontend) -> Result<(), String> {
    let text = ui
        .transcript
        .borrow()
        .last_assistant_text()
        .map(str::to_owned)
        .or_else(|| session::last_assistant_text(&ui.bound.session))
        .ok_or("No assistant message to copy.")?;
    screen::copy(&text)?;
    ui.notice("Copied last assistant message to clipboard".into(), "muted");
    Ok(())
}

fn bash(ui: &mut Frontend, command: &str, exclude: bool, work: &Work) -> Result<(), String> {
    if ui.bash.is_some() {
        return Err("A bash command is already running. Press Esc to cancel it first.".into());
    }
    ui.editor
        .borrow_mut()
        .editor
        .add_to_history(&format!("{}{command}", if exclude { "!!" } else { "!" }));
    let cancel = CancellationToken::new();
    ui.bash = Some(cancel.clone());
    let command = command.to_owned();
    let id = ui
        .transcript
        .borrow_mut()
        .begin_bash(command.clone(), exclude);
    ui.render();
    let send = ui.send.clone();
    let bound = ui.bound.clone();
    work.push(Box::pin(async move {
        let result = execute_bash(bound, command, exclude, cancel, id, send.clone()).await;
        let _ = send.send(UiEvent::BashEnd(id, result));
    }));
    Ok(())
}

async fn execute_bash(
    bound: Bound,
    command: String,
    exclude: bool,
    cancel: CancellationToken,
    id: u64,
    send: tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> Result<BashOutput, String> {
    let cwd = bound.session.cwd();
    let overridden = match bound.extensions.native() {
        Some(native) if native.has_handlers(ExtensionEvent::UserBash) => native.emit_user_bash(json!({"type": "user_bash", "command": command, "excludeFromContext": exclude, "cwd": cwd})).await,
        _ => None,
    };
    if overridden
        .as_ref()
        .is_some_and(|result| result.get("operations").is_some())
    {
        return Err("Custom extension Bash operations are not supported by the native host".into());
    }
    let result = if let Some(result) = overridden.as_ref().and_then(|result| result.get("result")) {
        BashOutput {
            output: result["output"].as_str().unwrap_or_default().to_owned(),
            exit_code: result["exitCode"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok()),
            cancelled: result["cancelled"].as_bool().unwrap_or(false),
            truncated: result["truncated"].as_bool().unwrap_or(false),
            full_output_path: result["fullOutputPath"].as_str().map(str::to_owned),
        }
    } else {
        let settings = bound.session.settings();
        let options = BashExecutorOptions {
            cancel: Some(cancel),
            shell_path: settings.shell_path(),
            on_chunk: Some(Arc::new(move |data| {
                let _ = send.send(UiEvent::BashChunk(id, data.to_owned()));
            })),
            ..Default::default()
        };
        let prefixed = settings
            .shell_command_prefix()
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| format!("{prefix}\n{command}"));
        BashExecutor::new()
            .execute(
                prefixed.as_deref().unwrap_or(&command),
                Path::new(&cwd),
                &options,
            )
            .await?
    };
    let mut fields = serde_json::Map::new();
    fields.insert("command".into(), json!(command));
    fields.insert("output".into(), json!(result.output));
    fields.insert(
        "timestamp".into(),
        json!(chrono::Utc::now().timestamp_millis()),
    );
    fields.insert("excludeFromContext".into(), json!(exclude));
    fields.insert("cancelled".into(), json!(result.cancelled));
    fields.insert("truncated".into(), json!(result.truncated));
    if let Some(code) = result.exit_code {
        fields.insert("exitCode".into(), json!(code));
    }
    if let Some(path) = &result.full_output_path {
        fields.insert("fullOutputPath".into(), json!(path));
    }
    let message =
        AgentMessage::custom("bashExecution", fields).map_err(|error| error.to_string())?;
    bound
        .session
        .record_bash_message(message)
        .map_err(|error| error.to_string())?;
    Ok(result)
}

async fn confirm(host: &dyn ExtensionUiHost, title: &str, prompt: &str) -> bool {
    matches!(
        host.dialog(
            UiDialog::Confirm(title.into(), prompt.into()),
            UiDialogOptions::default()
        )
        .await,
        Some(UiAnswer::Confirmed(true))
    )
}

fn share(ui: &Frontend, controller: &Rc<Controller>, work: &Work) {
    let host = controller.host(None);
    let controller = controller.clone();
    let session = ui.bound.session.clone();
    let dir = ui.options.agent_dir.clone();
    let destination =
        ShareDestination::from_setting(ui.settings.get()["sessionShareDestination"].as_str());
    if ui.options.offline {
        controller.notice("Sharing is disabled by --offline", "warning");
        return;
    }
    work.push(Box::pin(async move {
        if !confirm(
            host.as_ref(),
            "Share session",
            "Upload this session? The resulting link may be accessible to other people.",
        )
        .await
        {
            return;
        }
        let cancel = CancellationToken::new();
        match utilities::share_session(&session, &dir, destination, true, &cancel).await {
            Ok(Some(links)) => {
                controller.notice(format!("Session shared: {}", links.viewer), "success")
            }
            Ok(None) => {}
            Err(error) => controller.notice(error, "error"),
        }
    }));
}

fn bug(ui: &Frontend, hint: &str, controller: &Rc<Controller>, work: &Work) {
    let host = controller.host(None);
    let controller = controller.clone();
    let session = ui.bound.session.clone();
    let agent_dir = ui.options.agent_dir.clone();
    let settings = ui.settings.get_settings();
    let offline = ui.options.offline;
    let hint = (!hint.is_empty()).then(|| hint.to_owned());
    work.push(Box::pin(async move {
        let options = UiDialogOptions::default();
        let Some(UiAnswer::Confirmed(include_session)) = host
            .dialog(
                UiDialog::Confirm(
                    "Bug report".into(),
                    utilities::BUG_REPORT_TRANSCRIPT_NOTE.into(),
                ),
                options.clone(),
            )
            .await
        else {
            return;
        };
        let include_summary = if !offline && !include_session {
            let Some(UiAnswer::Confirmed(summary)) = host
                .dialog(
                    UiDialog::Confirm(
                        "Bug report".into(),
                        "Send session context to the model provider to generate a summary?".into(),
                    ),
                    options,
                )
                .await
            else {
                return;
            };
            summary
        } else {
            false
        };
        let signal = CancellationToken::new();
        let options = BugReportOptions {
            hint,
            include_session,
            include_summary,
        };
        let summary = if include_summary {
            match utilities::summarize_bug_report(
                &session,
                &agent_dir,
                options.hint.as_deref(),
                true,
                &signal,
            )
            .await
            {
                Ok(summary) => summary,
                Err(error) => {
                    controller.notice(error, "warning");
                    None
                }
            }
        } else {
            None
        };
        let settings =
            crate::core::settings_manager::SettingsManager::in_memory(settings, Default::default());
        let bundle = match utilities::build_bug_report(&session, &settings, &options, summary) {
            Ok(bundle) => bundle,
            Err(error) => {
                controller.notice(error, "error");
                return;
            }
        };
        let modes = if offline {
            vec!["Save ZIP".into()]
        } else {
            vec!["Upload report".into(), "Save ZIP".into()]
        };
        let Some(UiAnswer::Text(delivery)) = host
            .dialog(
                UiDialog::Select("Deliver bug report".into(), modes),
                UiDialogOptions::default(),
            )
            .await
        else {
            return;
        };
        if delivery == "Upload report" {
            match utilities::upload_bug_report(&session, &bundle, &agent_dir, true, &signal).await {
                Ok(Some(url)) => {
                    controller.notice(format!("Bug report: {url}"), "success");
                    return;
                }
                Ok(None) => return,
                Err(error) => {
                    controller.notice(format!("Upload failed: {error}"), "warning");
                    if !confirm(
                        host.as_ref(),
                        "Save bug report",
                        "Save a local ZIP instead?",
                    )
                    .await
                    {
                        return;
                    }
                }
            }
        }
        match utilities::export_bug_report(&session, &bundle, Path::new(&session.cwd())) {
            Ok(path) => {
                controller.notice(format!("Bug report saved: {}", path.display()), "success")
            }
            Err(error) => controller.notice(error, "error"),
        }
    }));
}

#[cfg(test)]
mod autocomplete_tests {
    use super::*;

    #[test]
    fn slash_completion_does_not_rewrite_text_typed_after_the_menu_opened()
    -> Result<(), &'static str> {
        let provider = EditorAutocomplete(CombinedAutocompleteProvider::new(
            vec![SlashCommand {
                name: "dialogs".into(),
                description: None,
                argument_hint: None,
                get_argument_completions: None,
            }],
            PathBuf::new(),
            None,
        ));
        let partial = vec!["/dial".into()];
        let suggestions = provider
            .get_suggestions(&partial, 0, 5, AutocompleteOptions::default())
            .ok_or("expected slash suggestions")?;
        let item = suggestions.items.first().ok_or("no slash suggestion")?;
        let finished = provider.apply_completion(partial, 0, 5, item, &suggestions.prefix);
        assert_eq!(finished.lines, ["/dialogs "]);

        let completed = vec!["/dialogs".into()];
        let stale = provider.apply_completion(completed, 0, 8, item, &suggestions.prefix);
        assert_eq!(stale.lines, ["/dialogs"]);
        assert_eq!(stale.cursor_col, 8);
        Ok(())
    }
}
