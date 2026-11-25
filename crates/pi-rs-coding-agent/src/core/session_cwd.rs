use std::path::{Path, PathBuf};

use super::session_manager::SessionManager;

#[derive(Debug)]
pub struct MissingSessionCwd {
    pub session_file: PathBuf,
    pub session_cwd: PathBuf,
    pub fallback_cwd: PathBuf,
}

impl std::fmt::Display for MissingSessionCwd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
            self.session_cwd.display(),
            self.session_file.display(),
            self.fallback_cwd.display()
        )
    }
}

impl std::error::Error for MissingSessionCwd {}

pub fn missing_session_cwd(session: &SessionManager, fallback: &Path) -> Option<MissingSessionCwd> {
    let file = session.get_session_file()?;
    let cwd = session.get_cwd();
    if cwd.is_empty() || Path::new(cwd).exists() {
        return None;
    }
    Some(MissingSessionCwd {
        session_file: file.to_path_buf(),
        session_cwd: PathBuf::from(cwd),
        fallback_cwd: fallback.to_path_buf(),
    })
}
