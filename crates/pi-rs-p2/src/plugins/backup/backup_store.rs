use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    Result,
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
}

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
        })
    }

    pub(super) fn with_terminal(mut self, terminal: Option<TerminalError>) -> Self {
        self.terminal = terminal;
        self
    }

    pub async fn run(&self, report: impl Fn(Value)) -> Result<()> {
        let mut stopped = self.stopped.lock().await;
        if *stopped {
            return Ok(());
        }
        let head = || self.bookmarks.get(self.name.clone());
        let result = async {
            report(status("uploading", head().await?.synced_head, None));
            let state = match self.sync.upload(&self.name, &self.store).await? {
                UploadResult::Idle | UploadResult::Uploaded { .. } => "synced",
                UploadResult::Behind { .. } => "behind",
                UploadResult::Diverged { .. } | UploadResult::StaleEpoch { .. } => "diverged",
            };
            report(status(state, head().await?.synced_head, None));
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
                report(status(
                    "error",
                    None,
                    Some(format!("{error} (stopped until a restart)")),
                ));
                Ok(())
            }
            Err(error) => {
                report(status("error", None, Some(error.to_string())));
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

pub(super) fn status(state: &str, head: Option<u64>, error: Option<String>) -> Value {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut status = json!({"state": state, "at": at});
    if let Some(head) = head {
        status["syncedHead"] = json!(head);
    }
    if let Some(error) = error {
        status["error"] = json!(error);
    }
    status
}
