use std::{
    cell::{Cell, RefCell},
    io,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use base64::Engine;
use indexmap::IndexMap;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::{ImageContent, ImageContentKind, ThinkingLevel};
use pi_rs_tui::{
    Component, ComponentRef, Editor, EditorAppearance, EditorAutocomplete, EditorComponent,
    EditorInsertion, EditorOptions, EditorTheme, OverlayHandle, OverlayMargin, OverlayOptions,
    SizeValue, TuiMainScreenRenderState, TuiMode, keybindings::BindingKeys,
    replace_control_characters,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    config,
    core::{
        agent_session::{AgentSessionEvent, CompactionReason},
        agent_session_runtime::RuntimeDiagnostic,
        auth_guidance::no_models_available,
        extensions::types::{UiAnswer, UiDialog, UiUpdate, UiWidgetPlacement},
        keybindings::KeybindingsManager,
        project_trust::{ProjectTrustStore, has_trust_requiring_project_resources},
        settings_manager::{CreateOptions, SettingsManager},
    },
    utils::{clipboard, clipboard_image},
};

use super::{
    DialogAnswer, InteractiveOptions, UiEvent, Work,
    chrome::{
        CustomEditor, DynamicText, Footer, Status, TranscriptView, select_theme, session_name,
    },
    commands,
    controller::{Bound, Controller},
    screen::{Screen, TerminalHandle, mode},
    selectors::{InputDialog, Picker},
    theme::Theme,
    transcript::Transcript,
    utilities::ProgramStatusReporter,
};

type TextRows = Rc<RefCell<Vec<String>>>;

struct DialogEntry {
    id: u64,
    handle: OverlayHandle,
    answer: DialogAnswer,
}

pub(super) struct Frontend {
    terminal: TerminalHandle,
    screen: Screen,
    main_state: Option<TuiMainScreenRenderState>,
    rows: Rc<Cell<usize>>,
    pub(super) options: Rc<InteractiveOptions>,
    pub(super) settings: SettingsManager,
    pub(super) theme: Theme,
    _watch: Option<notify::RecommendedWatcher>,
    pub(super) bound: Bound,
    pub(super) transcript: Rc<RefCell<Transcript>>,
    pub(super) editor: Rc<RefCell<CustomEditor>>,
    footer: Rc<RefCell<Footer>>,
    pub(super) status: Rc<RefCell<Status>>,
    pending: TextRows,
    above: TextRows,
    below: TextRows,
    widgets: IndexMap<String, (UiWidgetPlacement, Vec<String>)>,
    dock: [ComponentRef; 6],
    document: ComponentRef,
    dialogs: Vec<DialogEntry>,
    next_dialog: u64,
    selector: Option<OverlayHandle>,
    pub(super) send: UnboundedSender<UiEvent>,
    pub(super) images: Vec<ImageContent>,
    pub(super) bash: Option<CancellationToken>,
    pub(super) auth: Option<CancellationToken>,
    refreshed_models: Vec<pi_rs_ai::Model>,
    pub(super) paused: bool,
    pub(super) transitioning: bool,
    pub(super) last_clear: Option<Instant>,
    pub(super) last_escape: Option<Instant>,
    status_reporter: ProgramStatusReporter,
}

impl Frontend {
    pub(super) fn new(
        terminal: TerminalHandle,
        options: Rc<InteractiveOptions>,
        bound: Bound,
        send: UnboundedSender<UiEvent>,
    ) -> Result<Self, String> {
        let settings = load_settings(&options, &bound);
        let value = settings.get();
        crate::cli::startup_ui::setup_terminal(&value);
        let theme_setting = options.theme.as_deref().or_else(|| value["theme"].as_str());
        let theme = Theme::load(&options.agent_dir, None).map_err(|error| error.to_string())?;
        register_themes(&theme, &bound);
        let theme_error = theme_setting
            .and_then(|setting| theme.select(setting).err())
            .map(|error| error.to_string());
        let watcher = send.clone();
        let watch = theme.watch(move || {
            let _ = watcher.send(UiEvent::ThemeFile);
        });
        let rows = Rc::new(Cell::new(24));
        let editor = Rc::new(RefCell::new(new_editor(
            &theme,
            rows.clone(),
            &value,
            &options,
            &send,
        )));
        let transcript = Rc::new(RefCell::new(Transcript::new(theme.clone())));
        let footer = Rc::new(RefCell::new(Footer::new(theme.clone(), &bound.session)));
        let status = Rc::new(RefCell::new(Status::new(theme.clone())));
        let pending = Rc::new(RefCell::new(Vec::new()));
        let above = Rc::new(RefCell::new(Vec::new()));
        let below = Rc::new(RefCell::new(Vec::new()));
        let document = ComponentRef::new(TranscriptView(transcript.clone()));
        let dock = [
            ComponentRef::new(DynamicText(pending.clone())),
            ComponentRef::from_shared(status.clone()),
            ComponentRef::new(DynamicText(above.clone())),
            ComponentRef::from_shared(editor.clone()),
            ComponentRef::new(DynamicText(below.clone())),
            ComponentRef::from_shared(footer.clone()),
        ];
        let screen = Screen::new(
            &terminal,
            mode(&value, options.tui_mode),
            options.agent_dir.clone(),
            &value,
            &theme,
            &send,
        );
        let mut ui = Self {
            terminal,
            screen,
            main_state: None,
            rows,
            options,
            settings,
            theme,
            _watch: watch.ok(),
            bound,
            transcript,
            editor,
            footer,
            status,
            pending,
            above,
            below,
            widgets: IndexMap::new(),
            dock,
            document,
            dialogs: Vec::new(),
            next_dialog: 0,
            selector: None,
            send,
            images: Vec::new(),
            bash: None,
            auth: None,
            refreshed_models: Vec::new(),
            paused: false,
            transitioning: false,
            last_clear: None,
            last_escape: None,
            status_reporter: ProgramStatusReporter::default(),
        };
        ui.mount();
        ui.restore();
        ui.configure();
        if !value["quietStartup"].as_bool().unwrap_or(false) {
            ui.notice(
                format!("{} v{}", config::APP_NAME, config::VERSION),
                "accent",
            );
            ui.notice(
                "/ for commands, ! for Bash, /hotkeys for shortcuts".into(),
                "muted",
            );
        }
        if ui.bound.session.model().provider == "unknown" {
            ui.notice(no_models_available(), "warning");
        }
        if let Some(error) = theme_error {
            ui.notice(error, "warning");
        }
        ui.screen
            .tui_mut()
            .start()
            .map_err(|error| error.to_string())?;
        ui.rows.set(ui.screen.tui_mut().base().terminal.height());
        ui.enable_terminal_colors();
        ui.update_title(None);
        if let Err(error) = ui.screen.tui_mut().render_now(false) {
            let _ = ui.screen.stop(true);
            return Err(error.to_string());
        }
        let status = ui
            .status_reporter
            .report(session_name(&ui.bound.session).as_deref());
        ui.report(status);
        Ok(ui)
    }

    fn mount(&mut self) {
        self.screen.mount(
            self.document.clone(),
            self.dock.clone(),
            &self.theme,
            &self.settings.get(),
        );
        self.screen
            .tui_mut()
            .base_mut()
            .set_focus(Some(self.dock[3].clone()));
    }

    pub(super) fn available_models(&self) -> Vec<pi_rs_ai::Model> {
        let mut models = self.bound.session.available_models();
        for refreshed in &self.refreshed_models {
            if let Some(current) = models
                .iter_mut()
                .find(|model| model.provider == refreshed.provider && model.id == refreshed.id)
            {
                current.clone_from(refreshed);
            } else {
                models.push(refreshed.clone());
            }
        }
        models
    }

    pub(super) fn configure(&mut self) {
        let settings = self.settings.get();
        crate::cli::startup_ui::setup_terminal(&settings);
        self.transcript.borrow_mut().configure(&settings);
        self.screen.tui_mut().base_mut().set_clear_on_shrink(
            settings["terminal"]["clearOnShrink"]
                .as_bool()
                .unwrap_or(false),
        );
        self.screen
            .tui_mut()
            .base_mut()
            .set_show_hardware_cursor(settings["showHardwareCursor"].as_bool().unwrap_or(false));
        let mut editor = self.editor.borrow_mut();
        editor.editor.set_padding_x(
            settings["editorPaddingX"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0),
        );
        editor.editor.set_autocomplete_max_visible(
            settings["autocompleteMaxVisible"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(5),
        );
        let keys = editor.keys.get_effective_config();
        let definitions = keys
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    pi_rs_tui::KeybindingDefinition {
                        default_keys: BindingKeys::Many(value.keys()),
                        description: editor
                            .keys
                            .get_definition(key)
                            .map(|definition| definition.description.to_owned()),
                    },
                )
            })
            .collect();
        pi_rs_tui::set_keybindings(pi_rs_tui::KeybindingsManager::new(
            definitions,
            Default::default(),
        ));
        let native = self.bound.extensions.native();
        editor.shortcuts = native
            .map(|runner| {
                runner
                    .get_shortcuts(
                        &keys
                            .iter()
                            .map(|(key, value)| (key.clone(), value.keys()))
                            .collect(),
                    )
                    .into_keys()
                    .collect()
            })
            .unwrap_or_default();
        editor
            .editor
            .set_autocomplete_provider(Box::new(commands::autocomplete(&self.bound)));
        let theme = self.theme.clone();
        let level = self.bound.session.thinking_level();
        editor
            .editor
            .set_border_color(Box::new(move |text| theme.fg(thinking_border(level), text)));
    }

    pub(super) fn restore(&mut self) {
        let mut transcript = self.transcript.borrow_mut();
        transcript.set_extensions(self.bound.extensions.native().cloned());
        transcript.restore(&self.bound.entries());
        self.footer.borrow_mut().refresh(&self.bound.session, true);
    }

    pub(super) fn step(&mut self) -> io::Result<()> {
        if !self.paused {
            self.rows
                .set(self.screen.tui_mut().base().terminal.height());
            let mut index = 0;
            while index < self.dialogs.len() {
                if self.dialogs[index].answer.is_closed() {
                    let dialog = self.dialogs.remove(index);
                    self.hide_dialog(&dialog);
                } else {
                    index += 1;
                }
            }
            self.screen.step()?;
        }
        Ok(())
    }

    pub(super) fn render(&mut self) {
        self.screen.tui_mut().request_render(false);
    }

    pub(super) fn notice(&mut self, text: String, color: &'static str) {
        self.transcript
            .borrow_mut()
            .notice(text, if color == "info" { "muted" } else { color });
        self.render();
    }

    pub(super) fn show_armin(&mut self) {
        use super::utilities::{ArminAnimation, EasterEgg3d, EasterEggAnimation};
        if let Some(frame) = self
            .screen
            .fullscreen_frame()
            .filter(|frame| !frame.is_empty())
        {
            let frame = frame.to_vec();
            let animation = EasterEggAnimation::new(
                EasterEgg3d::Armin,
                frame,
                self.theme.clone(),
                pi_rs_tui::get_terminal_color_mode(pi_rs_tui::get_capabilities()),
            );
            self.close_selector();
            let component = ComponentRef::new(Decoration {
                content: animation,
                events: self.send.clone(),
            });
            let handle = self.screen.tui_mut().base_mut().show_overlay(
                component,
                OverlayOptions {
                    width: Some(SizeValue::Percent(100.)),
                    max_height: Some(SizeValue::Percent(100.)),
                    ..Default::default()
                },
            );
            self.selector = Some(handle);
        } else {
            self.show_decoration(ArminAnimation::new(self.theme.clone()));
        }
    }

    pub(super) fn show_decoration(&mut self, content: impl Component + 'static) {
        self.show_component(Decoration {
            content,
            events: self.send.clone(),
        });
    }

    pub(super) fn show_component(&mut self, component: impl Component + 'static) {
        self.close_selector();
        self.selector = Some(self.overlay(ComponentRef::new(component)));
    }

    pub(super) fn close_selector(&mut self) {
        if let Some(handle) = self.selector.take() {
            self.screen.tui_mut().base_mut().hide_overlay(handle);
        }
    }

    fn overlay(&mut self, component: ComponentRef) -> OverlayHandle {
        self.screen.tui_mut().base_mut().show_overlay(
            component,
            OverlayOptions {
                width: Some(SizeValue::Percent(80.0)),
                min_width: Some(32),
                max_height: Some(SizeValue::Percent(85.0)),
                margin: OverlayMargin::from(1),
                ..Default::default()
            },
        )
    }

    fn show_dialog(&mut self, dialog: UiDialog, answer: DialogAnswer) {
        if answer.is_closed() {
            return;
        }
        let id = self.next_dialog;
        self.next_dialog += 1;
        let events = self.send.clone();
        let title = match &dialog {
            UiDialog::Select(title, _)
            | UiDialog::Confirm(title, _)
            | UiDialog::Input(title, _)
            | UiDialog::Editor(title, _) => title.clone(),
        };
        let component = match dialog {
            UiDialog::Select(title, options) => ComponentRef::new(Picker::new(
                title,
                options.clone(),
                self.theme.clone(),
                move |selected| {
                    let answer = selected
                        .and_then(|index| options.get(index).cloned())
                        .map(UiAnswer::Text);
                    let _ = events.send(UiEvent::DialogFinished(id, answer));
                },
            )),
            UiDialog::Confirm(title, prompt) => ComponentRef::new(
                Picker::new(
                    format!("{title}\n{prompt}"),
                    vec!["Yes".into(), "No".into()],
                    self.theme.clone(),
                    move |selected| {
                        let _ = events.send(UiEvent::DialogFinished(
                            id,
                            selected.map(|index| UiAnswer::Confirmed(index == 0)),
                        ));
                    },
                )
                .without_filter()
                .preselect(1),
            ),
            UiDialog::Input(title, placeholder) => ComponentRef::new(InputDialog::new(
                title,
                placeholder,
                self.theme.clone(),
                move |value| {
                    let _ = events.send(UiEvent::DialogFinished(id, value.map(UiAnswer::Text)));
                },
            )),
            UiDialog::Editor(title, prefill) => {
                ComponentRef::new(TextDialog::new(title, None, prefill, true, id, self))
            }
        };
        let handle = self.overlay(component);
        self.dialogs.push(DialogEntry { id, handle, answer });
        self.blocked(
            id,
            Some(super::utilities::BlockedStatus {
                kind: pi_rs_tui::ProgramStatusKind::Question,
                message: title,
            }),
        );
    }

    fn show_secret(&mut self, title: String, answer: DialogAnswer) {
        if answer.is_closed() {
            return;
        }
        let id = self.next_dialog;
        self.next_dialog += 1;
        let message = title.clone();
        let send = self.send.clone();
        let component = InputDialog::new(title, None, self.theme.clone(), move |value| {
            let _ = send.send(UiEvent::DialogFinished(id, value.map(UiAnswer::Text)));
        })
        .masked();
        let handle = self.overlay(ComponentRef::new(component));
        self.dialogs.push(DialogEntry { id, handle, answer });
        self.blocked(
            id,
            Some(super::utilities::BlockedStatus {
                kind: pi_rs_tui::ProgramStatusKind::Auth,
                message,
            }),
        );
    }

    pub(super) fn close_dialogs(&mut self) {
        while let Some(dialog) = self.dialogs.pop() {
            self.hide_dialog(&dialog);
        }
        self.close_selector();
    }

    fn hide_dialog(&mut self, dialog: &DialogEntry) {
        self.screen.tui_mut().base_mut().hide_overlay(dialog.handle);
        self.blocked(dialog.id, None);
    }

    fn blocked(&mut self, id: u64, blocked: Option<super::utilities::BlockedStatus>) {
        let status = self.status_reporter.set_blocked(
            &id.to_string(),
            blocked,
            session_name(&self.bound.session).as_deref(),
        );
        self.report(status);
    }

    pub(super) fn apply_settings(&mut self) -> Result<(), String> {
        let settings = self.settings.get();
        let target = mode(&settings, self.options.tui_mode);
        if target == self.screen.tui_mut().mode() {
            self.screen.settings_changed(&settings);
        } else {
            self.close_dialogs();
            self.pause()?;
            self.screen = Screen::new(
                &self.terminal,
                target,
                self.options.agent_dir.clone(),
                &settings,
                &self.theme,
                &self.send,
            );
            self.mount();
            if target == TuiMode::Regular
                && let Some(state) = self.main_state.take()
            {
                self.screen.restore_main(state);
            }
            self.resume()?;
        }
        self.configure();
        self.render();
        Ok(())
    }

    pub(super) fn pause(&mut self) -> Result<(), String> {
        if !self.paused {
            if let Some(main) = self.screen.capture_main() {
                self.main_state = Some(main);
            }
            self.screen.stop(true).map_err(|error| error.to_string())?;
            self.paused = true;
        }
        Ok(())
    }

    pub(super) fn resume(&mut self) -> Result<(), String> {
        if self.paused {
            self.screen
                .tui_mut()
                .start()
                .map_err(|error| error.to_string())?;
            self.paused = false;
            self.enable_terminal_colors();
            self.update_title(None);
        }
        Ok(())
    }

    fn enable_terminal_colors(&mut self) {
        self.screen
            .tui_mut()
            .base_mut()
            .set_terminal_color_scheme_notifications(true);
        self.query_colors();
    }

    fn query_colors(&mut self) {
        let report = self.send.clone();
        let late = report.clone();
        self.screen.tui_mut().base_mut().query_terminal_colors(
            Duration::from_millis(150),
            Box::new(move |colors| {
                let _ = report.send(UiEvent::Colors(colors));
            }),
            Some(Box::new(move |colors| {
                let _ = late.send(UiEvent::Colors(colors));
            })),
        );
    }

    pub(super) fn cancel_bash(&self) {
        if let Some(cancel) = &self.bash {
            cancel.cancel();
        }
    }
    pub(super) fn cancel_auth(&self) {
        if let Some(cancel) = &self.auth {
            cancel.cancel();
        }
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        self.report(Some(pi_rs_tui::ProgramStatus {
            state: pi_rs_tui::ProgramStatusState::Clear,
            app: Some(config::APP_NAME.to_owned()),
            ..Default::default()
        }));
        if self.paused {
            return Ok(());
        }
        let transcript = if self.screen.tui_mut().mode() == TuiMode::Fullscreen
            && self.settings.get()["fullscreenExitOutput"].as_str() == Some("transcript")
        {
            let width = self.screen.tui_mut().base().terminal.width();
            Some(self.transcript.borrow_mut().render(width))
        } else {
            None
        };
        self.screen.stop(false)?;
        if let Some(lines) = transcript {
            let terminal = &mut self.screen.tui_mut().base_mut().terminal;
            for line in lines {
                terminal.write(&line);
                terminal.write("\r\n");
            }
        }
        self.paused = true;
        Ok(())
    }

    fn report(&mut self, status: Option<pi_rs_tui::ProgramStatus>) {
        if let (Some(write), Some(status)) = (&mut self.terminal.report, status) {
            write(status);
        }
    }

    pub(super) fn update_title(&mut self, title: Option<String>) {
        let title = title.unwrap_or_else(|| {
            let cwd = self.bound.session.cwd();
            let directory = Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&cwd);
            match session_name(&self.bound.session) {
                Some(name) => format!("{} - {name} - {directory}", config::APP_NAME),
                None => format!("{} - {directory}", config::APP_NAME),
            }
        });
        self.screen.tui_mut().base_mut().terminal.write(&format!(
            "\x1b]0;{}\x07",
            replace_control_characters(&title)
        ));
    }

    fn update_extension(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Notify(text, kind) => self.notice(
                text,
                match kind.as_deref() {
                    Some("error") => "error",
                    Some("warning") => "warning",
                    _ => "muted",
                },
            ),
            UiUpdate::Status(key, text) => {
                let mut footer = self.footer.borrow_mut();
                if let Some(text) = text {
                    footer.statuses.insert(key, text);
                } else {
                    footer.statuses.shift_remove(&key);
                }
            }
            UiUpdate::Widget(key, lines, placement) => {
                if let Some(lines) = lines {
                    self.widgets.insert(
                        key,
                        (placement.unwrap_or(UiWidgetPlacement::AboveEditor), lines),
                    );
                } else {
                    self.widgets.shift_remove(&key);
                }
                let mut above = self.above.borrow_mut();
                let mut below = self.below.borrow_mut();
                above.clear();
                below.clear();
                for (placement, rows) in self.widgets.values() {
                    let target = match placement {
                        UiWidgetPlacement::AboveEditor => &mut above,
                        UiWidgetPlacement::BelowEditor => &mut below,
                    };
                    target.extend(rows.iter().take(10).cloned());
                }
            }
            UiUpdate::Title(title) => self.update_title(Some(title)),
            UiUpdate::EditorText(text) => self.editor.borrow_mut().editor.set_text(&text),
        }
        self.render();
    }

    fn event(&mut self, event: &AgentSessionEvent) {
        self.transcript.borrow_mut().event(event);
        let mut redraw_title = false;
        match event {
            AgentSessionEvent::AgentStart => self
                .status
                .borrow_mut()
                .show("Working… (esc to interrupt)", false),
            AgentSessionEvent::AgentSettled { .. } => {
                self.status.borrow_mut().clear();
                self.footer.borrow_mut().refresh(&self.bound.session, true);
            }
            AgentSessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => self
                .status
                .borrow_mut()
                .retry(*attempt, *max_attempts, *delay_ms),
            AgentSessionEvent::AutoRetryEnd { .. } => self.status.borrow_mut().clear(),
            AgentSessionEvent::CompactionStart { reason } => self.status.borrow_mut().show(
                match reason {
                    CompactionReason::Manual => "Compacting context… (esc to cancel)",
                    CompactionReason::Overflow => {
                        "Context overflow detected, auto-compacting… (esc to cancel)"
                    }
                    _ => "Auto-compacting… (esc to cancel)",
                },
                false,
            ),
            AgentSessionEvent::CompactionEnd { .. } => {
                self.status.borrow_mut().clear();
                self.footer.borrow_mut().refresh(&self.bound.session, false);
            }
            AgentSessionEvent::ThinkingLevelChanged { level } => {
                let color = self.theme.clone();
                let border = thinking_border(*level);
                self.editor
                    .borrow_mut()
                    .editor
                    .set_border_color(Box::new(move |text| color.fg(border, text)));
                self.footer.borrow_mut().refresh(&self.bound.session, false);
            }
            AgentSessionEvent::ModelSelect { .. } => {
                self.footer.borrow_mut().refresh(&self.bound.session, false)
            }
            AgentSessionEvent::SessionInfoChanged { .. } => {
                self.footer.borrow_mut().refresh(&self.bound.session, true);
                redraw_title = true;
            }
            AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } => {
                let mut rows = self.pending.borrow_mut();
                rows.clear();
                rows.extend(
                    steering
                        .iter()
                        .map(|s| self.theme.fg("muted", &format!("Steering: {s}"))),
                );
                rows.extend(
                    follow_up
                        .iter()
                        .map(|s| self.theme.fg("muted", &format!("Follow-up: {s}"))),
                );
            }
            _ => {}
        }
        let status = self
            .status_reporter
            .handle_event(event, session_name(&self.bound.session).as_deref());
        self.report(status);
        if redraw_title {
            self.update_title(None);
        }
        self.render();
    }

    pub(super) fn dispatch(
        &mut self,
        event: UiEvent,
        controller: &Rc<Controller>,
        work: &Work,
    ) -> Result<(), String> {
        match event {
            UiEvent::Submit(text) => commands::submit(self, text, false, controller, work)?,
            UiEvent::Key(key) => commands::key(self, key, controller, work)?,
            UiEvent::Shortcut(key) => commands::shortcut(self, key, controller, work),
            UiEvent::Choice(choice) => commands::chosen(self, choice, controller, work)?,
            UiEvent::CloseSelector => self.close_selector(),
            UiEvent::Paste => self.paste()?,
            UiEvent::Suspend =>
            {
                #[cfg(unix)]
                if self.screen.tui_mut().base().terminal.is_tty() {
                    self.pause()?;
                    let result = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP)
                        .map_err(|error| error.to_string());
                    self.resume()?;
                    result?;
                }
            }
            UiEvent::Quit => {}
            UiEvent::Bound(bound, diagnostics) => {
                self.close_dialogs();
                self.bound = bound;
                self.refreshed_models.clear();
                self.bound
                    .refresh_catalog(work, self.send.clone(), self.options.offline);
                self.settings = load_settings(&self.options, &self.bound);
                self.editor.borrow_mut().keys.reload();
                self.widgets.clear();
                self.above.borrow_mut().clear();
                self.below.borrow_mut().clear();
                self.pending.borrow_mut().clear();
                self.footer.borrow_mut().statuses.clear();
                self.status.borrow_mut().clear();
                self.images.clear();
                self.editor.borrow_mut().editor.set_text("");
                self.restore();
                register_themes(&self.theme, &self.bound);
                let watcher = self.send.clone();
                self._watch = self
                    .theme
                    .watch(move || {
                        let _ = watcher.send(UiEvent::ThemeFile);
                    })
                    .ok();
                let theme = self
                    .options
                    .theme
                    .clone()
                    .or_else(|| self.settings.get_theme_setting());
                if let Err(error) = self.theme.select(theme.as_deref().unwrap_or("system")) {
                    self.theme
                        .select("system")
                        .map_err(|error| error.to_string())?;
                    self.notice(error.to_string(), "warning");
                }
                self.apply_settings()?;
                self.update_title(None);
                let status = self
                    .status_reporter
                    .reset(session_name(&self.bound.session).as_deref());
                self.report(status);
                for RuntimeDiagnostic { message, level } in diagnostics {
                    self.notice(message, level.as_str());
                }
            }
            UiEvent::Session(generation, event) if self.bound.generation == generation => {
                self.event(&event)
            }
            UiEvent::Restore(generation) if self.bound.generation == generation => {
                self.restore();
                self.render();
            }
            UiEvent::Models(generation, models) => {
                if generation == self.bound.generation {
                    self.refreshed_models = models;
                }
            }
            UiEvent::Dialog(generation, dialog, answer)
                if generation.is_none_or(|id| id == self.bound.generation) =>
            {
                self.show_dialog(dialog, answer)
            }
            UiEvent::SecretDialog(title, answer) => self.show_secret(title, answer),
            UiEvent::ExtensionUpdate(generation, update)
                if generation.is_none_or(|id| id == self.bound.generation) =>
            {
                self.update_extension(update)
            }
            UiEvent::DialogFinished(id, answer) => {
                if let Some(index) = self.dialogs.iter().position(|entry| entry.id == id) {
                    let entry = self.dialogs.remove(index);
                    self.hide_dialog(&entry);
                    let _ = entry.answer.send(answer);
                }
            }
            UiEvent::Transition(transition, response) => {
                self.close_selector();
                self.cancel_bash();
                self.cancel_auth();
                self.transitioning = true;
                let controller = controller.clone();
                work.push(Box::pin(async move {
                    let result = controller.transition(transition).await;
                    let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
                    if let Some(response) = response {
                        let _ = response.send(result);
                    }
                    let _ = controller.events.send(UiEvent::TransitionFinished(status));
                }));
            }
            UiEvent::TransitionFinished(result) => {
                self.transitioning = false;
                self.status.borrow_mut().clear();
                result?;
            }
            UiEvent::Notice(text, color) => self.notice(text, color),
            UiEvent::Colors(colors) => {
                if self
                    .theme
                    .apply_terminal_colors(colors)
                    .map_err(|error| error.to_string())?
                {
                    self.transcript.borrow_mut().invalidate();
                    self.render();
                }
            }
            UiEvent::ColorScheme(scheme) => {
                if self
                    .theme
                    .apply_terminal_scheme(scheme)
                    .map_err(|error| error.to_string())?
                {
                    self.transcript.borrow_mut().invalidate();
                    self.render();
                }
                self.query_colors();
            }
            UiEvent::ThemeFile => {
                self.theme.reload().map_err(|error| error.to_string())?;
                self.transcript.borrow_mut().invalidate();
                self.render();
            }
            UiEvent::BashChunk(id, chunk) => {
                self.transcript.borrow_mut().bash_chunk(id, &chunk);
                self.render();
            }
            UiEvent::BashEnd(id, result) => {
                self.bash = None;
                self.transcript
                    .borrow_mut()
                    .end_bash(id, result.as_ref().map_err(String::as_str));
                self.render();
            }
            UiEvent::EditorFinished(result) => {
                self.resume()?;
                if let Some(text) = result? {
                    self.editor.borrow_mut().editor.set_text(&text);
                } else {
                    self.notice(
                        "Set VISUAL or EDITOR to use the external editor.".into(),
                        "warning",
                    );
                }
                self.render();
            }
            UiEvent::AuthFinished(code) => {
                self.auth = None;
                self.resume()?;
                if code == 0 {
                    let _ = self
                        .send
                        .send(UiEvent::Transition(super::Transition::Reload, None));
                } else {
                    self.notice("Authentication was not changed.".into(), "warning");
                }
            }
            UiEvent::Session(..)
            | UiEvent::Restore(_)
            | UiEvent::Dialog(..)
            | UiEvent::ExtensionUpdate(..) => {}
        }
        Ok(())
    }

    fn paste(&mut self) -> Result<(), String> {
        let platform = clipboard::Platform::current();
        let env = clipboard::ClipboardEnv::from_env();
        if let Some(image) = clipboard_image::read_clipboard_image(platform, &env)? {
            self.images.push(ImageContent {
                kind: ImageContentKind::Image,
                data: base64::engine::general_purpose::STANDARD.encode(image.bytes),
                mime_type: image.mime_type,
            });
            self.notice("Image attached to the next message".into(), "muted");
        } else {
            let text = clipboard::read_text(platform, &env)?;
            self.editor.borrow_mut().editor.insert_text_at_cursor(&text);
        }
        self.render();
        Ok(())
    }
}

fn register_themes(theme: &Theme, bound: &Bound) {
    if let Some(resources) = bound.extensions.resources() {
        theme.set_registered_paths(
            resources
                .themes
                .iter()
                .map(|theme| theme.source_path.clone())
                .collect(),
        );
    }
}

fn load_settings(options: &InteractiveOptions, bound: &Bound) -> SettingsManager {
    let cwd = bound.session.cwd();
    let cwd = Path::new(&cwd);
    let trust = bound
        .trusted_project
        .or(options.project_trust_override)
        .or_else(|| {
            ProjectTrustStore::new(&options.agent_dir)
                .get(cwd)
                .ok()
                .flatten()
        });
    let mut settings = SettingsManager::create_with_dirs(
        cwd,
        &options.agent_dir,
        CreateOptions {
            project_trusted: false,
        },
    );
    let trusted = trust.unwrap_or_else(|| {
        !has_trust_requiring_project_resources(cwd)
            || settings.get()["defaultProjectTrust"].as_str() == Some("always")
    });
    settings.set_project_trusted(trusted);
    settings
}

fn new_editor(
    theme: &Theme,
    rows: Rc<Cell<usize>>,
    settings: &serde_json::Value,
    options: &InteractiveOptions,
    send: &UnboundedSender<UiEvent>,
) -> CustomEditor {
    let mut editor = text_editor(
        theme,
        rows,
        settings["editorPaddingX"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
    );
    let send_submit = send.clone();
    editor.set_on_submit(Some(Box::new(move |text| {
        let _ = send_submit.send(UiEvent::Submit(text.to_owned()));
    })));
    CustomEditor {
        editor,
        keys: KeybindingsManager::create(&options.agent_dir),
        shortcuts: Vec::new(),
        events: send.clone(),
    }
}

fn text_editor(theme: &Theme, rows: Rc<Cell<usize>>, padding_x: usize) -> Editor {
    let border = theme.clone();
    Editor::new(
        Box::new(move || rows.get()),
        Rc::new(|| {}),
        EditorTheme {
            border_color: Rc::new(move |text| border.fg("border", text)),
            select_list: select_theme(theme),
        },
        EditorOptions {
            padding_x,
            ..Default::default()
        },
    )
}

fn thinking_border(level: ThinkingLevel) -> &'static str {
    match level.as_str() {
        "minimal" => "thinkingMinimal",
        "low" => "thinkingLow",
        "medium" => "thinkingMedium",
        "high" => "thinkingHigh",
        "xhigh" => "thinkingXhigh",
        "max" => "thinkingMax",
        _ => "thinkingOff",
    }
}

struct TextDialog {
    title: String,
    placeholder: Option<String>,
    multiline: bool,
    id: u64,
    events: UnboundedSender<UiEvent>,
    theme: Theme,
    editor: Editor,
}

impl TextDialog {
    fn new(
        title: String,
        placeholder: Option<String>,
        prefill: Option<String>,
        multiline: bool,
        id: u64,
        ui: &Frontend,
    ) -> Self {
        let mut editor = text_editor(&ui.theme, ui.rows.clone(), 1);
        if let Some(prefill) = prefill {
            editor.set_text(&prefill);
        }
        let events = ui.send.clone();
        editor.set_on_submit(Some(Box::new(move |text| {
            let _ = events.send(UiEvent::DialogFinished(
                id,
                Some(UiAnswer::Text(text.to_owned())),
            ));
        })));
        Self {
            title,
            placeholder,
            multiline,
            id,
            events: ui.send.clone(),
            theme: ui.theme.clone(),
            editor,
        }
    }
}

impl Component for TextDialog {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for title in self.title.lines() {
            lines.extend(pi_rs_tui::wrap_text_with_ansi(
                &self.theme.fg("accent", title),
                width.max(1),
            ));
        }
        if self.editor.text().is_empty()
            && let Some(placeholder) = &self.placeholder
        {
            lines.extend(pi_rs_tui::wrap_text_with_ansi(
                &self.theme.fg("muted", placeholder),
                width.max(1),
            ));
        }
        lines.extend(self.editor.render(width));
        lines.push(self.theme.fg(
            "muted",
            &pi_rs_tui::truncate_to_width(
                if self.multiline {
                    "Enter to submit, Shift+Enter for newline, Esc to cancel"
                } else {
                    "Enter to submit, Esc to cancel"
                },
                width,
                true,
            ),
        ));
        lines
    }
    fn as_focusable(&mut self) -> Option<&mut dyn pi_rs_tui::Focusable> {
        self.editor.as_focusable()
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
    fn handle_input(&mut self, input: &str) {
        if pi_rs_tui::keys::matches_key_data(input, "escape")
            || pi_rs_tui::keys::matches_key_data(input, "ctrl+c")
        {
            let _ = self.events.send(UiEvent::DialogFinished(self.id, None));
        } else if self.multiline || !pi_rs_tui::keys::matches_key_data(input, "shift+enter") {
            self.editor.handle_input(input);
        }
    }
}

struct Decoration<T> {
    content: T,
    events: UnboundedSender<UiEvent>,
}

impl<T: Component> Component for Decoration<T> {
    fn render(&self, width: usize) -> Vec<String> {
        self.content.render(width)
    }

    fn handle_input(&mut self, input: &str) {
        let keys = pi_rs_tui::get_keybindings();
        if ["tui.select.cancel", "tui.select.confirm", "app.clear"]
            .into_iter()
            .any(|key| keys.matches(input, key))
        {
            let _ = self.events.send(UiEvent::CloseSelector);
        } else {
            self.content.handle_input(input);
        }
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }
    fn tick(&mut self, now: Instant) -> bool {
        self.content.tick(now)
    }
}
