//! Settings-aware main-screen dialogs used before the interactive session starts.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use pi_rs_tui::keybindings::BindingKeys;
use pi_rs_tui::{
    ComponentRef, KeybindingDefinition, ProcessTerminal, ProgramStatus, ProgramStatusKind,
    ProgramStatusState, Terminal, Tui, TuiMainScreen, TuiStopOptions, set_capability_overrides,
    set_keybindings,
};
use serde_json::{Value, json};

use crate::config::{APP_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR, PACKAGE_NAME};
use crate::core::keybindings::KeybindingsManager;
use crate::core::project_trust::format_project_trust_prompt;
use crate::core::settings_manager::SettingsManager;
use crate::modes::interactive::selectors::{
    FIRST_TIME_SETUP_QUESTION, FirstTimeSetup, FirstTimeSetupResult, InputDialog, Picker,
    trust_items,
};
use crate::modes::interactive::theme::Theme;
use crate::modes::interactive::utilities::{BlockedStatus, dialog_blocked_status};
use crate::utils::flush_stdout::flush_stdout;

pub fn should_run_first_time_setup(settings_path: &Path) -> bool {
    should_setup(
        settings_path,
        std::env::var("PI_EXPERIMENTAL").as_deref() == Ok("1"),
        std::env::var_os(ENV_AGENT_DIR).is_some_and(|value| !value.is_empty()),
    )
}

fn should_setup(settings_path: &Path, experimental: bool, agent_override: bool) -> bool {
    PACKAGE_NAME == "@earendil-works/pi-coding-agent"
        && APP_NAME == "pi"
        && CONFIG_DIR_NAME == ".pi"
        && experimental
        && !agent_override
        && !settings_path.exists()
}

pub fn install_keybindings(agent_dir: &Path) {
    let config = KeybindingsManager::create(agent_dir).get_effective_config();
    let definitions = config
        .into_iter()
        .map(|(id, value)| {
            (
                id,
                KeybindingDefinition {
                    default_keys: BindingKeys::Many(value.keys()),
                    description: None,
                },
            )
        })
        .collect();
    set_keybindings(pi_rs_tui::KeybindingsManager::new(
        definitions,
        Default::default(),
    ));
}

pub(crate) fn setup_terminal(settings: &Value) {
    set_capability_overrides(
        crate::core::settings_manager::terminal_capability_overrides_from(settings),
    );
}

pub(super) fn load_theme(settings: &SettingsManager, agent_dir: &Path) -> Result<Theme, String> {
    let setting = settings.get_theme_setting();
    Theme::load(agent_dir, setting.as_deref())
        .or_else(|_| Theme::load(agent_dir, None))
        .map_err(|error| error.to_string())
}

pub(super) fn run_dialog<T: 'static>(
    settings: &Value,
    agent_dir: &Path,
    theme: Theme,
    create: impl FnOnce(Theme, Box<dyn FnMut(T)>) -> ComponentRef,
    process: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    install_keybindings(agent_dir);
    setup_terminal(settings);
    let terminal = ProcessTerminal::new();
    run_on_terminal(terminal, settings, agent_dir, theme, create, process)
}

pub(super) fn run_dialog_blocked<T: 'static>(
    settings: &Value,
    agent_dir: &Path,
    theme: Theme,
    blocked: BlockedStatus,
    create: impl FnOnce(Theme, Box<dyn FnMut(T)>) -> ComponentRef,
    process: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    install_keybindings(agent_dir);
    setup_terminal(settings);
    let mut terminal = ProcessTerminal::new();
    terminal.set_program_status(ProgramStatus {
        state: ProgramStatusState::Blocked,
        app: Some(APP_NAME.into()),
        kind: Some(blocked.kind),
        message: Some(blocked.message),
    });
    run_on_terminal(terminal, settings, agent_dir, theme, create, process)
}

fn run_on_terminal<T: 'static>(
    terminal: impl Terminal + 'static,
    settings: &Value,
    agent_dir: &Path,
    theme: Theme,
    create: impl FnOnce(Theme, Box<dyn FnMut(T)>) -> ComponentRef,
    mut process: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    if !terminal.is_tty() {
        return Err("Interactive selection requires a terminal".into());
    }
    let mut ui = TuiMainScreen::new(
        terminal,
        crate::core::settings_manager::show_hardware_cursor_from(settings),
        Some(agent_dir.to_path_buf()),
    );
    let clear_on_shrink = settings["terminal"]["clearOnShrink"]
        .as_bool()
        .unwrap_or_else(|| std::env::var("PI_CLEAR_ON_SHRINK").as_deref() == Ok("1"));
    ui.base.set_clear_on_shrink(clear_on_shrink);
    let result = Rc::new(RefCell::new(None));
    let callback_result = Rc::clone(&result);
    let component = create(
        theme.clone(),
        Box::new(move |selection| {
            let mut result = callback_result.borrow_mut();
            if result.is_none() {
                *result = Some(selection);
            }
        }),
    );
    ui.base.content.add_child(component.clone());
    ui.base.set_focus(Some(component));
    ui.start().map_err(|error| error.to_string())?;

    let recolored = Rc::new(Cell::new(false));
    let apply = |dirty: Rc<Cell<bool>>, theme: Theme| {
        Box::new(move |colors| {
            dirty.set(theme.apply_terminal_colors(colors).unwrap_or_default() || dirty.get());
        })
    };
    ui.base.query_terminal_colors(
        Duration::from_millis(250),
        apply(Rc::clone(&recolored), theme.clone()),
        Some(apply(Rc::clone(&recolored), theme.clone())),
    );
    let colors_changed = Rc::clone(&recolored);
    ui.base
        .on_terminal_color_scheme_change(Box::new(move |scheme| {
            colors_changed.set(
                theme.apply_terminal_scheme(scheme).unwrap_or_default() || colors_changed.get(),
            );
        }));
    ui.base.set_terminal_color_scheme_notifications(true);

    let operation = (|| {
        ui.render_now(false).map_err(|error| error.to_string())?;
        loop {
            ui.step(Duration::from_millis(16))
                .map_err(|error| error.to_string())?;
            if recolored.replace(false) {
                ui.request_render(false);
            }
            process()?;
            if let Some(result) = result.borrow_mut().take() {
                return Ok(result);
            }
        }
    })();
    ui.base.set_focus(None);
    ui.base.content.clear();
    ui.base.set_clear_on_shrink(true);
    ui.request_render(false);
    let clear = ui.render_now(false);
    let stop = ui.stop(TuiStopOptions::default());
    let flushed = flush_stdout();
    if let Err(error) = clear.and(stop).and(flushed) {
        return operation.and(Err(error.to_string()));
    }
    operation
}

pub async fn show_startup_selector<T>(
    settings: &SettingsManager,
    agent_dir: &Path,
    title: String,
    options: Vec<(String, T)>,
    blocked_kind: Option<ProgramStatusKind>,
) -> Result<Option<T>, String> {
    let (labels, values): (Vec<_>, Vec<_>) = options
        .into_iter()
        .map(|(label, value)| (label, Some(value)))
        .unzip();
    let mut values = values;
    let theme = load_theme(settings, agent_dir)?;
    let blocked =
        dialog_blocked_status(blocked_kind.unwrap_or(ProgramStatusKind::Question), &title);
    let result = run_dialog_blocked(
        &settings.get(),
        agent_dir,
        theme,
        blocked,
        |theme, select| {
            ComponentRef::new(
                Picker::new(title, labels, theme, select)
                    .without_filter()
                    .without_wrap(),
            )
        },
        || Ok(()),
    )?;
    Ok(result
        .and_then(|index| values.get_mut(index))
        .and_then(Option::take))
}

pub async fn show_startup_input(
    settings: &SettingsManager,
    agent_dir: &Path,
    title: String,
    placeholder: Option<String>,
    blocked_kind: Option<ProgramStatusKind>,
) -> Result<Option<String>, String> {
    let theme = load_theme(settings, agent_dir)?;
    let blocked =
        dialog_blocked_status(blocked_kind.unwrap_or(ProgramStatusKind::Question), &title);
    run_dialog_blocked(
        &settings.get(),
        agent_dir,
        theme,
        blocked,
        |theme, submit| ComponentRef::new(InputDialog::new(title, placeholder, theme, submit)),
        || Ok(()),
    )
}

pub async fn select_project_trust(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let (rows, _) = trust_items(cwd, None, true);
    let options = rows
        .into_iter()
        .map(|item| (item.value.label.clone(), item.value.label))
        .collect();
    show_startup_selector(
        settings,
        agent_dir,
        format_project_trust_prompt(cwd),
        options,
        Some(ProgramStatusKind::Permission),
    )
    .await
}

pub async fn show_first_time_setup(
    settings: &mut SettingsManager,
    agent_dir: &Path,
) -> Result<(), String> {
    let theme = Theme::load(agent_dir, Some("system")).map_err(|error| error.to_string())?;
    let setup = run_dialog_blocked(
        &settings.get(),
        agent_dir,
        theme,
        dialog_blocked_status(ProgramStatusKind::Permission, FIRST_TIME_SETUP_QUESTION),
        |theme, submit| ComponentRef::new(FirstTimeSetup::new(theme, submit)),
        || Ok(()),
    )?;
    if let Some(setup) = setup {
        persist_setup(settings, setup)?;
    }
    Ok(())
}

fn persist_setup(
    settings: &mut SettingsManager,
    result: FirstTimeSetupResult,
) -> Result<(), String> {
    if result.share_analytics
        && settings.get_global_settings()["trackingId"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        settings.set_value(
            &["trackingId".into()],
            json!(uuid::Uuid::new_v4().to_string()),
        )?;
    }
    settings.set_value(&["enableAnalytics".into()], json!(result.share_analytics))?;
    settings.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings_manager::CreateOptions;
    use pi_rs_tui::terminal::TerminalEvent;
    use std::collections::VecDeque;

    struct VirtualTerminal(VecDeque<TerminalEvent>);
    impl Terminal for VirtualTerminal {
        fn width(&self) -> usize {
            80
        }
        fn height(&self) -> usize {
            24
        }
        fn write(&mut self, _: &str) {}
        fn read_available(&mut self) -> Vec<u8> {
            Vec::new()
        }
        fn is_tty(&self) -> bool {
            true
        }
        fn poll_input(&mut self, _: Duration) -> std::io::Result<Vec<TerminalEvent>> {
            self.0
                .pop_front()
                .map(|event| vec![event])
                .ok_or_else(|| std::io::Error::other("Virtual terminal exhausted"))
        }
    }

    #[test]
    fn startup_gate_and_analytics_persistence() -> Result<(), String> {
        let path = tempfile::tempdir().map_err(|error| error.to_string())?;
        let settings_path = path.path().join("settings.json");
        assert!(should_setup(&settings_path, true, false));
        assert!(!should_setup(&settings_path, false, false));
        assert!(!should_setup(&settings_path, true, true));
        let mut settings =
            SettingsManager::create_with_dirs(path.path(), path.path(), CreateOptions::default());
        persist_setup(
            &mut settings,
            FirstTimeSetupResult {
                share_analytics: true,
            },
        )?;
        assert!(!should_setup(&settings_path, true, false));
        let first = settings.get_global_settings();
        let tracking = first["trackingId"].as_str().ok_or("missing tracking ID")?;
        assert!(uuid::Uuid::parse_str(tracking).is_ok());
        persist_setup(
            &mut settings,
            FirstTimeSetupResult {
                share_analytics: false,
            },
        )?;
        let second = settings.get_global_settings();
        assert_eq!(second["trackingId"], tracking);
        assert_eq!(second["enableAnalytics"], false);
        Ok(())
    }

    #[test]
    fn real_component_drives_virtual_startup() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let theme = Theme::load(dir.path(), Some("system")).map_err(|error| error.to_string())?;
        let terminal = VirtualTerminal(
            ["\x1b[B", "\r"]
                .into_iter()
                .map(|input| TerminalEvent::Input(input.into()))
                .collect(),
        );
        let result = run_on_terminal(
            terminal,
            &json!({}),
            dir.path(),
            theme,
            |theme, done| ComponentRef::new(FirstTimeSetup::new(theme, done)),
            || Ok(()),
        )?;
        assert_eq!(
            result,
            Some(FirstTimeSetupResult {
                share_analytics: false
            })
        );
        Ok(())
    }
}
