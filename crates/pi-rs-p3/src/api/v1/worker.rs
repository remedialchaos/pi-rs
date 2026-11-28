//! A session worker's process values and Chord sync layer for the backup plugin's worker facet.

/// The file, relative to a worker's `dir`, whose presence opens the session's store with an
/// outbox (each commit's record, which backups upload). The backup plugin writes it once backup is
/// on for the session and removes it when backup stops for good; the base reads it before the
/// store opens.
pub const BACKUP_MARKER: &str = "backup/enabled";

pub use pi_rs_chord::sync::{
    Body, FlushOptions, FlushScheduler, MemorySyncState, PutBatchResult, SyncClient, SyncState,
    SyncStateStore, UploadResult,
};

/// Returns whether backup is enabled for a worker whose private directory is `dir`.
pub fn worker_outbox_enabled(dir: &std::path::Path) -> bool {
    dir.join(BACKUP_MARKER).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_outbox_enabled_tracks_backup_marker_presence() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(BACKUP_MARKER, "backup/enabled");
        assert!(!worker_outbox_enabled(dir.path()));
        let marker = dir.path().join(BACKUP_MARKER);
        let parent = marker
            .parent()
            .ok_or_else(|| std::io::Error::other("missing marker parent"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(&marker, b"")?;
        assert!(worker_outbox_enabled(dir.path()));
        std::fs::remove_file(&marker)?;
        assert!(!worker_outbox_enabled(dir.path()));
        Ok(())
    }
}
