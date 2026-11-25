use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    node::app::{
        AppOptions, SwapJournal, SwapKind, SwapPhase, SwapRecoveryReport, recover_swaps,
        start_server,
    },
    sqlite::{Options, SqliteDatabase},
    store::{ColumnRecord, ColumnType, Query, StoreEngine, TableDefinition},
};
use serde_json::json;
use std::{fs, path::Path};

fn items_table() -> Result<TableDefinition> {
    TableDefinition::new(
        "items",
        1,
        IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Str))]),
        vec!["id".into()],
        IndexMap::new(),
    )
}

async fn seed_store_dir(dir: &Path, row_id: &str, blob_contents: &[u8]) -> Result<()> {
    fs::create_dir_all(dir.join("files"))?;
    fs::write(dir.join("files/blob.txt"), blob_contents)?;
    let db = SqliteDatabase::open(dir.join("store.sqlite"), Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let table = items_table()?;
    let mut writes = table.declaration("test")?;
    writes.push(table.put("test", json!({ "id": row_id }))?);
    store.commit(writes).await?;
    db.close().await?;
    Ok(())
}

async fn read_row_ids(dir: &Path) -> Result<Vec<String>> {
    let db = SqliteDatabase::open(dir.join("store.sqlite"), Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let table = items_table()?;
    let page = store.read(Query::new("test", table)?).await?;
    db.close().await?;
    Ok(page
        .rows
        .into_iter()
        .filter_map(|row| row.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect())
}

fn write_journal_file(journal_dir: &Path, journal: &SwapJournal) -> Result<()> {
    fs::create_dir_all(journal_dir)?;
    let path = journal_dir.join(format!("{}.swap.json", journal.id));
    fs::write(path, serde_json::to_vec(journal)?)?;
    Ok(())
}

#[tokio::test]
async fn interrupted_swapped_phase_rolls_back_sqlite_and_sibling_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let journal_dir = root.join("swaps");
    let target = root.join("target");
    let aside = root.join(".chord-aside-crash1");
    let stage = root.join(".chord-import-crash1");

    // Simulate crash right after target -> aside and stage -> target, before SQLite commit.
    seed_store_dir(&aside, "old-row", b"old-blob").await?;
    seed_store_dir(&target, "new-row", b"new-blob").await?;
    write_journal_file(
        &journal_dir,
        &SwapJournal {
            version: 1,
            id: "crash1".into(),
            kind: SwapKind::Replace,
            phase: SwapPhase::Swapped,
            target: target.clone(),
            stage: Some(stage.clone()),
            aside: aside.clone(),
            had_target: true,
        },
    )?;

    let report = recover_swaps(&journal_dir)?;
    assert_eq!(
        report,
        SwapRecoveryReport {
            rolled_back: 1,
            rolled_forward: 0
        }
    );
    assert_eq!(read_row_ids(&target).await?, ["old-row"]);
    assert_eq!(fs::read(target.join("files/blob.txt"))?, b"old-blob");
    assert!(!aside.exists());
    assert!(!stage.exists());
    assert_eq!(fs::read_dir(&journal_dir)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn interrupted_prepared_phase_restores_aside_and_removes_stage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let journal_dir = root.join("swaps");
    let target = root.join("target");
    let aside = root.join(".chord-aside-crash2");
    let stage = root.join(".chord-import-crash2");

    // Simulate crash between target -> aside and stage -> target.
    seed_store_dir(&aside, "old-row", b"old-blob").await?;
    seed_store_dir(&stage, "new-row", b"new-blob").await?;
    write_journal_file(
        &journal_dir,
        &SwapJournal {
            version: 1,
            id: "crash2".into(),
            kind: SwapKind::Replace,
            phase: SwapPhase::Prepared,
            target: target.clone(),
            stage: Some(stage.clone()),
            aside: aside.clone(),
            had_target: true,
        },
    )?;

    let report = recover_swaps(&journal_dir)?;
    assert_eq!(report.rolled_back, 1);
    assert_eq!(read_row_ids(&target).await?, ["old-row"]);
    assert_eq!(fs::read(target.join("files/blob.txt"))?, b"old-blob");
    assert!(!aside.exists());
    assert!(!stage.exists());
    Ok(())
}

#[tokio::test]
async fn interrupted_committed_phase_rolls_forward_and_cleans_aside() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let journal_dir = root.join("swaps");
    let target = root.join("target");
    let aside = root.join(".chord-aside-crash3");
    let stage = root.join(".chord-import-crash3");

    seed_store_dir(&aside, "old-row", b"old-blob").await?;
    seed_store_dir(&target, "new-row", b"new-blob").await?;
    write_journal_file(
        &journal_dir,
        &SwapJournal {
            version: 1,
            id: "crash3".into(),
            kind: SwapKind::Replace,
            phase: SwapPhase::Committed,
            target: target.clone(),
            stage: Some(stage.clone()),
            aside: aside.clone(),
            had_target: true,
        },
    )?;

    let report = recover_swaps(&journal_dir)?;
    assert_eq!(
        report,
        SwapRecoveryReport {
            rolled_back: 0,
            rolled_forward: 1
        }
    );
    assert_eq!(read_row_ids(&target).await?, ["new-row"]);
    assert_eq!(fs::read(target.join("files/blob.txt"))?, b"new-blob");
    assert!(!aside.exists());
    assert!(!stage.exists());
    Ok(())
}

#[tokio::test]
async fn app_server_reopen_recovers_interrupted_worker_swap_automatically() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("app");
    let options = || {
        AppOptions::new(
            "example",
            data_dir.clone(),
            json!({"kind":"owner"}),
            json!({"kind":"server"}),
        )
    };
    let server = start_server(options()).await?;
    let worker_dir = server.layout.worker_directory("session", "session|s1")?;
    let swaps_dir = server.layout.server.join("swaps");
    server.close().await?;

    let aside = worker_dir
        .parent()
        .expect("worker parent")
        .join(".chord-aside-restart");
    let stage = worker_dir
        .parent()
        .expect("worker parent")
        .join(".chord-import-restart");
    seed_store_dir(&aside, "persisted-old", b"old-file").await?;
    seed_store_dir(&worker_dir, "uncommitted-new", b"new-file").await?;
    write_journal_file(
        &swaps_dir,
        &SwapJournal {
            version: 1,
            id: "restart".into(),
            kind: SwapKind::Replace,
            phase: SwapPhase::Swapped,
            target: worker_dir.clone(),
            stage: Some(stage),
            aside: aside.clone(),
            had_target: true,
        },
    )?;

    let reopened = start_server(options()).await?;
    assert_eq!(read_row_ids(&worker_dir).await?, ["persisted-old"]);
    assert_eq!(fs::read(worker_dir.join("files/blob.txt"))?, b"old-file");
    assert!(!aside.exists());
    assert_eq!(fs::read_dir(&swaps_dir)?.count(), 0);
    reopened.close().await?;
    Ok(())
}
