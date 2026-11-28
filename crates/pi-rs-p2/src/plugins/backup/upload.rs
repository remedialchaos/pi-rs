use std::{
    collections::HashMap,
    io::{self, Cursor, Read},
    sync::Arc,
    time::Duration,
};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    sync::{AsyncRemote, BatchInfo, Body, PutBatchResult, remote_head},
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, timeout},
};
use uuid::Uuid;

use super::{BACKUP_TERMINAL, TerminalError, is_terminal};

pub const CHUNK_BYTES: usize = 1024 * 1024;
const IDLE: Duration = Duration::from_secs(60);

pub enum Begun {
    Upload(String),
    Result(PutBatchResult),
}

enum Part {
    Chunk(Vec<u8>, oneshot::Sender<()>),
    End,
}

struct UploadReader {
    input: mpsc::Receiver<Part>,
    chunk: Cursor<Vec<u8>>,
    consumed: Option<oneshot::Sender<()>>,
}

impl Read for UploadReader {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        loop {
            let count = self.chunk.read(destination)?;
            if count != 0 || destination.is_empty() {
                return Ok(count);
            }
            if let Some(consumed) = self.consumed.take() {
                let _ = consumed.send(());
            }
            match self.input.blocking_recv() {
                Some(Part::Chunk(bytes, consumed)) => {
                    self.chunk = Cursor::new(bytes);
                    self.consumed = Some(consumed);
                }
                Some(Part::End) => return Ok(0),
                None => return Err(io::Error::other("Backup upload aborted")),
            }
        }
    }
}

struct Upload {
    store: String,
    sender: Option<mpsc::Sender<Part>>,
    result: Option<JoinHandle<Result<PutBatchResult>>>,
    last_activity: Instant,
}

#[derive(Clone)]
pub struct Uploads {
    remote: Arc<dyn AsyncRemote>,
    device: String,
    active: Arc<Mutex<HashMap<String, Upload>>>,
    terminal: Option<TerminalError>,
}

impl Uploads {
    pub fn new(remote: Arc<dyn AsyncRemote>, device: String) -> Self {
        Self {
            remote,
            device,
            active: Arc::default(),
            terminal: None,
        }
    }

    pub(super) fn with_terminal(mut self, terminal: Option<TerminalError>) -> Self {
        self.terminal = terminal;
        self
    }

    fn map_error(terminal: Option<&TerminalError>, error: Error) -> Error {
        if terminal.is_some_and(|classify| classify(&error)) {
            ChordError::new(BACKUP_TERMINAL, error.to_string()).into()
        } else {
            error
        }
    }

    pub async fn begin(&self, store: &str, after: u64, to: u64, archive: bool) -> Result<Begun> {
        let mut active = self.active.lock().await;
        if active.values().any(|upload| upload.store == store) {
            return Err(Error::Invalid(
                "A backup upload for this store is already open".into(),
            ));
        }
        if to <= after {
            return Err(Error::Invalid("Backup slot must advance its store".into()));
        }
        let listing = self
            .remote
            .list(store.into())
            .await
            .map_err(|error| Self::map_error(self.terminal.as_ref(), error))?;
        if let Some(slot) = listing.batches.iter().find(|slot| slot.after == after) {
            return Ok(Begun::Result(PutBatchResult::Exists {
                to: slot.to,
                device: slot.device.clone(),
            }));
        }
        let head = remote_head(&listing)?;
        if after != head {
            return Ok(Begun::Result(PutBatchResult::NotHead { head }));
        }

        let (sender, input) = mpsc::channel(1);
        let body = Body::from_reader(UploadReader {
            input,
            chunk: Cursor::new(Vec::new()),
            consumed: None,
        });
        let put = self.remote.put_batch(
            store.into(),
            BatchInfo {
                after,
                to,
                device: self.device.clone(),
                archive: archive.then_some(true),
                folded: None,
            },
            body,
        );
        let terminal = self.terminal.clone();
        let result = tokio::spawn(async move {
            put.await
                .map_err(|error| Self::map_error(terminal.as_ref(), error))
        });
        let id = Uuid::new_v4().to_string();
        active.insert(
            id.clone(),
            Upload {
                store: store.into(),
                sender: Some(sender),
                result: Some(result),
                last_activity: Instant::now(),
            },
        );
        let entries = Arc::downgrade(&self.active);
        let expired = id.clone();
        tokio::spawn(async move {
            let mut delay = IDLE;
            loop {
                tokio::time::sleep(delay).await;
                let Some(active) = entries.upgrade() else {
                    break;
                };
                let mut entries = active.lock().await;
                let Some(upload) = entries.get_mut(&expired) else {
                    break;
                };
                delay = IDLE.saturating_sub(upload.last_activity.elapsed());
                if delay.is_zero() {
                    if let Some(result) = upload.result.take() {
                        // Keep the store reserved until the remote drops its temporary body.
                        drop(upload.sender.take());
                        drop(entries);
                        let _ = result.await;
                        active.lock().await.remove(&expired);
                    }
                    break;
                }
            }
        });
        Ok(Begun::Upload(id))
    }

    pub async fn append(&self, store: &str, id: &str, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > CHUNK_BYTES {
            return Err(Error::Invalid("Backup chunk exceeds 1 MiB".into()));
        }
        let sender = {
            let mut active = self.active.lock().await;
            let upload = active
                .get_mut(id)
                .filter(|upload| upload.store == store && upload.result.is_some())
                .ok_or_else(no_upload)?;
            upload.last_activity = Instant::now();
            upload.sender.clone().ok_or_else(no_upload)?
        };
        let (consumed, received) = oneshot::channel();
        let appended = timeout(IDLE, async {
            sender
                .send(Part::Chunk(bytes, consumed))
                .await
                .map_err(|_| {
                    Error::Invalid("Backup upload closed before accepting the chunk".into())
                })?;
            received.await.map_err(|_| {
                Error::Invalid("Backup upload closed while accepting the chunk".into())
            })
        })
        .await
        .map_err(|_| Error::Invalid("Backup upload timed out".into()))?;
        if appended.is_err()
            && sender.is_closed()
            && let Err(error) = self.finish(store, id).await
            && is_terminal(&error)
        {
            return Err(error);
        }
        appended
    }

    pub async fn finish(&self, store: &str, id: &str) -> Result<PutBatchResult> {
        let (sender, result) = {
            let mut active = self.active.lock().await;
            let upload = active
                .get_mut(id)
                .filter(|upload| upload.store == store)
                .ok_or_else(no_upload)?;
            let result = upload.result.take().ok_or_else(no_upload)?;
            (upload.sender.take().ok_or_else(no_upload)?, result)
        };
        let active = self.active.clone();
        let id = id.to_owned();
        // A lost finish response must not leave the store reserved.
        tokio::spawn(async move {
            let _ = sender.send(Part::End).await;
            let result = result.await;
            active.lock().await.remove(&id);
            result.map_err(|error| Error::Invalid(error.to_string()))?
        })
        .await
        .map_err(|error| Error::Invalid(error.to_string()))?
    }

    pub async fn abort(&self, store: &str, id: &str) {
        let result = {
            let mut active = self.active.lock().await;
            let Some(upload) = active.get_mut(id).filter(|upload| upload.store == store) else {
                return;
            };
            let Some(result) = upload.result.take() else {
                return;
            };
            drop(upload.sender.take());
            result
        };
        let active = self.active.clone();
        let id = id.to_owned();
        let _ = tokio::spawn(async move {
            let _ = result.await;
            active.lock().await.remove(&id);
        })
        .await;
    }

    pub async fn close(&self) {
        let abandoned = std::mem::take(&mut *self.active.lock().await);
        for upload in abandoned.into_values() {
            drop(upload.sender);
            if let Some(result) = upload.result {
                let _ = result.await;
            }
        }
    }
}

fn no_upload() -> Error {
    ChordError::invalid("args", "backup: no such upload", None).into()
}
