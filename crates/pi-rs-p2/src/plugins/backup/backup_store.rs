use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    Error, Result,
    sqlite::SqliteDatabase,
    store::StoreEngine,
    sync::{
        AsyncRemote, SyncClient, SyncFuture, SyncState, SyncStateStore, UploadResult,
        check_session_id,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::TerminalError;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bookmark {
    #[serde(skip_serializing_if = "Option::is_none")]
    synced_head: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pulling: Option<u64>,
}

struct Bookmarks(PathBuf);

impl SyncStateStore for Bookmarks {
    fn get(&self, id: String) -> SyncFuture<SyncState> {
        let directory = self.0.clone();
        Box::pin(async move {
            check_session_id(&id)?;
            let path = directory.join(format!("{id}.json"));
            let value: Bookmark = match tokio::fs::read(path).await {
                Ok(data) => serde_json::from_slice(&data)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Bookmark::default(),
                Err(error) => return Err(error.into()),
            };
            Ok(SyncState {
                synced_head: value.synced_head,
                pulling: value.pulling,
            })
        })
    }

    fn set(&self, id: String, state: SyncState) -> SyncFuture<()> {
        let directory = self.0.clone();
        Box::pin(async move {
            check_session_id(&id)?;
            let data = serde_json::to_vec(&Bookmark {
                synced_head: state.synced_head,
                pulling: state.pulling,
            })?;
            tokio::task::spawn_blocking(move || {
                atomic_write(&directory, &format!("{id}.json"), &data)
            })
            .await
            .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?
        })
    }

    fn delete(&self, id: String) -> SyncFuture<()> {
        let directory = self.0.clone();
        Box::pin(async move {
            check_session_id(&id)?;
            let path = directory.join(format!("{id}.json"));
            match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
    }
}

pub type AccountProvider = Arc<dyn Fn() -> SyncFuture<Option<String>> + Send + Sync>;

pub(super) fn atomic_write(directory: &Path, name: &str, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(data)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join(name))
        .map_err(|error| error.error)?;
    Ok(())
}

/// Runs sync against one named store and holds its bookmark across restarts.
pub struct BackupStore {
    name: String,
    sync: SyncClient,
    bookmarks: Arc<Bookmarks>,
    store: StoreEngine,
    stopped: Mutex<bool>,
    terminal: Option<TerminalError>,
    account: Option<AccountProvider>,
}

impl BackupStore {
    pub async fn open(
        store: StoreEngine,
        name: String,
        directory: &Path,
        target_key: &str,
        remote: Arc<dyn AsyncRemote>,
        device: String,
    ) -> Result<Self> {
        check_session_id(&name)?;
        check_session_id(target_key)?;
        store
            .database()
            .call(|connection| {
                connection.execute("UPDATE chord_meta SET outbox=1 WHERE outbox=0", [])?;
                Ok(())
            })
            .await?;
        let bookmarks = Arc::new(Bookmarks(directory.join("backup").join(target_key)));
        Ok(Self {
            name,
            sync: SyncClient::new(remote, bookmarks.clone(), device),
            bookmarks,
            store,
            stopped: Mutex::new(false),
            terminal: None,
            account: None,
        })
    }

    pub fn with_terminal(mut self, terminal: Option<TerminalError>) -> Self {
        self.terminal = terminal;
        self
    }

    pub fn with_account(mut self, account: Option<AccountProvider>) -> Self {
        self.account = account;
        self
    }

    async fn owner(&self) -> Result<Option<String>> {
        let path = self.bookmarks.0.join(format!("{}.account", self.name));
        match tokio::fs::read_to_string(path).await {
            Ok(owner) => Ok(Some(owner)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn bind_owner(&self, account: &str) -> Result<()> {
        let directory = self.bookmarks.0.clone();
        let file_name = format!("{}.account", self.name);
        let bytes = account.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || atomic_write(&directory, &file_name, &bytes))
            .await
            .map_err(|error| Error::Invalid(error.to_string()))?
    }

    pub async fn run(&self, report: impl Fn(Value)) -> Result<()> {
        let mut stopped = self.stopped.lock().await;
        if *stopped {
            return Ok(());
        }
        let head = || self.bookmarks.get(self.name.clone());
        if let Some(get_account) = &self.account {
            let current = match get_account().await {
                Ok(account) => account,
                Err(error) => {
                    report(retrying_status(
                        error.to_string(),
                        head().await?.synced_head,
                    ));
                    return Err(error);
                }
            };
            if let Some(current) = current {
                match self.owner().await? {
                    Some(owner) if owner != current => {
                        report(paused_status("other_account", head().await?.synced_head));
                        return Ok(());
                    }
                    None => self.bind_owner(&current).await?,
                    Some(_) => {}
                }
            }
        }
        let result = async {
            report(status("uploading", head().await?.synced_head));
            let mut upload = self.sync.upload(&self.name, &self.store).await?;
            if matches!(upload, UploadResult::RemoteEmpty { .. }) {
                self.bookmarks.delete(self.name.clone()).await?;
                upload = self.sync.upload(&self.name, &self.store).await?;
            }
            let synced_head = head().await?.synced_head;
            match upload {
                UploadResult::Idle | UploadResult::Uploaded { .. } => {
                    report(status("synced", synced_head));
                }
                UploadResult::Diverged { .. } | UploadResult::RemoteEmpty { .. } => {
                    *stopped = true;
                    report(stopped_status(
                        "diverged",
                        "the backup's history is not this store's",
                        synced_head,
                    ));
                }
                UploadResult::Behind { .. } => {
                    *stopped = true;
                    report(stopped_status(
                        "behind",
                        "the store lost commits the backup holds",
                        synced_head,
                    ));
                }
            }
            Ok(())
        }
        .await;
        match result {
            Err(error)
                if self
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal(&error)) =>
            {
                *stopped = true;
                report(stopped_status(
                    "refused",
                    &error.to_string(),
                    head().await.ok().and_then(|s| s.synced_head),
                ));
                Ok(())
            }
            Err(error) => {
                report(retrying_status(
                    error.to_string(),
                    head().await.ok().and_then(|s| s.synced_head),
                ));
                Err(error)
            }
            Ok(()) => Ok(()),
        }
    }

    /// Restore remote history into an empty database using the same target bookmark as uploads.
    pub async fn restore(
        remote: Arc<dyn AsyncRemote>,
        device: String,
        name: &str,
        directory: &Path,
        target_key: &str,
        empty: SqliteDatabase,
    ) -> Result<StoreEngine> {
        check_session_id(target_key)?;
        let state = Arc::new(Bookmarks(directory.join("backup").join(target_key)));
        SyncClient::new(remote, state, device)
            .pull(name, None, Some(empty))
            .await
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn status(state: &str, head: Option<u64>) -> Value {
    let mut status = json!({"state": state, "at": now_ms()});
    if let Some(head) = head {
        status["syncedHead"] = json!(head);
    }
    status
}

pub(super) fn retrying_status(error: String, head: Option<u64>) -> Value {
    let mut status = json!({"state": "retrying", "error": error, "at": now_ms()});
    if let Some(head) = head {
        status["syncedHead"] = json!(head);
    }
    status
}

pub(super) fn paused_status(reason: &str, head: Option<u64>) -> Value {
    let mut status = json!({"state": "paused", "reason": reason, "at": now_ms()});
    if let Some(head) = head {
        status["syncedHead"] = json!(head);
    }
    status
}

pub(super) fn stopped_status(reason: &str, error: &str, head: Option<u64>) -> Value {
    let mut status = json!({"state": "stopped", "reason": reason, "error": error, "at": now_ms()});
    if let Some(head) = head {
        status["syncedHead"] = json!(head);
    }
    status
}

/// Returns a `paused` (`reason = "off"`) status if `directory/backup/<target>/*.json` shows a
/// previous backup bookmark, or `None` for a store never backed up.
pub async fn paused_while_off(directory: &Path) -> Result<Option<Value>> {
    let root = directory.join("backup");
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut found = false;
    let mut synced_head: Option<u64> = None;
    while let Some(target) = entries.next_entry().await? {
        if !target.file_type().await?.is_dir() {
            continue;
        }
        let mut bookmarks = tokio::fs::read_dir(target.path()).await?;
        while let Some(bookmark) = bookmarks.next_entry().await? {
            if !bookmark.file_type().await?.is_file() {
                continue;
            }
            if !bookmark.file_name().to_string_lossy().ends_with(".json") {
                continue;
            }
            found = true;
            let text = tokio::fs::read_to_string(bookmark.path()).await?;
            if let Ok(parsed) = serde_json::from_str::<Bookmark>(&text)
                && let Some(head) = parsed.synced_head
            {
                synced_head = Some(synced_head.map_or(head, |best| best.max(head)));
            }
        }
    }
    Ok(found.then(|| paused_status("off", synced_head)))
}
