//! Session discovery and the startup resume dialog.

use std::path::{Path, PathBuf};

use pi_rs_tui::ComponentRef;

use super::startup_ui::{load_theme, run_dialog};
use crate::core::session_manager::{SessionInfo, SessionManager, get_default_session_dir};
use crate::core::settings_manager::SettingsManager;
use crate::modes::interactive::selectors::SessionPicker;

pub fn list_current_sessions(cwd: &Path, dir: Option<PathBuf>) -> Result<Vec<SessionInfo>, String> {
    SessionManager::list(cwd, dir)
}

pub fn list_all_sessions(agent_dir: &Path, dir: Option<&Path>) -> Result<Vec<SessionInfo>, String> {
    if let Some(dir) = dir {
        return SessionManager::list_all(dir);
    }
    let root = agent_dir.join("sessions");
    let folders = match std::fs::read_dir(&root) {
        Ok(folders) => folders,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Cannot list sessions in {}: {error}",
                root.display()
            ));
        }
    };
    let mut sessions = SessionManager::list_all(&root)?;
    for folder in folders {
        let folder = folder.map_err(|error| error.to_string())?;
        if folder
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            sessions.extend(SessionManager::list_all(folder.path())?);
        }
    }
    sessions.sort_by(|a, b| {
        b.modified_millis
            .cmp(&a.modified_millis)
            .then_with(|| b.path.cmp(&a.path))
    });
    Ok(sessions)
}

pub async fn select_session(
    cwd: &Path,
    dir: Option<PathBuf>,
    settings: &SettingsManager,
    agent_dir: &Path,
) -> Result<Option<SessionInfo>, String> {
    let theme = load_theme(settings, agent_dir)?;
    let settings = settings.get();
    let agent_dir = agent_dir.to_path_buf();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let current = if let Some(path) = &dir {
            list_current_sessions(&cwd, Some(path.clone()))?
        } else {
            let default = get_default_session_dir(&cwd);
            let folder = default.file_name().ok_or("No project session directory")?;
            SessionManager::list_sessions(agent_dir.join("sessions").join(folder))?
        };
        let agent = agent_dir.clone();
        run_dialog(
            &settings,
            &agent_dir,
            theme,
            |theme, mut selected| {
                ComponentRef::new(SessionPicker::new(
                    current,
                    move || list_all_sessions(&agent, dir.as_deref()),
                    theme,
                    move |info| selected(info.cloned()),
                ))
            },
            || Ok(()),
        )
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_sessions_in_project_folders_and_custom_directory() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|error| error.to_string())?;
        let cwd = root.path().join("workspace");
        let folder = root.path().join("sessions").join("--workspace--");
        std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        std::fs::create_dir(&cwd).map_err(|error| error.to_string())?;
        let logfile = folder.join("first.jsonl");
        let header = serde_json::json!({"type":"session","version":3,"id":"id","cwd":cwd,"timestamp":"2025-06-01T12:30:00.000Z"});
        std::fs::write(&logfile, format!("{header}\n")).map_err(|error| error.to_string())?;
        assert_eq!(
            list_all_sessions(root.path(), None)?
                .first()
                .map(|session| &session.path),
            Some(&logfile)
        );
        assert_eq!(list_all_sessions(root.path(), Some(&folder))?.len(), 1);
        assert_eq!(list_current_sessions(&cwd, Some(folder.clone()))?.len(), 1);
        assert!(list_current_sessions(root.path(), Some(folder))?.is_empty());
        Ok(())
    }
}
