//! Upload, pull and restore contracts exercised through real SQLite and the directory remote.
use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    sqlite::{Options, SqliteDatabase},
    store::{ColumnRecord, ColumnType, Query, StoreEngine, TableDefinition},
    sync::{
        self, AsyncRemote, FileSystemSyncRemote, MemorySyncState, SyncClient, SyncStateStore,
        UploadResult,
    },
};
use serde_json::json;
use std::sync::Arc;
fn table() -> Result<TableDefinition> {
    TableDefinition::new(
        "items",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("body".into(), ColumnRecord::new(ColumnType::Str)),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )
}
async fn source() -> Result<StoreEngine> {
    let store =
        StoreEngine::open(SqliteDatabase::open(":memory:", Options::default()).await?).await?;
    store
        .database()
        .call(|c| {
            c.execute("UPDATE chord_meta SET outbox=1", [])?;
            Ok(())
        })
        .await?;
    store.commit(table()?.declaration("app")?).await?;
    Ok(store)
}
#[test]
fn history_resumes_inside_batches_and_selects_latest_restore_archive() -> Result<()> {
    let batch = |after, to, archive| sync::BatchInfo {
        after,
        to,
        archive,
        device: "a".into(),
        folded: None,
    };
    let listing = sync::RemoteListing {
        batches: vec![
            batch(0, 5, None),
            batch(5, 9, Some(true)),
            batch(9, 12, None),
        ],
        snapshots: vec![3],
    };
    let resumed = sync::remote_history(&listing, Some(7))?;
    assert_eq!(
        resumed.batches.iter().map(|b| b.after).collect::<Vec<_>>(),
        vec![5, 9]
    );
    // A snapshot at 3 is not on this chain; an empty restore cannot invent its missing edge.
    assert_eq!(sync::remote_history(&listing, None)?.head, 3);
    let mut restore = listing;
    restore.snapshots.clear();
    let history = sync::remote_history(&restore, None)?;
    assert_eq!(history.snapshot, None);
    assert_eq!(history.batches[0].after, 5);
    assert_eq!(history.head, 12);
    Ok(())
}
#[tokio::test]
async fn async_upload_batches_at_eight_mib_and_pull_replays_to_sqlite() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let remote = Arc::new(FileSystemSyncRemote::new(dir.path()));
    let state = Arc::new(MemorySyncState::default());
    let client = SyncClient::new(remote.clone(), state.clone(), "writer");
    let store = source().await?;
    let table = table()?;
    for n in 0..18 {
        store
            .commit(vec![table.put(
                "app",
                json!({"id":format!("{n:03}"),"body":"x".repeat(512*1024)}),
            )?])
            .await?;
    }
    assert_eq!(
        client.upload("s", &store).await?,
        UploadResult::Uploaded { to: 19 }
    );
    let listing = remote.list("s")?;
    assert_eq!(listing.batches.len(), 2);
    for batch in listing.batches {
        let file = remote
            .get_batch("s", batch.after)?
            .ok_or_else(|| pi_rs_chord::Error::Invalid("missing batch".into()))?;
        let commits = sync::BatchReader::new(file).collect::<Result<Vec<_>>>()?;
        let size = commits
            .iter()
            .map(|c| serde_json::to_string(&c.writes).map(|s| s.encode_utf16().count() + 64))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .sum::<usize>();
        assert!(size <= sync::UPLOAD_BYTES || commits.len() == 1);
    }
    assert_eq!(store.meta().await?.outbox_floor, 19);
    let reader = SyncClient::new(remote, Arc::new(MemorySyncState::default()), "reader");
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let restored = reader.pull("s", None, Some(db)).await?;
    assert_eq!(restored.meta().await?.last_seq, 19);
    assert!(restored.meta().await?.outbox);
    assert_eq!(restored.count(Query::new("app", table)?).await?, 18);
    assert_eq!(client.upload("s", &store).await?, UploadResult::Idle);
    assert_eq!(state.get("s".into()).await?.synced_head, Some(19));
    store.database().close().await?;
    restored.database().close().await
}
#[tokio::test]
async fn unsynced_local_changes_refuse_pull_and_unread_remote_bodies_can_be_dropped() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let remote = Arc::new(FileSystemSyncRemote::new(dir.path()));
    let store = source().await?;
    let writer = SyncClient::new(remote.clone(), Arc::new(MemorySyncState::default()), "a");
    writer.upload("s", &store).await?;
    let target =
        StoreEngine::open(SqliteDatabase::open(":memory:", Options::default()).await?).await?;
    let reader_state = Arc::new(MemorySyncState::default());
    let reader = SyncClient::new(remote.clone(), reader_state.clone(), "b");
    reader.pull("s", Some(target.clone()), None).await?;
    target
        .commit(vec![
            table()?.put("app", json!({"id":"local","body":"no"}))?,
        ])
        .await?;
    assert!(reader.pull("s", Some(target.clone()), None).await.is_err());
    assert_eq!(target.meta().await?.last_seq, 2);
    assert_eq!(reader_state.get("s".into()).await?.synced_head, Some(1));
    // Dropping an unread async body must terminate its owned producer.
    let body = AsyncRemote::get_batch(remote.as_ref(), "s".into(), 0).await?;
    drop(body);
    store.database().close().await?;
    target.database().close().await
}
#[tokio::test]
async fn outbox_gap_uploads_archive_and_restores_only_an_empty_target() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let remote = Arc::new(FileSystemSyncRemote::new(dir.path()));
    let store = source().await?;
    pi_rs_chord::store::trim_outbox(store.database(), 1).await?;
    let writer = SyncClient::new(remote.clone(), Arc::new(MemorySyncState::default()), "a");
    assert_eq!(
        writer.upload("s", &store).await?,
        UploadResult::Uploaded { to: 1 }
    );
    assert_eq!(remote.list("s")?.batches[0].archive, Some(true));
    let reader = SyncClient::new(remote, Arc::new(MemorySyncState::default()), "b");
    let restored = reader
        .pull(
            "s",
            None,
            Some(SqliteDatabase::open(":memory:", Options::default()).await?),
        )
        .await?;
    assert_eq!(restored.meta().await?.last_seq, 1);
    assert!(restored.meta().await?.outbox);
    store.database().close().await?;
    restored.database().close().await
}

struct TruncatedOnce {
    remote: Arc<FileSystemSyncRemote>,
    first: std::sync::atomic::AtomicBool,
}
impl AsyncRemote for TruncatedOnce {
    fn list(&self, id: String) -> sync::SyncFuture<sync::RemoteListing> {
        AsyncRemote::list(self.remote.as_ref(), id)
    }
    fn get_snapshot(&self, id: String, to: u64) -> sync::SyncFuture<Option<sync::Body>> {
        AsyncRemote::get_snapshot(self.remote.as_ref(), id, to)
    }
    fn put_batch(
        &self,
        id: String,
        batch: sync::BatchInfo,
        body: sync::Body,
    ) -> sync::SyncFuture<sync::PutBatchResult> {
        AsyncRemote::put_batch(self.remote.as_ref(), id, batch, body)
    }
    fn get_batch(&self, id: String, after: u64) -> sync::SyncFuture<Option<sync::Body>> {
        let remote = self.remote.clone();
        let truncate = self.first.swap(false, std::sync::atomic::Ordering::AcqRel);
        Box::pin(async move {
            let Some(body) = AsyncRemote::get_batch(remote.as_ref(), id, after).await? else {
                return Ok(None);
            };
            if !truncate {
                return Ok(Some(body));
            }
            let mut bytes = body.collect().await?;
            bytes.truncate(bytes.len().saturating_sub(8));
            Ok(Some(sync::Body::from_bytes(bytes)))
        })
    }
}
#[tokio::test]
async fn truncated_gzip_pull_keeps_four_mib_prefix_and_retries_remaining_commits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let remote = Arc::new(FileSystemSyncRemote::new(dir.path()));
    let store = source().await?;
    let table = table()?;
    for n in 0..10 {
        store
            .commit(vec![table.put(
                "app",
                json!({"id":format!("{n:03}"),"body":"x".repeat(512*1024)}),
            )?])
            .await?;
    }
    SyncClient::new(
        remote.clone(),
        Arc::new(MemorySyncState::default()),
        "writer",
    )
    .upload("s", &store)
    .await?;
    let reader_state = Arc::new(MemorySyncState::default());
    let reader = SyncClient::new(
        Arc::new(TruncatedOnce {
            remote,
            first: std::sync::atomic::AtomicBool::new(true),
        }),
        reader_state.clone(),
        "reader",
    );
    let target =
        StoreEngine::open(SqliteDatabase::open(":memory:", Options::default()).await?).await?;
    assert!(reader.pull("s", Some(target.clone()), None).await.is_err());
    let head = target.meta().await?.last_seq;
    assert!(
        head > 1 && head < 11,
        "first durable chunk must survive failed gzip footer: {head}"
    );
    assert_eq!(reader_state.get("s".into()).await?.pulling, Some(11));
    let restored = reader.pull("s", Some(target.clone()), None).await?;
    assert_eq!(restored.meta().await?.last_seq, 11);
    assert_eq!(restored.count(Query::new("app", table)?).await?, 10);
    assert_eq!(reader_state.get("s".into()).await?.pulling, None);
    target.database().close().await?;
    store.database().close().await
}

#[tokio::test]
async fn remote_empty_reports_at_synced_and_resumes_with_archive_after_delete() -> Result<()> {
    let first_dir = tempfile::tempdir()?;
    let first_remote = Arc::new(FileSystemSyncRemote::new(first_dir.path()));
    let writer_state = Arc::new(MemorySyncState::default());
    let writer = SyncClient::new(first_remote, writer_state.clone(), "laptop");
    let store = source().await?;
    let table = table()?;
    for n in 0..4 {
        store
            .commit(vec![
                table.put("app", json!({"id": format!("{n:03}"), "body": "seed"}))?,
            ])
            .await?;
    }
    assert_eq!(
        writer.upload("s", &store).await?,
        UploadResult::Uploaded { to: 5 }
    );
    store
        .commit(vec![
            table.put("app", json!({"id": "004", "body": "next"}))?,
        ])
        .await?;

    // Remote lost its copy while local bookmark still says syncedHead = 5.
    let emptied_dir = tempfile::tempdir()?;
    let emptied_remote = Arc::new(FileSystemSyncRemote::new(emptied_dir.path()));
    let rewriter = SyncClient::new(emptied_remote.clone(), writer_state.clone(), "laptop");
    assert_eq!(
        rewriter.upload("s", &store).await?,
        UploadResult::RemoteEmpty { at: 5 }
    );

    // Clearing bookmark makes the next upload send the full store as an archive.
    writer_state.delete("s".into()).await?;
    assert_eq!(
        rewriter.upload("s", &store).await?,
        UploadResult::Uploaded { to: 6 }
    );
    let listing = emptied_remote.list("s")?;
    assert_eq!(listing.batches.len(), 1);
    assert_eq!(listing.batches[0].after, sync::LOG_START);
    assert_eq!(listing.batches[0].to, 6);
    assert_eq!(listing.batches[0].archive, Some(true));

    let reader = SyncClient::new(
        emptied_remote,
        Arc::new(MemorySyncState::default()),
        "replica",
    );
    let restored = reader
        .pull(
            "s",
            None,
            Some(SqliteDatabase::open(":memory:", Options::default()).await?),
        )
        .await?;
    assert_eq!(restored.meta().await?.last_seq, 6);
    assert_eq!(restored.count(Query::new("app", table)?).await?, 5);
    store.database().close().await?;
    restored.database().close().await
}
