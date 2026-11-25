//! Drop-in-compatible subset of `proper-lockfile`: atomic directory locks with
//! stale-lock takeover and `ELOCKED` contention errors.
//!
//! Upstream `proper-lockfile` creates a lock directory and renews its mtime with
//! a heartbeat timer while the lock is held. This port does not spawn a thread:
//! it refreshes the mtime on each retry instead. That keeps the lock stale after
//! `stale_ms` of total inactivity if no other contender retries, which is
//! sufficient for the short synchronous critical sections in `settings-manager`,
//! `auth-storage`, and `trust-manager`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Options mirroring the subset of `proper-lockfile` options used upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOptions {
    /// Consider a lock stale (and removable) after this many milliseconds.
    pub stale_ms: u64,
    /// Number of retries after an initial `ELOCKED` failure.
    pub retries: usize,
    /// Canonicalize the target so symlink aliases share one lock.
    pub realpath: bool,
    /// Explicit lock directory, overriding `<target>.lock`.
    pub lockfile_path: Option<PathBuf>,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            stale_ms: 10_000,
            retries: 0,
            realpath: true,
            lockfile_path: None,
        }
    }
}

/// A held lock. Dropping it removes the lock directory, releasing the lock.
#[derive(Debug)]
pub struct LockGuard {
    lock_dir: PathBuf,
}

impl LockGuard {
    /// The directory used as the lock.
    pub fn lock_path(&self) -> &Path {
        &self.lock_dir
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best effort: the lock directory is kept empty, so a plain remove_dir
        // succeeds unless another process already took over a stale lock.
        let _ = std::fs::remove_dir(&self.lock_dir);
    }
}

/// Return the lock directory for `target`, applying `realpath` and the
/// `lockfile_path` override. A non-existent target falls back to its literal
/// path so locks can be acquired before the file is created.
fn lock_path_for(target: &Path, options: &LockOptions) -> PathBuf {
    if let Some(override_path) = &options.lockfile_path {
        return override_path.clone();
    }
    let resolved = if options.realpath {
        std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf())
    } else {
        target.to_path_buf()
    };
    let mut name = resolved.into_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// True when the lock directory's mtime is at least `stale_ms` behind `now`.
fn is_stale(lock_path: &Path, stale_ms: u64, now: SystemTime) -> bool {
    let modified = match std::fs::metadata(lock_path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    match now.duration_since(modified) {
        Ok(age) => age.as_millis() >= u128::from(stale_ms),
        Err(_) => false,
    }
}

/// Renew a lock's mtime without a background thread: creating and immediately
/// removing a marker file bumps the containing directory's mtime, leaving the
/// lock directory empty so `remove_dir` in `Drop` still works.
fn refresh_mtime(lock_path: &Path) {
    let marker = lock_path.join(".heartbeat");
    if std::fs::write(&marker, b"").is_ok() {
        let _ = std::fs::remove_file(&marker);
    }
}

/// `min(10 * 2^retry, 1000)` milliseconds backoff, saturating on large retries.
fn backoff_ms(retry: usize) -> u64 {
    let shift = u32::try_from(retry.min(10)).unwrap_or(10);
    10_u64.saturating_mul(1_u64 << shift).min(1000)
}

/// The shared synchronous algorithm behind both public entry points.
fn lock_with_now(
    target: &Path,
    options: &LockOptions,
    now: SystemTime,
) -> Result<LockGuard, String> {
    let lock_path = lock_path_for(target, options);
    let mut retry = 0usize;
    let mut stale_removals = 0usize;

    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                return Ok(LockGuard {
                    lock_dir: lock_path,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // One stale takeover is allowed even with `retries == 0`; each
                // additional contender may retry up to `options.retries` times.
                if is_stale(&lock_path, options.stale_ms, now) && stale_removals <= options.retries
                {
                    stale_removals += 1;
                    if std::fs::remove_dir(&lock_path).is_ok() {
                        continue;
                    }
                }
                refresh_mtime(&lock_path);
                if retry >= options.retries {
                    return Err("ELOCKED".to_string());
                }
                std::thread::sleep(Duration::from_millis(backoff_ms(retry)));
                retry += 1;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Synchronous form of `proper-lockfile.lock`; provided for API symmetry with
/// the upstream async entry point but using the same synchronous algorithm.
pub fn lock(target: &Path, options: &LockOptions) -> Result<LockGuard, String> {
    lock_with_now(target, options, SystemTime::now())
}

/// Acquire a lock synchronously, as used by `settings-manager`, `auth-storage`,
/// and `trust-manager` upstream.
pub fn lock_sync(target: &Path, options: &LockOptions) -> Result<LockGuard, String> {
    lock_with_now(target, options, SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> Result<tempfile::TempDir, String> {
        tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .map_err(|e| e.to_string())
    }

    #[test]
    fn backoff_grows_and_caps_at_one_second() {
        assert_eq!(backoff_ms(0), 10);
        assert_eq!(backoff_ms(1), 20);
        assert_eq!(backoff_ms(6), 640);
        assert_eq!(backoff_ms(7), 1000);
        assert_eq!(backoff_ms(usize::MAX), 1000);
    }

    #[test]
    fn lock_creates_and_releases_directory() -> Result<(), String> {
        let root = temp_dir("pi-lock-create-")?;
        let target = root.path().join("settings.json");
        std::fs::write(&target, "{}").map_err(|e| e.to_string())?;

        let guard = lock_sync(&target, &LockOptions::default())?;
        assert!(guard.lock_path().is_dir());
        assert_eq!(
            guard.lock_path(),
            PathBuf::from(format!("{}.lock", target.display()))
        );
        drop(guard);
        assert!(!target.with_extension("json.lock").exists());
        Ok(())
    }

    #[test]
    fn second_lock_fails_with_elocked() -> Result<(), String> {
        let root = temp_dir("pi-lock-contend-")?;
        let target = root.path().join("auth.json");
        std::fs::write(&target, "{}").map_err(|e| e.to_string())?;

        let first = lock(&target, &LockOptions::default())?;
        let second = lock(&target, &LockOptions::default());
        assert_eq!(second.err(), Some("ELOCKED".to_string()));
        drop(first);
        assert!(lock_sync(&target, &LockOptions::default()).is_ok());
        Ok(())
    }

    #[test]
    fn stale_lock_is_taken_over() -> Result<(), String> {
        let root = temp_dir("pi-lock-stale-")?;
        let target = root.path().join("trust.json");
        std::fs::write(&target, "{}").map_err(|e| e.to_string())?;

        let held = lock_sync(&target, &LockOptions::default())?;
        let future = SystemTime::now()
            .checked_add(Duration::from_secs(60))
            .ok_or("system time overflow")?;
        let takeover = lock_with_now(&target, &LockOptions::default(), future)?;
        assert!(takeover.lock_path().is_dir());
        drop(takeover);
        drop(held);
        Ok(())
    }

    #[test]
    fn lockfile_path_override_is_honored() -> Result<(), String> {
        let root = temp_dir("pi-lock-override-")?;
        let target = root.path().join("settings.json");
        std::fs::write(&target, "{}").map_err(|e| e.to_string())?;
        let custom = root.path().join("custom.lock");
        let options = LockOptions {
            lockfile_path: Some(custom.clone()),
            ..LockOptions::default()
        };

        let guard = lock_sync(&target, &options)?;
        assert!(custom.is_dir());
        assert!(!target.with_extension("json.lock").exists());
        drop(guard);
        assert!(!custom.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn realpath_aliases_share_the_lock_but_literal_paths_do_not() -> Result<(), String> {
        use std::os::unix::fs::symlink;

        let root = temp_dir("pi-lock-realpath-")?;
        let real = root.path().join("auth.json");
        let alias = root.path().join("auth-alias.json");
        std::fs::write(&real, "{}").map_err(|e| e.to_string())?;
        symlink(&real, &alias).map_err(|e| e.to_string())?;

        let guard = lock_sync(&real, &LockOptions::default())?;
        assert_eq!(
            lock_sync(&alias, &LockOptions::default()).err(),
            Some("ELOCKED".to_string())
        );
        let literal_options = LockOptions {
            realpath: false,
            ..LockOptions::default()
        };
        assert!(lock_sync(&alias, &literal_options).is_ok());
        drop(guard);
        Ok(())
    }
}
