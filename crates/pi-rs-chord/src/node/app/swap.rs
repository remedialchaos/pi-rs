//! Crash-hardened scoped store swap and recovery using durable fsync and a swap journal.
use super::{new_token, private_directory};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_SUFFIX: &str = ".swap.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapKind {
    Replace,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapPhase {
    Prepared,
    Swapped,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapJournal {
    pub version: u32,
    pub id: String,
    pub kind: SwapKind,
    pub phase: SwapPhase,
    pub target: PathBuf,
    pub stage: Option<PathBuf>,
    pub aside: PathBuf,
    pub had_target: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapRecoveryReport {
    pub rolled_back: usize,
    pub rolled_forward: usize,
}

/// Flush directory metadata to disk on Unix; no-op on non-Unix platforms.
pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    let _ = path;
    Ok(())
}

/// Recursively flush all regular files and directories under `path` (children before parents).
pub fn sync_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Err(Error::Invalid("swap tree cannot contain symlinks".into()));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            sync_tree(&entry.path())?;
        }
        sync_dir(path)?;
    } else if meta.is_file() {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

fn target_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::Invalid("swap target has no parent".into()))
}

fn occupied(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(Error::Invalid(
            "worker data is not a private directory".into(),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn write_journal(path: &Path, journal: &SwapJournal) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("swap journal has no parent".into()))?;
    private_directory(parent)?;
    let tmp = parent.join(format!(".tmp.{}", new_token(12)));
    let bytes = serde_json::to_vec(journal)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_res = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_dir(parent)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::remove_dir_all(path).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Crash-hardened filesystem swap transaction paired with a durable journal marker.
pub struct ScopedStoreSwap {
    journal_path: PathBuf,
    journal: SwapJournal,
    finished: bool,
}

impl ScopedStoreSwap {
    /// Sync `stage`, record a durable journal, move any existing `target` aside, and move `stage`
    /// into `target`. Caller must then run the SQLite transaction and call `commit()` or `rollback()`.
    pub fn begin_replace(journal_dir: &Path, stage: &Path, target: &Path) -> Result<Self> {
        private_directory(journal_dir)?;
        sync_tree(stage)?;
        let parent = target_parent(target)?;
        let id = new_token(16);
        let aside = parent.join(format!(".chord-aside-{id}"));
        let had_target = occupied(target)?;
        let journal_path = journal_dir.join(format!("{id}{JOURNAL_SUFFIX}"));
        let mut journal = SwapJournal {
            version: JOURNAL_VERSION,
            id,
            kind: SwapKind::Replace,
            phase: SwapPhase::Prepared,
            target: target.to_path_buf(),
            stage: Some(stage.to_path_buf()),
            aside: aside.clone(),
            had_target,
        };
        write_journal(&journal_path, &journal)?;

        if had_target {
            if let Err(err) = fs::rename(target, &aside) {
                let _ = recover_one(&journal_path, &journal);
                return Err(err.into());
            }
            if let Err(err) = sync_dir(parent) {
                let _ = recover_one(&journal_path, &journal);
                return Err(err);
            }
        }
        if let Err(err) = fs::rename(stage, target) {
            let _ = recover_one(&journal_path, &journal);
            return Err(err.into());
        }
        if let Err(err) = sync_dir(parent) {
            let _ = recover_one(&journal_path, &journal);
            return Err(err);
        }

        journal.phase = SwapPhase::Swapped;
        if let Err(err) = write_journal(&journal_path, &journal) {
            let _ = recover_one(&journal_path, &journal);
            return Err(err);
        }

        Ok(Self {
            journal_path,
            journal,
            finished: false,
        })
    }

    /// Record a durable journal and move any existing `target` aside prior to deleting host data.
    pub fn begin_delete(journal_dir: &Path, target: &Path) -> Result<Self> {
        private_directory(journal_dir)?;
        let parent = target_parent(target)?;
        let id = new_token(16);
        let aside = parent.join(format!(".chord-deleted-{id}"));
        let had_target = occupied(target)?;
        let journal_path = journal_dir.join(format!("{id}{JOURNAL_SUFFIX}"));
        let mut journal = SwapJournal {
            version: JOURNAL_VERSION,
            id,
            kind: SwapKind::Delete,
            phase: SwapPhase::Prepared,
            target: target.to_path_buf(),
            stage: None,
            aside: aside.clone(),
            had_target,
        };
        write_journal(&journal_path, &journal)?;

        if had_target {
            if let Err(err) = fs::rename(target, &aside) {
                let _ = recover_one(&journal_path, &journal);
                return Err(err.into());
            }
            if let Err(err) = sync_dir(parent) {
                let _ = recover_one(&journal_path, &journal);
                return Err(err);
            }
        }

        journal.phase = SwapPhase::Swapped;
        if let Err(err) = write_journal(&journal_path, &journal) {
            let _ = recover_one(&journal_path, &journal);
            return Err(err);
        }

        Ok(Self {
            journal_path,
            journal,
            finished: false,
        })
    }

    /// Mark the swap committed (point of no return) and remove backup/journal artifacts.
    pub fn commit(mut self) -> Result<()> {
        self.finished = true;
        self.journal.phase = SwapPhase::Committed;
        write_journal(&self.journal_path, &self.journal)?;
        if self.journal.had_target {
            let _ = remove_dir_if_exists(&self.journal.aside);
        }
        if let Some(stage) = &self.journal.stage {
            let _ = remove_dir_if_exists(stage);
        }
        if let Some(parent) = self.journal.target.parent() {
            let _ = sync_dir(parent);
        }
        let _ = fs::remove_file(&self.journal_path);
        if let Some(jparent) = self.journal_path.parent() {
            let _ = sync_dir(jparent);
        }
        Ok(())
    }

    /// Roll back the filesystem swap, restoring any previous `target` directory.
    pub fn rollback(mut self) -> Result<()> {
        self.finished = true;
        recover_one(&self.journal_path, &self.journal)?;
        Ok(())
    }
}

impl Drop for ScopedStoreSwap {
    fn drop(&mut self) {
        if !self.finished {
            let _ = recover_one(&self.journal_path, &self.journal);
        }
    }
}

/// Standalone crash-hardened directory replacement using the swap journal protocol.
pub fn replace_store_dir(journal_dir: &Path, stage: &Path, target: &Path) -> Result<()> {
    let swap = ScopedStoreSwap::begin_replace(journal_dir, stage, target)?;
    swap.commit()
}

fn recover_one(journal_path: &Path, journal: &SwapJournal) -> Result<bool> {
    if journal.version != JOURNAL_VERSION {
        return Err(Error::Invalid(format!(
            "unsupported swap journal version {}",
            journal.version
        )));
    }
    let parent = target_parent(&journal.target)?;
    let rolled_forward = match journal.phase {
        SwapPhase::Committed => {
            match journal.kind {
                SwapKind::Replace => {
                    if !journal.target.exists()
                        && let Some(stage) = &journal.stage
                        && stage.exists()
                    {
                        fs::rename(stage, &journal.target)?;
                    }
                    if journal.had_target {
                        remove_dir_if_exists(&journal.aside)?;
                    }
                    if let Some(stage) = &journal.stage {
                        remove_dir_if_exists(stage)?;
                    }
                }
                SwapKind::Delete => {
                    remove_dir_if_exists(&journal.target)?;
                    if journal.had_target {
                        remove_dir_if_exists(&journal.aside)?;
                    }
                }
            }
            sync_dir(parent)?;
            true
        }
        SwapPhase::Prepared | SwapPhase::Swapped => {
            match journal.kind {
                SwapKind::Replace => {
                    if journal.had_target {
                        if journal.aside.exists() {
                            if journal.target.exists() {
                                if journal.stage.as_ref().is_some_and(|s| !s.exists()) {
                                    if let Some(stage) = &journal.stage {
                                        fs::rename(&journal.target, stage)?;
                                    }
                                } else {
                                    remove_dir_if_exists(&journal.target)?;
                                }
                            }
                            fs::rename(&journal.aside, &journal.target)?;
                        }
                        if let Some(stage) = &journal.stage {
                            remove_dir_if_exists(stage)?;
                        }
                    } else {
                        if journal.target.exists()
                            && journal.stage.as_ref().is_none_or(|s| !s.exists())
                        {
                            remove_dir_if_exists(&journal.target)?;
                        }
                        if let Some(stage) = &journal.stage {
                            remove_dir_if_exists(stage)?;
                        }
                    }
                }
                SwapKind::Delete => {
                    if journal.had_target && journal.aside.exists() && !journal.target.exists() {
                        fs::rename(&journal.aside, &journal.target)?;
                    }
                }
            }
            sync_dir(parent)?;
            false
        }
    };
    match fs::remove_file(journal_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    if let Some(jparent) = journal_path.parent() {
        sync_dir(jparent)?;
    }
    Ok(rolled_forward)
}

/// Recover any interrupted store swaps recorded in `journal_dir`.
pub fn recover_swaps(journal_dir: &Path) -> Result<SwapRecoveryReport> {
    let mut report = SwapRecoveryReport::default();
    let entries = match fs::read_dir(journal_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(err.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp.") {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if name.ends_with(JOURNAL_SUFFIX) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    for path in paths {
        let bytes = fs::read(&path)?;
        let journal: SwapJournal = serde_json::from_slice(&bytes)?;
        if recover_one(&path, &journal)? {
            report.rolled_forward += 1;
        } else {
            report.rolled_back += 1;
        }
    }
    Ok(report)
}
