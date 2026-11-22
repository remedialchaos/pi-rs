//! Interactive resource enable/disable editor for `pi config`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;

use pi_rs_tui::ComponentRef;

use super::startup_ui::{load_theme, run_dialog};
use crate::core::package_manager::ResolveResult;
use crate::core::settings_manager::{SettingsManager, SettingsScope};
use crate::modes::interactive::selectors::{
    ConfigEdit, ConfigPicker, ConfigPickerOptions, config_resources,
};

pub async fn select_config(
    resolved: ResolveResult,
    settings: &mut SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    write_scope: SettingsScope,
    project_mode_available: bool,
) -> Result<(), String> {
    let project_mode_available = project_mode_available && settings.is_project_trusted();
    if write_scope == SettingsScope::Project && !project_mode_available {
        return Err(
            "Project is not trusted. Use --approve to modify local resource config.".into(),
        );
    }
    let theme = load_theme(settings, agent_dir)?;
    let resources = config_resources(resolved, settings, cwd, agent_dir, project_mode_available);
    let terminal_settings = settings.get();
    let edits = Rc::new(RefCell::new(VecDeque::<ConfigEdit>::new()));
    let pending = Rc::clone(&edits);
    // Construct the component before the loop so its immutable settings borrow ends before writes begin.
    let finished = Rc::new(std::cell::Cell::new(false));
    let completed = Rc::clone(&finished);
    let component = ComponentRef::new(ConfigPicker::new(
        resources,
        settings,
        ConfigPickerOptions {
            cwd,
            agent_dir,
            scope: write_scope,
            project_available: project_mode_available,
        },
        theme.clone(),
        move |change| {
            if let Some(edit) = change {
                pending.borrow_mut().push_back(edit);
            } else {
                completed.set(true);
            }
        },
    ));
    // The terminal runner owns the component and closes only after all queued changes have been persisted.
    let close = Rc::new(RefCell::new(None::<Box<dyn FnMut(())>>));
    let register = Rc::clone(&close);
    run_dialog(
        &terminal_settings,
        agent_dir,
        theme,
        move |_, on_close| {
            *register.borrow_mut() = Some(on_close);
            component
        },
        || {
            for edit in edits.borrow_mut().drain(..) {
                settings.set_value_scoped(edit.scope, &[edit.key], edit.value)?;
            }
            if finished.get()
                && let Some(mut on_close) = close.borrow_mut().take()
            {
                on_close(());
            }
            Ok(())
        },
    )
}
