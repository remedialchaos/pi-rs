//! Who runs on a data directory: the server and its supervisor each hold a lock beside it while they
//! live, and write a record (`running.json`: pid, port, version, start time;
//! `supervisor-running.json`: pid). A record counts only while its lock is held: a crash leaves the
//! file, never the lock.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// What `running.json` holds while a server runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRecord {
    pub pid: u32,
    pub port: u16,
    pub version: String,
    /// When it listened, in ms since the epoch.
    pub started_at: u64,
}

/// What `supervisor-running.json` holds while a supervisor runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRecord {
    pub pid: u32,
}

/// The file the server's lock lives in, beside the data directory it guards.
pub fn server_lock(data_dir: &Path) -> PathBuf {
    data_dir.join(".p3-server.lock")
}

/// The server's record in `data_dir`.
pub fn server_file(data_dir: &Path) -> PathBuf {
    data_dir.join("running.json")
}

/// The file the supervisor's lock lives in.
pub fn supervisor_lock(data_dir: &Path) -> PathBuf {
    data_dir.join(".p3-supervisor.lock")
}

/// The supervisor's record in `data_dir`: its pid.
pub fn supervisor_file(data_dir: &Path) -> PathBuf {
    data_dir.join("supervisor-running.json")
}

/// Whether another process holds the lock at `path`.
pub fn held(path: &Path) -> bool {
    match pi_rs_chord::node::workers::ProcessLock::try_acquire(path) {
        Ok(Some(lock)) => {
            drop(lock);
            false
        }
        Ok(None) => true,
        // A lock that cannot be taken is no proof that someone runs: report it held (conservative).
        Err(_) => true,
    }
}

/// The record of the server running on `data_dir`: its lock is held and `running.json` is there;
/// else `None`.
pub fn running(data_dir: &Path) -> Option<ServerRecord> {
    if !held(&server_lock(data_dir)) {
        return None;
    }
    read_record(&server_file(data_dir))
}

/// The pid of the supervisor running on `data_dir`, if one runs.
pub fn supervisor_pid(data_dir: &Path) -> Option<u32> {
    if !held(&supervisor_lock(data_dir)) {
        return None;
    }
    read_record(&supervisor_file(data_dir)).map(|record: SupervisorRecord| record.pid)
}

/// Writes the server's record.
pub fn write_record(data_dir: &Path, record: &ServerRecord) -> Result<(), std::io::Error> {
    let text =
        serde_json::to_string(record).map_err(|error| std::io::Error::other(error.to_string()))?;
    crate::files::write_private(&server_file(data_dir), &text).map_err(std::io::Error::other)
}

/// Writes the supervisor's record.
pub fn write_supervisor_record(data_dir: &Path, pid: u32) -> Result<(), std::io::Error> {
    let text = serde_json::to_string(&SupervisorRecord { pid })
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    crate::files::write_private(&supervisor_file(data_dir), &text).map_err(std::io::Error::other)
}

/// Removes the server's record; a missing one is fine.
pub fn remove_record(data_dir: &Path) {
    let _ = fs::remove_file(server_file(data_dir));
}

/// Removes the supervisor's record; a missing one is fine.
pub fn remove_supervisor_record(data_dir: &Path) {
    let _ = fs::remove_file(supervisor_file(data_dir));
}

fn read_record<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    // A trust boundary: a file. A server that holds the lock but has not listened yet has no record.
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_counts_only_while_its_lock_is_held() {
        let root = tempfile::tempdir().expect("temp dir");
        let data_dir = root.path();
        let record = ServerRecord {
            pid: 42,
            port: 9123,
            version: "1.2.3".into(),
            started_at: 1_700_000_000_000,
        };
        let text = serde_json::to_string(&record).expect("json");
        assert!(text.contains("\"startedAt\""));
        write_record(data_dir, &record).expect("record");
        // Nobody holds the lock yet: the record is stale.
        assert_eq!(running(data_dir), None);

        let lock = pi_rs_chord::node::workers::ProcessLock::try_acquire(&server_lock(data_dir))
            .expect("lock")
            .expect("free");
        assert_eq!(running(data_dir), Some(record));
        drop(lock);
        assert_eq!(running(data_dir), None);
    }

    #[test]
    fn a_supervisor_record_names_its_pid_while_the_lock_is_held() {
        let root = tempfile::tempdir().expect("temp dir");
        let data_dir = root.path();
        write_supervisor_record(data_dir, 77).expect("record");
        assert_eq!(supervisor_pid(data_dir), None);
        let lock = pi_rs_chord::node::workers::ProcessLock::try_acquire(&supervisor_lock(data_dir))
            .expect("lock")
            .expect("free");
        assert_eq!(supervisor_pid(data_dir), Some(77));
        remove_supervisor_record(data_dir);
        assert_eq!(supervisor_pid(data_dir), None);
        drop(lock);
    }
}
