use pi_rs_durable::{
    storage::{
        CommitResult, DocumentPoint, Storage, StorageResult, StorageWrite, jsonl::JsonlStorage,
        memory::MemoryStorage, sqlite::SqliteStorage, store::StoreStorage,
    },
    testing::storage_conformance::run_storage_conformance,
};
use serde_json::{Value, json};

#[test]
fn memory_conforms() -> StorageResult<()> {
    run_storage_conformance(|| Ok(MemoryStorage::new()))
}

#[test]
fn sqlite_conforms() -> StorageResult<()> {
    run_storage_conformance(SqliteStorage::in_memory)
}

#[test]
fn chord_store_conforms() -> StorageResult<()> {
    run_storage_conformance(StoreStorage::in_memory)
}

#[test]
fn jsonl_conforms() -> StorageResult<()> {
    let root = tempfile::tempdir()?;
    let mut case = 0;
    run_storage_conformance(|| {
        case += 1;
        JsonlStorage::open(root.path().join(format!("case-{case}.jsonl")))
    })
}

fn write(value: Value) -> StorageResult<StorageWrite> {
    Ok(serde_json::from_value(value)?)
}

#[test]
fn chord_store_persists_native_tables_revisions_and_rolls_back_backend_errors() -> StorageResult<()>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("chord.sqlite");
    let s = StoreStorage::open_named(&path, "app/testing")?;
    let sql = rusqlite::Connection::open(&path)?;
    sql.execute("UPDATE chord_meta SET outbox=1 WHERE singleton=1", [])?;
    let mut published = Vec::new();
    let batch = [vec![
        write(json!({"type":"conversation","value":{"id":1}}))?,
        write(
            json!({"type":"entry","value":{"id":2,"conversationId":1,"kind":"marker","head":2}}),
        )?,
        write(
            json!({"type":"document.create","record":{"id":3,"kind":"notes","scope":{"kind":"conversation","conversationId":1},"history":"rewindable","fork":"asOf"},"content":{"kind":"base","version":1,"value":{"n":1}}}),
        )?,
    ]];
    assert!(matches!(
        s.commit(&batch, &mut |_, seq| published.push(seq))?,
        CommitResult::Stored { stored: 1 }
    ));
    assert_eq!(published.len(), 1);
    let seq = published[0];
    let sql_seq = i64::try_from(seq)
        .map_err(|error| pi_rs_durable::storage::StorageError::Backend(error.to_string()))?;
    let row = sql.query_row(
        "SELECT commit_seq, \"record$v\" FROM \"app/testing:entries\" WHERE id=2",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    assert_eq!(row, (sql_seq, 1));
    let owner = sql.query_row(
        "SELECT owner,namespace,history FROM chord_documents WHERE id=3",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    assert_eq!(
        (owner.0.as_str(), owner.1.as_str(), owner.2.as_str()),
        ("app/testing", "conversation:1", "rewindable")
    );
    let revision = sql.query_row(
        "SELECT seq,kind,content FROM chord_revisions WHERE document_id=3",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    assert_eq!(
        (revision.0, revision.1.as_str(), revision.2.as_str()),
        (sql_seq, "base", "{\"n\":1}")
    );
    let outbox: String = sql.query_row(
        "SELECT writes FROM chord_outbox WHERE seq=?",
        [sql_seq],
        |r| r.get(0),
    )?;
    let outbox: Vec<Value> = serde_json::from_str(&outbox)?;
    assert!(outbox.iter().any(|w| w["kind"] == "document.create"));
    assert!(
        outbox
            .iter()
            .any(|w| w["table"] == "app/testing:entry_heads")
    );

    sql.execute_batch("CREATE TRIGGER failed_entry BEFORE INSERT ON \"app/testing:entries\" WHEN NEW.id=9 BEGIN SELECT RAISE(ABORT,'injected write failure'); END;")?;
    let batches = [
        vec![write(
            json!({"type":"document.change","id":3,"content":{"kind":"delta","version":1,"ops":[["s",["n"],2]]}}),
        )?],
        vec![write(
            json!({"type":"entry","value":{"id":9,"conversationId":1,"kind":"note"}}),
        )?],
    ];
    let error = s.commit(&batches, &mut |_, seq| published.push(seq));
    assert!(error.is_err_and(|error| error.to_string().contains("injected write failure")));
    assert_eq!(published, [seq]);
    assert!(s.entry(9)?.is_none());
    assert_eq!(
        s.document(3, DocumentPoint::Current)?
            .map(|d| Value::Object(d.value)),
        Some(json!({"n":1}))
    );
    assert_eq!(
        sql.query_row(
            "SELECT last_seq FROM chord_meta WHERE singleton=1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        sql_seq
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM chord_outbox", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    s.close()?;
    let reopened = StoreStorage::open_named(&path, "app/testing")?;
    assert_eq!(reopened.mint_id()?, 4);
    assert_eq!(reopened.entry(2)?.map(|e| e.commit_seq), Some(seq));
    assert_eq!(
        reopened
            .document(3, DocumentPoint::Seq(seq))?
            .map(|d| Value::Object(d.value)),
        Some(json!({"n":1}))
    );
    Ok(())
}

#[tokio::test]
async fn chord_store_claims_and_allocator_are_shared_but_records_are_scoped()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_chord::{
        sqlite::{Options, SqliteDatabase},
        store::StoreEngine,
    };
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db).await?;
    let first = StoreStorage::from_store(store.clone(), "first")?;
    let duplicate = StoreStorage::from_store(store.clone(), "first");
    assert!(duplicate.is_err_and(|error| error.to_string().contains("claimed")));
    let second = StoreStorage::from_store(store.clone(), "second")?;
    assert_eq!(first.mint_id()?, 2);
    assert_eq!(second.mint_id()?, 3);
    let batches = [vec![
        write(json!({"type":"conversation","value":{"id":1}}))?,
        write(
            json!({"type":"document.create","record":{"id":2,"kind":"notes","scope":{"kind":"session"}},"content":{"kind":"base","version":1,"value":{}}}),
        )?,
    ]];
    first.commit(&batches, &mut |_, _| {})?;
    assert!(first.document(2, DocumentPoint::Current)?.is_some());
    assert!(second.document(2, DocumentPoint::Current)?.is_none());
    assert!(second.conversation(1)?.is_none());
    first.close()?;
    assert!(store.claims().held());
    let reopened = StoreStorage::from_store(store.clone(), "first")?;
    assert!(reopened.conversation(1)?.is_some());
    assert_eq!(reopened.mint_id()?, 4);
    reopened.close()?;
    second.close()?;
    assert!(!store.claims().held());
    assert!(store.meta().await?.last_seq > 0);
    Ok(())
}
