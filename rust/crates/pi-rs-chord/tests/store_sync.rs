use pi_rs_chord::{
    Result,
    sqlite::{Options, SqliteDatabase, apply_migrations_in},
    store::{self, OutboxCommit},
    sync::{self, BatchReader, FileSystemSyncRemote, PutBatchResult},
};
use serde_json::json;
use std::io::{Read, Write};
#[tokio::test]
async fn v1_store_upgrades_on_disk_and_live_uniqueness_outbox_floor_remain_enforced() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("store.db");
    let db = SqliteDatabase::open(&file, Options::default()).await?;
    db.transaction(|db|{apply_migrations_in(db,"chord_store_schema",&store::migrations()[..1])?;db.execute_batch("INSERT INTO chord_documents VALUES (2,'o','ns','k','','rewindable',1,NULL,NULL,1,1,'0123456789abcdef'); INSERT INTO chord_revisions VALUES(2,1,'base',1,'{\"s\":\"old\"}');")?;Ok(())}).await?;
    let meta = store::apply_store_schema(&db).await?;
    assert_eq!(meta.next_id, 2);
    db.transaction(|db|{assert!(db.execute_batch("INSERT INTO chord_documents(id,owner,namespace,kind,key,history,created_at,revision,version,fingerprint) VALUES(3,'o','ns','k','','latest',2,1,1,'f')").is_err());db.execute_batch("UPDATE chord_documents SET host='h',retired_at=2 WHERE id=2; INSERT INTO chord_documents(id,owner,namespace,kind,key,history,created_at,revision,version,fingerprint) VALUES(3,'o','ns','k','','latest',2,1,1,'f'); UPDATE chord_meta SET last_seq=2,outbox=1,next_id=4;INSERT INTO chord_outbox VALUES(1,1,3,'[]'),(2,1,4,'[]');")?;Ok(())}).await?;
    assert_eq!(store::read_outbox(&db, 0, 100).await?.len(), 2);
    store::trim_outbox(&db, 1).await?;
    assert!(store::read_outbox(&db, 0, 100).await.is_err());
    assert_eq!(store::read_outbox(&db, 1, 100).await?[0].next_id, 4);
    assert!(store::trim_outbox(&db, 3).await.is_err());
    db.close().await?;
    let raw = rusqlite::Connection::open(&file)?;
    assert_eq!(
        raw.query_row("SELECT version FROM chord_store_schema", [], |r| r
            .get::<_, u32>(0))?,
        3
    );
    assert_eq!(
        raw.query_row("SELECT host FROM chord_documents WHERE id=2", [], |r| {
            r.get::<_, String>(0)
        })?,
        "h"
    );
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM pragma_table_list WHERE name LIKE 'chord_%' AND strict=1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        6
    );
    Ok(())
}
fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
#[test]
fn gzip_jsonl_rejects_old_version_nonincreasing_sequences_truncation_and_bad_utf8() -> Result<()> {
    let commits = vec![
        OutboxCommit {
            seq: 1,
            next_id: 3,
            writes: vec![json!({"kind":"document.retire","id":2,"revision":2})],
        },
        OutboxCommit {
            seq: 4,
            next_id: 4,
            writes: vec![],
        },
    ];
    let bytes = sync::encode_batch(Vec::new(), commits.clone())?;
    assert_eq!(
        BatchReader::new(bytes.as_slice()).collect::<Result<Vec<_>>>()?,
        commits
    );
    assert_eq!(
        sync::encode_log_line(&commits[1])?,
        "{\"v\":2,\"seq\":4,\"nextId\":4,\"writes\":[]}\n"
    );
    for body in [
        b"{\"v\":1,\"seq\":1,\"nextId\":3,\"writes\":[]}\n".as_slice(),
        b"{\"v\":2,\"seq\":0,\"nextId\":3,\"writes\":[]}\n",
        b"{}",
        b"\xff\n",
    ] {
        assert!(
            BatchReader::new(gzip(body)?.as_slice())
                .collect::<Result<Vec<_>>>()
                .is_err()
        );
    }
    let duplicate = format!(
        "{}{}",
        sync::encode_log_line(&commits[0])?,
        sync::encode_log_line(&commits[0])?
    );
    assert!(
        BatchReader::new(gzip(duplicate.as_bytes())?.as_slice())
            .collect::<Result<Vec<_>>>()
            .is_err()
    );
    let mut truncated = bytes;
    truncated.truncate(truncated.len() - 5);
    assert!(
        BatchReader::new(truncated.as_slice())
            .collect::<Result<Vec<_>>>()
            .is_err()
    );
    Ok(())
}
#[test]
fn filesystem_remote_slots_are_single_writer_atomic_and_folded_bodies_disappear() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let remote = FileSystemSyncRemote::new(dir.path());
    for id in ["..", "../x", "/abs", "a/b", ".", ""] {
        assert!(sync::check_session_id(id).is_err());
    }
    let body = sync::encode_batch(
        Vec::new(),
        [OutboxCommit {
            seq: 1,
            next_id: 3,
            writes: vec![],
        }],
    )?;
    assert_eq!(
        remote.put_batch("session", 0, 1, "device", body.as_slice(), false)?,
        PutBatchResult::Created
    );
    assert_eq!(
        remote.put_batch("session", 0, 2, "other", &b""[..], false)?,
        PutBatchResult::Exists {
            to: 1,
            device: "device".into()
        }
    );
    assert_eq!(
        remote.put_batch("session", 2, 3, "other", &b""[..], false)?,
        PutBatchResult::NotHead { head: 1 }
    );
    assert!(
        dir.path()
            .join("session/slots/0000000000000000.json")
            .is_file()
    );
    assert_eq!(
        BatchReader::new(
            remote
                .get_batch("session", 0)?
                .ok_or_else(|| pi_rs_chord::Error::Invalid("missing batch".into()))?
        )
        .collect::<Result<Vec<_>>>()?
        .len(),
        1
    );
    remote.put_meta("session", &json!({"title":"copy"}))?;
    assert!(!dir.path().join("session/meta.json.tmp").exists());
    assert_eq!(remote.get_meta("session")?, Some(json!({"title":"copy"})));
    remote.put_snapshot("session", 1, body.as_slice())?;
    let mut bytes = Vec::new();
    if let Some(mut file) = remote.get_snapshot("session", 1)? {
        file.read_to_end(&mut bytes)?;
    }
    assert_eq!(bytes, body);
    remote.fold_batches("session", &[0])?;
    assert!(remote.get_batch("session", 0)?.is_none());
    assert_eq!(sync::remote_head(&remote.list("session")?)?, 1);
    assert_eq!(remote.sessions()?.len(), 1);
    remote.delete_snapshots("session", &[1])?;
    assert!(remote.get_snapshot("session", 1)?.is_none());
    Ok(())
}
