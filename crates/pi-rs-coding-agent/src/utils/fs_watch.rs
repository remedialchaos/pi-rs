//! Filesystem watching, ported from `utils/fs-watch.ts`.
//!
//! Differences from Node's `fs.watch`: notify delivers typed [`notify::Event`] values through a
//! channel instead of `(eventType, filename)` callbacks, and the watcher closes on `Drop` rather
//! than through an explicit `close()`. Node's unhandled `error` event crash (#2791) does not exist
//! here; errors are delivered as `Err` values on the channel and must be consumed by the caller.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

/// Retry delay used by upstream callers when a watcher cannot be created.
pub const FS_WATCH_RETRY_DELAY_MS: Duration = Duration::from_millis(5000);

pub struct FsWatcher {
    // Kept alive for the lifetime of the wrapper; dropping it closes the OS watch.
    _watcher: RecommendedWatcher,
    events: Receiver<notify::Result<Event>>,
}

impl FsWatcher {
    /// Watch a path recursively, mirroring Node's default `watch(path, listener)`.
    pub fn watch(path: &Path) -> Result<Self, String> {
        Self::watch_with(path, RecursiveMode::Recursive)
    }

    pub fn watch_with(path: &Path, mode: RecursiveMode) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            // A dropped receiver means nobody is listening any more; there is nothing to report.
            let _ = tx.send(result);
        })
        .map_err(|error| format!("Failed to create watcher: {error}"))?;
        watcher
            .watch(path, mode)
            .map_err(|error| format!("Failed to watch {}: {error}", path.display()))?;
        Ok(Self {
            _watcher: watcher,
            events: rx,
        })
    }

    pub fn try_recv(&self) -> Option<Result<Event, String>> {
        match self.events.try_recv() {
            Ok(Ok(event)) => Some(Ok(event)),
            Ok(Err(error)) => Some(Err(error.to_string())),
            Err(_) => None,
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, String> {
        match self.events.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Upstream swallows errors from `watcher.close()`. Rust watchers close on drop and notify's
/// `stop_watching` errors are not surfaced by this wrapper, so this is deliberately a no-op.
pub fn close_watcher(_watcher: Option<&FsWatcher>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn retry_delay_is_five_seconds() {
        assert_eq!(FS_WATCH_RETRY_DELAY_MS, Duration::from_millis(5000));
    }

    #[test]
    fn close_watcher_tolerates_missing_watcher() {
        close_watcher(None);
    }

    #[test]
    fn watcher_receives_a_file_creation_event() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let watcher = FsWatcher::watch(dir.path())?;
        let target = dir.path().join("new-file.txt");
        fs::write(&target, b"content").map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut matched = false;
        while std::time::Instant::now() < deadline {
            let event = watcher.recv_timeout(Duration::from_millis(500))?;
            if event
                .paths
                .iter()
                .any(|path| path.ends_with("new-file.txt"))
            {
                matched = true;
                break;
            }
        }
        assert!(matched, "no file-creation event for {target:?}");
        Ok(())
    }

    #[test]
    fn watching_a_missing_path_fails() {
        assert!(FsWatcher::watch(std::path::Path::new("/pi-rs/no/such/watch/path-9182")).is_err());
    }
}
