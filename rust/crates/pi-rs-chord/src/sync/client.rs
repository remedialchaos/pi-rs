//! Async orchestration with bounded blocking-codec bridges. No filesystem work runs on Tokio workers.
use super::{
    BatchInfo, BatchReader, FileSystemSyncRemote, OUTBOX_PAGE, OUTBOX_PAGE_BYTES, PULL_BYTES,
    PutBatchResult, RemoteListing, UPLOAD_BYTES, encode_batch, encode_log_line, invalid,
    remote_head, remote_history,
};
use crate::{
    Error, Result,
    sqlite::SqliteDatabase,
    store::{OutboxCommit, StoreEngine, read_outbox, trim_outbox},
};
use std::{
    future::Future,
    io::{self, Cursor, Read},
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
pub type SyncFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
async fn blocking<T: Send + 'static>(
    run: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(run)
        .await
        .map_err(|e| invalid(format!("sync worker failed: {e}")))?
}
/// An owned body closes its producer when dropped, including rejection before the first read.
/// At most two 64 KiB input chunks wait behind a slow consumer.
pub struct Body {
    receiver: mpsc::Receiver<Result<Vec<u8>>>,
}
impl Body {
    pub fn from_reader(mut reader: impl Read + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel(2);
        tokio::task::spawn_blocking(move || {
            loop {
                let mut bytes = vec![0; 64 * 1024];
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.truncate(n);
                        if sender.blocking_send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = sender.blocking_send(Err(e.into()));
                        break;
                    }
                }
            }
        });
        Self { receiver }
    }
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::from_reader(Cursor::new(bytes))
    }
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<u8>>> {
        self.receiver.recv().await
    }
    pub async fn collect(mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            bytes.extend(chunk?);
        }
        Ok(bytes)
    }
    pub(crate) fn channel() -> (Self, BodyWriter) {
        let (sender, receiver) = mpsc::channel(2);
        (Self { receiver }, BodyWriter(sender))
    }
    pub(crate) fn reader(self) -> BodyReader {
        BodyReader {
            receiver: self.receiver,
            chunk: Cursor::new(Vec::new()),
        }
    }
}
pub(crate) struct BodyReader {
    receiver: mpsc::Receiver<Result<Vec<u8>>>,
    chunk: Cursor<Vec<u8>>,
}
impl Read for BodyReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let count = self.chunk.read(bytes)?;
            if count != 0 || bytes.is_empty() {
                return Ok(count);
            }
            match self.receiver.blocking_recv() {
                Some(Ok(chunk)) => self.chunk = Cursor::new(chunk),
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => return Ok(0),
            }
        }
    }
}
/// Blocking producers emit bounded chunks and stop when the async consumer drops.
pub(crate) struct BodyWriter(mpsc::Sender<Result<Vec<u8>>>);
impl BodyWriter {
    pub(crate) fn fail(&self, error: Error) {
        let _ = self.0.blocking_send(Err(error));
    }
}
impl io::Write for BodyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(64 * 1024);
        if count == 0 {
            return Ok(0);
        }
        self.0
            .blocking_send(Ok(bytes[..count].to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "sync body consumer closed"))?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub trait AsyncRemote: Send + Sync {
    fn list(&self, id: String) -> SyncFuture<RemoteListing>;
    fn get_batch(&self, id: String, after: u64) -> SyncFuture<Option<Body>>;
    fn get_snapshot(&self, id: String, to: u64) -> SyncFuture<Option<Body>>;
    fn put_batch(&self, id: String, batch: BatchInfo, body: Body) -> SyncFuture<PutBatchResult>;
}
impl AsyncRemote for FileSystemSyncRemote {
    fn list(&self, id: String) -> SyncFuture<RemoteListing> {
        let remote = self.clone();
        Box::pin(blocking(move || FileSystemSyncRemote::list(&remote, &id)))
    }
    fn get_batch(&self, id: String, after: u64) -> SyncFuture<Option<Body>> {
        let remote = self.clone();
        Box::pin(async move {
            Ok(
                blocking(move || FileSystemSyncRemote::get_batch(&remote, &id, after))
                    .await?
                    .map(Body::from_reader),
            )
        })
    }
    fn get_snapshot(&self, id: String, to: u64) -> SyncFuture<Option<Body>> {
        let remote = self.clone();
        Box::pin(async move {
            Ok(
                blocking(move || FileSystemSyncRemote::get_snapshot(&remote, &id, to))
                    .await?
                    .map(Body::from_reader),
            )
        })
    }
    fn put_batch(&self, id: String, batch: BatchInfo, body: Body) -> SyncFuture<PutBatchResult> {
        let remote = self.clone();
        Box::pin(blocking(move || {
            FileSystemSyncRemote::put_batch(
                &remote,
                &id,
                batch.after,
                batch.to,
                &batch.device,
                body.reader(),
                batch.archive == Some(true),
            )
        }))
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    pub synced_head: Option<u64>,
    pub pulling: Option<u64>,
}
pub trait SyncStateStore: Send + Sync {
    fn get(&self, id: String) -> SyncFuture<SyncState>;
    fn set(&self, id: String, state: SyncState) -> SyncFuture<()>;
}
#[derive(Default)]
pub struct MemorySyncState(Mutex<std::collections::HashMap<String, SyncState>>);
impl SyncStateStore for MemorySyncState {
    fn get(&self, id: String) -> SyncFuture<SyncState> {
        let value = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(value) })
    }
    fn set(&self, id: String, state: SyncState) -> SyncFuture<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, state);
        Box::pin(async { Ok(()) })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum UploadResult {
    Idle,
    Uploaded { to: u64 },
    Behind { at: u64 },
    Diverged { at: u64 },
    StaleEpoch { at: u64, epoch: u64 },
}
#[derive(Clone)]
pub struct SyncClient {
    remote: Arc<dyn AsyncRemote>,
    state: Arc<dyn SyncStateStore>,
    pub device: String,
}
async fn open_target(db: SqliteDatabase) -> Result<StoreEngine> {
    let store = StoreEngine::open(db).await?;
    store
        .database()
        .call(|c| {
            c.execute("UPDATE chord_meta SET outbox=1", [])?;
            Ok(())
        })
        .await?;
    Ok(store)
}
impl SyncClient {
    pub fn new(
        remote: Arc<dyn AsyncRemote>,
        state: Arc<dyn SyncStateStore>,
        device: impl Into<String>,
    ) -> Self {
        Self {
            remote,
            state,
            device: device.into(),
        }
    }
    async fn shared(&self, id: &str, local: u64) -> Result<u64> {
        let state = self.state.get(id.into()).await?;
        Ok(if state.pulling.is_some_and(|to| local <= to) {
            local
        } else {
            state.synced_head.unwrap_or(0)
        })
    }
    async fn saved(&self, id: &str, to: u64) -> Result<()> {
        self.state
            .set(
                id.into(),
                SyncState {
                    synced_head: Some(to),
                    pulling: None,
                },
            )
            .await
    }
    pub async fn upload(&self, id: &str, store: &StoreEngine) -> Result<UploadResult> {
        super::check_session_id(id)?;
        let mut uploaded = None;
        loop {
            let meta = store.meta().await?;
            if !meta.outbox {
                return Err(invalid("sync upload requires an enabled outbox"));
            }
            let head = meta.last_seq as u64;
            let shared = self.shared(id, head).await?;
            if head < shared {
                return Ok(UploadResult::Behind { at: shared });
            }
            if head == shared {
                return Ok(uploaded.map_or(UploadResult::Idle, |to| UploadResult::Uploaded { to }));
            }
            let (to, body, archive) = if (meta.outbox_floor as u64) > shared {
                let (to, body) = store.export_stream().await?;
                (to as u64, body, true)
            } else {
                let commits = self
                    .upload_range(store, shared as i64, meta.last_seq)
                    .await?;
                let to = commits
                    .last()
                    .map(|c| c.seq as u64)
                    .ok_or_else(|| invalid("outbox is discontinuous"))?;
                let bytes = blocking(move || encode_batch(Vec::new(), commits)).await?;
                (to, Body::from_bytes(bytes), false)
            };
            let batch = BatchInfo {
                after: shared,
                to,
                device: self.device.clone(),
                archive: archive.then_some(true),
                folded: None,
            };
            let synced = match self.remote.put_batch(id.into(), batch, body).await? {
                PutBatchResult::Created => {
                    uploaded = Some(to);
                    to
                }
                PutBatchResult::Exists { to, device } if device == self.device => to,
                PutBatchResult::StaleEpoch { epoch } => {
                    return Ok(UploadResult::StaleEpoch { at: shared, epoch });
                }
                _ => return Ok(UploadResult::Diverged { at: shared }),
            };
            // Persist acknowledgement before trimming. A failed state save leaves the retryable body intact.
            self.saved(id, synced).await?;
            let meta = store.meta().await?;
            let through = (synced.min(meta.last_seq as u64)) as i64;
            if through > meta.outbox_floor {
                trim_outbox(store.database(), through).await?;
            }
        }
    }
    async fn upload_range(
        &self,
        store: &StoreEngine,
        after: i64,
        to: i64,
    ) -> Result<Vec<OutboxCommit>> {
        let mut result = Vec::new();
        let mut at = after;
        let mut bytes = 0;
        let mut limit = OUTBOX_PAGE;
        while at < to {
            let page = read_outbox(store.database(), at, limit).await?;
            if page.is_empty() {
                return Err(invalid("outbox is discontinuous"));
            }
            let mut page_bytes = 0;
            let count = page.len();
            for commit in page {
                if commit.seq > to {
                    break;
                }
                if commit.seq != at + 1 {
                    return Err(invalid("outbox is discontinuous"));
                }
                let size = crate::json::stringify(&serde_json::to_value(&commit.writes)?)?
                    .encode_utf16()
                    .count()
                    + 64;
                if !result.is_empty() && bytes + size > UPLOAD_BYTES {
                    return Ok(result);
                }
                page_bytes += size;
                bytes += size;
                at = commit.seq;
                result.push(commit);
            }
            let average = page_bytes / count.max(1);
            limit = (OUTBOX_PAGE_BYTES / average.max(1)).clamp(1, OUTBOX_PAGE);
        }
        Ok(result)
    }
    /// Existing targets remain open on failure. Empty targets are opened only after remote history admission.
    pub async fn pull(
        &self,
        id: &str,
        mut target: Option<StoreEngine>,
        empty: Option<SqliteDatabase>,
    ) -> Result<StoreEngine> {
        super::check_session_id(id)?;
        let listing = self.remote.list(id.into()).await?;
        let local = if let Some(store) = &target {
            store.meta().await?.last_seq as u64
        } else {
            0
        };
        if local > self.shared(id, local).await? {
            return Err(invalid("sync: target has unsynced local changes"));
        }
        let history = remote_history(&listing, Some(local))?;
        let mut planned = Vec::new();
        if let Some(to) = history.snapshot {
            planned.push((true, 0, to, true));
        }
        for b in history.batches {
            planned.push((false, b.after, b.to, b.archive == Some(true)));
        }
        let owned = target.is_none();
        let mut empty = empty;
        let result = async {
            let mut head = local;
            for (snapshot, after, to, archive) in planned {
                if to <= head {
                    continue;
                }
                let body = if snapshot {
                    self.remote.get_snapshot(id.into(), to).await?
                } else {
                    self.remote.get_batch(id.into(), after).await?
                }
                .ok_or_else(|| invalid("sync: remote history is unavailable"))?;
                self.state
                    .set(
                        id.into(),
                        SyncState {
                            synced_head: Some(head),
                            pulling: Some(to),
                        },
                    )
                    .await?;
                if archive {
                    if target.is_some() {
                        return Err(invalid("sync: restore an archive into an empty database"));
                    }
                    let db = empty
                        .take()
                        .ok_or_else(|| invalid("sync: no empty target database"))?;
                    target = Some(StoreEngine::import_reader(db, body.reader(), true).await?);
                } else {
                    if target.is_none() {
                        target = Some(
                            open_target(
                                empty
                                    .take()
                                    .ok_or_else(|| invalid("sync: no empty target database"))?,
                            )
                            .await?,
                        );
                    }
                    let store = target
                        .as_ref()
                        .ok_or_else(|| invalid("sync: missing store"))?;
                    let (sender, mut receiver) = mpsc::channel(1);
                    tokio::task::spawn_blocking(move || {
                        let mut chunk = Vec::new();
                        let mut bytes = 0;
                        for item in BatchReader::new(body.reader()) {
                            let commit = match item {
                                Ok(c) => c,
                                Err(e) => {
                                    let _ = sender.blocking_send(Err(e));
                                    return;
                                }
                            };
                            if commit.seq as u64 <= head {
                                continue;
                            }
                            bytes += match encode_log_line(&commit) {
                                Ok(t) => t.encode_utf16().count(),
                                Err(e) => {
                                    let _ = sender.blocking_send(Err(e));
                                    return;
                                }
                            };
                            chunk.push(commit);
                            if bytes >= PULL_BYTES {
                                if sender
                                    .blocking_send(Ok(std::mem::take(&mut chunk)))
                                    .is_err()
                                {
                                    return;
                                }
                                bytes = 0;
                            }
                        }
                        if !chunk.is_empty() {
                            let _ = sender.blocking_send(Ok(chunk));
                        }
                    });
                    while let Some(chunk) = receiver.recv().await {
                        store.apply_batch(chunk?).await?;
                    }
                }
                let store = target
                    .as_ref()
                    .ok_or_else(|| invalid("sync: missing restored store"))?;
                head = store.meta().await?.last_seq as u64;
                if head != to {
                    return Err(invalid(
                        "sync: remote body does not reach its declared head",
                    ));
                }
                self.saved(id, head).await?;
            }
            if target.is_none() {
                target = Some(
                    open_target(
                        empty
                            .take()
                            .ok_or_else(|| invalid("sync: no empty target database"))?,
                    )
                    .await?,
                );
            }
            let store = target
                .as_ref()
                .ok_or_else(|| invalid("sync: missing target"))?;
            let meta = store.meta().await?;
            if (meta.last_seq as u64) < remote_head(&listing)? {
                return Err(invalid("sync: remote history is unavailable"));
            }
            self.saved(id, meta.last_seq as u64).await?;
            trim_outbox(store.database(), meta.last_seq).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            if owned && let Some(store) = target {
                let _ = store.database().close().await;
            }
            return Err(e);
        }
        target.ok_or_else(|| Error::Invalid("sync: missing restored target".into()))
    }
}
