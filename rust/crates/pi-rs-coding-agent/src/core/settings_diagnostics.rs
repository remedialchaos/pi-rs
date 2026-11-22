use std::collections::HashSet;

use super::{agent_session_runtime::RuntimeDiagnostic, settings_manager::SettingsManager};

pub fn collect_settings_diagnostics(settings: &mut SettingsManager) -> Vec<RuntimeDiagnostic> {
    settings
        .drain_errors()
        .into_iter()
        .map(|error| {
            RuntimeDiagnostic::warning(match error.path {
                Some(path) => format!("Invalid settings file {}: {}", path.display(), error.error),
                None => format!("Invalid {} settings: {}", error.scope.as_str(), error.error),
            })
        })
        .collect()
}

pub fn deduplicate_diagnostics(diagnostics: &mut Vec<RuntimeDiagnostic>) {
    let mut seen = HashSet::new();
    diagnostics
        .retain(|diagnostic| seen.insert((diagnostic.level.as_str(), diagnostic.message.clone())));
}
