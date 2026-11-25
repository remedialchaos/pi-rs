//! The owner's `mcp.json` followed while the server runs.
//!
//! The file is read at each change: a save caught half-way is read once more before it counts, and
//! an invalid read keeps the last valid servers and says why the last read failed.

use crate::plugins::mcp::config::{OwnerConfig, parse_owner_config};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

/// How long a poll waits between two reads of the file.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The file as last read: the last valid servers and their errors, and why the last read failed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnerConfigState {
    pub config: OwnerConfig,
    pub failure: Option<String>,
}

/// The size and the time of the file as last read.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}

/// The owner's configuration file, read at each change.
#[derive(Clone)]
pub struct OwnerConfigFile {
    path: PathBuf,
    state: Arc<Mutex<OwnerConfigState>>,
    stamp: Arc<Mutex<Option<Stamp>>>,
}

impl OwnerConfigFile {
    /// The file in `data_dir`, with the servers last read validly before this reader existed.
    pub fn new(data_dir: &Path, last: OwnerConfig) -> Self {
        Self {
            path: data_dir.join("mcp.json"),
            state: Arc::new(Mutex::new(OwnerConfigState {
                config: last,
                failure: None,
            })),
            stamp: Arc::new(Mutex::new(None)),
        }
    }

    /// The path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file as last read.
    pub fn state(&self) -> OwnerConfigState {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Reads the file now: a missing file is no servers, an invalid one keeps the last valid servers.
    pub async fn read(&self) -> OwnerConfigState {
        // The read counts as the last one: a poll that follows it sees no change.
        if let Some(stamp) = self.stamp_of().await {
            *self.stamp.lock().unwrap_or_else(|error| error.into_inner()) = Some(stamp);
        }
        let text = match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => text,
            // A missing file is no servers.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
            Err(error) => {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.failure = Some(error.to_string());
                return state.clone();
            }
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match parse_owner_config(&text) {
            Ok(config) => {
                state.config = config;
                state.failure = None;
            }
            Err(error) => {
                // An invalid read keeps the last valid servers.
                state.failure = Some(error);
            }
        }
        state.clone()
    }

    /// Reads the file when it changed since the last read; true when it did.
    pub async fn poll(&self) -> bool {
        let stamp = match self.stamp_of().await {
            Some(stamp) => stamp,
            None => return false,
        };
        let changed = {
            let mut seen = self.stamp.lock().unwrap_or_else(|error| error.into_inner());
            if *seen == Some(stamp) {
                false
            } else {
                *seen = Some(stamp);
                true
            }
        };
        if !changed {
            return false;
        }
        self.read().await;
        true
    }

    /// The size and the time of the file, when it is there.
    async fn stamp_of(&self) -> Option<Stamp> {
        let metadata = tokio::fs::metadata(&self.path).await.ok()?;
        Some(Stamp {
            size: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The owner servers of a configuration, by name.
    fn servers(state: &OwnerConfigState) -> Vec<String> {
        state
            .config
            .servers
            .iter()
            .map(|server| match server {
                crate::plugins::mcp::config::OwnerServer::Stdio { name, .. }
                | crate::plugins::mcp::config::OwnerServer::Http { name, .. } => name.clone(),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_missing_file_is_no_servers() {
        let directory = tempfile::tempdir().expect("a directory");
        let file = OwnerConfigFile::new(directory.path(), OwnerConfig::default());
        let state = file.read().await;
        assert!(servers(&state).is_empty());
        assert_eq!(state.failure, None);
    }

    #[tokio::test]
    async fn a_file_is_read_and_followed() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("mcp.json");
        fs::write(&path, r#"{"mcpServers":{"files":{"command":"mcp-files"}}}"#)
            .expect("a configuration");
        let file = OwnerConfigFile::new(directory.path(), OwnerConfig::default());
        let state = file.read().await;
        assert_eq!(servers(&state), ["files"]);
        assert_eq!(state.failure, None);
        // An unchanged file is not read again.
        assert!(!file.poll().await);
        // A changed file is.
        fs::write(
            &path,
            r#"{"mcpServers":{"files":{"command":"mcp-files"},"docs":{"command":"mcp-docs"}}}"#,
        )
        .expect("a configuration");
        assert!(file.poll().await);
        assert_eq!(servers(&file.state()), ["files", "docs"]);
    }

    #[tokio::test]
    async fn an_invalid_file_keeps_the_last_valid_servers() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("mcp.json");
        fs::write(&path, r#"{"mcpServers":{"files":{"command":"mcp-files"}}}"#)
            .expect("a configuration");
        let file = OwnerConfigFile::new(directory.path(), OwnerConfig::default());
        file.read().await;
        fs::write(&path, "not json").expect("an invalid configuration");
        assert!(file.poll().await);
        let state = file.state();
        assert_eq!(servers(&state), ["files"]);
        assert_eq!(state.failure.as_deref(), Some("Invalid mcp.json syntax"));
    }
}
