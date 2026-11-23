use pi_rs_chord::delta::{Op, Segment};
use pi_rs_chord::json::JsonValue;
use pi_rs_durable::storage::jsonl::{JsonlStorage, JsonlStorageOptions};
use pi_rs_durable::storage::memory::MemoryStorage;
use pi_rs_durable::storage::sqlite::{SqliteStorage, SqliteStorageOptions};
use pi_rs_durable::storage::*;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use tempfile::tempdir;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
fn exists<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| "missing test value".into())
}
fn root() -> StorageWrite {
    StorageWrite::Conversation {
        value: ConversationRecord {
            id: 1,
            parent: None,
            owner: None,
        },
    }
}
fn entry(id: u64, conversation_id: u64, head: Option<u64>) -> StorageWrite {
    StorageWrite::Entry {
        value: EntryRecord {
            id,
            conversation_id,
            kind: "message".into(),
            model: None,
            data: Some(Value::Null),
            head,
            edits: None,
            by_task_id: None,
        },
    }
}
fn create(id: u64, scope: DocumentScope, history: Option<DocumentHistory>) -> DocumentCreate {
    DocumentCreate {
        id,
        kind: "example".into(),
        scope,
        key: None,
        history,
        fork: history.map(|_| DocumentFork::Current),
    }
}
fn base(number: u64) -> DocumentContent {
    DocumentContent::Base {
        version: 1,
        value: JsonObject::from_iter([("n".into(), json!(number))]),
    }
}
fn delta(number: f64) -> DocumentContent {
    DocumentContent::Delta {
        version: 1,
        ops: vec![Op::Set(
            vec![Segment::Key("n".into())],
            JsonValue::Number(number),
        )],
    }
}
fn commit(
    storage: &dyn Storage,
    batches: &[Vec<StorageWrite>],
) -> Result<(CommitResult, Vec<(usize, u64)>)> {
    let mut receipts = Vec::new();
    let result = storage.commit(batches, &mut |i, seq| receipts.push((i, seq)))?;
    Ok((result, receipts))
}

#[test]
fn atomic_prefix_fork_cursor_and_copy_across_native_backends() -> Result {
    let dir = tempdir()?;
    let memory = MemoryStorage::new();
    let sqlite = SqliteStorage::in_memory()?;
    let jsonl = JsonlStorage::open(dir.path())?;
    for storage in [&memory as &dyn Storage, &sqlite, &jsonl] {
        let doc = create(
            4,
            DocumentScope::Conversation { conversation_id: 1 },
            Some(DocumentHistory::Rewindable),
        );
        let child = create(
            10,
            DocumentScope::Conversation { conversation_id: 8 },
            Some(DocumentHistory::Rewindable),
        );
        let (result, receipts) = commit(
            storage,
            &[
                vec![
                    root(),
                    entry(2, 1, Some(2)),
                    entry(3, 1, None),
                    StorageWrite::DocumentCreate {
                        record: doc.clone(),
                        content: base(1),
                    },
                ],
                vec![
                    entry(5, 1, Some(5)),
                    StorageWrite::DocumentChange {
                        id: 4,
                        content: delta(2.0),
                    },
                ],
                vec![
                    StorageWrite::Conversation {
                        value: ConversationRecord {
                            id: 8,
                            parent: Some(ConversationParent {
                                conversation_id: 1,
                                at: 3,
                            }),
                            owner: None,
                        },
                    },
                    entry(9, 8, None),
                    StorageWrite::DocumentCopy {
                        record: child.clone(),
                        source: DocumentCopySource {
                            id: 4,
                            at: DocumentPoint::Seq(1),
                        },
                    },
                ],
                vec![
                    entry(11, 8, None),
                    StorageWrite::DocumentCopy {
                        record: create(12, child.scope, child.history),
                        source: DocumentCopySource {
                            id: 4,
                            at: DocumentPoint::Current,
                        },
                    },
                    StorageWrite::DocumentChange {
                        id: 4,
                        content: base(3),
                    },
                ],
                vec![entry(13, 8, None)],
            ],
        )?;
        assert!(matches!(result, CommitResult::Refused { stored: 3, .. }));
        assert_eq!(receipts, vec![(0, 1), (1, 2), (2, 3)]);
        assert!(storage.entry(11)?.is_none());
        assert!(storage.entry(13)?.is_none());
        assert!(storage.visible_entry(8, 5)?.is_none());
        assert_eq!(exists(storage.visible_entry(8, 3)?)?.commit_seq, 1);
        assert_eq!(exists(storage.find_latest_head_marker(8, None)?)?.id, 2);
        let mut query = EntryQuery {
            conversation_id: 8,
            min_entry_id: None,
            max_entry_id: None,
            order: Some(ScanOrder::Ascending),
        };
        let first = storage.scan_entries(&query, 2, None)?;
        assert_eq!(
            first
                .items
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        query.order = Some(ScanOrder::Descending);
        assert!(matches!(
            storage.scan_entries(&query, 2, first.next.as_ref()),
            Err(StorageError::Request(_))
        ));
        query.order = None;
        assert_eq!(
            storage
                .scan_entries(&query, 2, first.next.as_ref())?
                .items
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            [9]
        );
        let doc_current = exists(storage.document(4, DocumentPoint::Current)?)?;
        assert_eq!(doc_current.value["n"].as_f64(), Some(2.0));
        assert_eq!(doc_current.deltas_since_base, 1);
        assert_eq!(
            exists(storage.document(10, DocumentPoint::Current)?)?.value["n"],
            json!(1)
        );
        assert_eq!(storage.mint_id()?, 11);
        storage.close()?;
        assert!(storage.entry(2).is_err());
    }
    Ok(())
}

#[test]
fn sqlite_driver_error_rolls_back_every_accepted_batch() -> Result {
    let dir = tempdir()?;
    let path = dir.path().join("store.db");
    let storage = SqliteStorage::open_with_options(
        &path,
        SqliteStorageOptions {
            prefix: Some("test_".into()),
        },
    )?;
    let sql = rusqlite::Connection::open(&path)?;
    sql.execute_batch("CREATE TRIGGER fail_entry BEFORE INSERT ON test_entries WHEN NEW.id = 3 BEGIN SELECT RAISE(ABORT, 'simulated SQLite write failure'); END")?;
    let mut receipts = Vec::new();
    let outcome = storage.commit(
        &[vec![root(), entry(2, 1, None)], vec![entry(3, 1, None)]],
        &mut |i, seq| receipts.push((i, seq)),
    );
    assert!(matches!(outcome, Err(StorageError::Sqlite(_))));
    assert!(receipts.is_empty());
    assert!(storage.conversation(1)?.is_none());
    assert!(storage.entry(2)?.is_none());
    let (next_id, next_seq): (String, i64) = sql.query_row(
        "SELECT next_id,next_seq FROM test_durable_metadata",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!((next_id, next_seq), ("2".into(), 1));
    sql.execute_batch("DROP TRIGGER fail_entry")?;
    assert_eq!(
        commit(&storage, &[vec![root(), entry(2, 1, None)]])?.1,
        [(0, 1)]
    );
    storage.close()?;
    let reopened = SqliteStorage::open_with_options(
        path,
        SqliteStorageOptions {
            prefix: Some("test_".into()),
        },
    )?;
    assert_eq!(exists(reopened.entry(2)?)?.entry.data, Some(Value::Null));
    assert_eq!(reopened.mint_id()?, 3);
    Ok(())
}

#[test]
fn sqlite_upgrades_version_one_and_keeps_rows() -> Result {
    let dir = tempdir()?;
    let path = dir.path().join("legacy.db");
    let storage = SqliteStorage::open(&path)?;
    let pending: TaskRecord = serde_json::from_value(
        json!({"id":2,"conversationId":1,"kind":"test café","version":1,"input":null,"background":false,"abortRequested":false,"state":{"status":"pending","checkpoint":null}}),
    )?;
    commit(
        &storage,
        &[vec![
            root(),
            StorageWrite::Task {
                value: pending.clone(),
            },
        ]],
    )?;
    storage.close()?;
    let database = rusqlite::Connection::open(&path)?;
    database.execute_batch("UPDATE durable_schema SET version=1; ALTER TABLE tasks RENAME TO saved_tasks;
        CREATE TABLE tasks (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, kind TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('pending','running','waiting','completing','terminal')),
            abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0,1)), background INTEGER NOT NULL CHECK (background IN (0,1)),
            record TEXT NOT NULL CHECK (json_valid(record))) STRICT;
        INSERT INTO tasks SELECT * FROM saved_tasks; DROP TABLE saved_tasks;")?;
    let reopened = SqliteStorage::open(&path)?;
    assert_eq!(reopened.task(2)?, Some(pending.clone()));
    let sleeping = TaskRecord {
        state: TaskState::Sleeping {
            checkpoint: Value::Null,
            until: Some(15),
            signals: None,
        },
        ..pending
    };
    assert_eq!(
        commit(
            &reopened,
            &[vec![StorageWrite::Task {
                value: sleeping.clone()
            }]]
        )?
        .1,
        [(0, 2)]
    );
    let version: u32 =
        database.query_row("SELECT version FROM durable_schema", [], |row| row.get(0))?;
    assert_eq!(version, 2);
    assert_eq!(
        reopened
            .scan_tasks(
                &TaskQuery {
                    status: Some(TaskStatus::Sleeping),
                    kind: Some(sleeping.kind.clone()),
                    ..TaskQuery::default()
                },
                5,
                None
            )?
            .items,
        vec![sleeping]
    );
    Ok(())
}

#[test]
fn jsonl_recovers_torn_and_unconfirmed_tails_without_exposing_them() -> Result {
    let dir = tempdir()?;
    let path = dir.path();
    let storage = JsonlStorage::open_with_options(
        path,
        JsonlStorageOptions {
            fsync: true,
            read_only: false,
        },
    )?;
    let record = create(2, DocumentScope::Session, None);
    assert_eq!(
        commit(
            &storage,
            &[vec![
                root(),
                StorageWrite::DocumentCreate {
                    record: record.clone(),
                    content: base(1)
                }
            ]]
        )?
        .1,
        [(0, 1)]
    );
    storage.close()?;
    let marker = path.join("main.jsonl");
    let doc = path.join("doc-2.jsonl");
    let original_marker = fs::read(&marker)?;
    let original_doc = fs::read(&doc)?;
    assert_eq!(
        String::from_utf8(original_marker.clone())?,
        "{\"format\":1,\"type\":\"commit\",\"seq\":1,\"writes\":[{\"type\":\"conversation\",\"value\":{\"id\":1}},{\"type\":\"document.create\",\"record\":{\"id\":2,\"kind\":\"example\",\"scope\":{\"kind\":\"session\"}},\"ordinal\":0}]}\n"
    );
    assert_eq!(
        String::from_utf8(original_doc.clone())?,
        "{\"format\":1,\"type\":\"record\",\"seq\":1,\"ordinal\":0,\"payload\":{\"type\":\"document\",\"id\":2,\"content\":{\"kind\":\"base\",\"version\":1,\"value\":{\"n\":1}}}}\n"
    );
    OpenOptions::new()
        .append(true)
        .open(&marker)?
        .write_all(b"{\"format\":1,\"type\":\"commit\",\"seq\":2")?;
    OpenOptions::new().append(true).open(&doc)?.write_all(b"{\"format\":1,\"type\":\"record\",\"seq\":2,\"ordinal\":0,\"payload\":{\"type\":\"document\",\"id\":2,\"content\":{\"kind\":\"base\",\"version\":1,\"value\":{\"n\":9}}}}\n\xff torn")?;
    let read_only = JsonlStorage::open_with_options(
        path,
        JsonlStorageOptions {
            fsync: false,
            read_only: true,
        },
    )?;
    assert_eq!(
        exists(read_only.document(2, DocumentPoint::Current)?)?.value["n"],
        json!(1)
    );
    assert!(matches!(
        read_only.commit(&[vec![entry(3, 1, None)]], &mut |_, _| {}),
        Err(StorageError::JsonlReadOnly(_))
    ));
    assert!(fs::read(&doc)?.len() > original_doc.len());
    let repaired = JsonlStorage::open(path)?;
    assert_eq!(fs::read(&marker)?, original_marker);
    assert_eq!(fs::read(&doc)?, original_doc);
    assert_eq!(
        commit(
            &repaired,
            &[vec![StorageWrite::DocumentChange {
                id: 2,
                content: base(8)
            }]]
        )?
        .1,
        [(0, 2)]
    );
    assert_eq!(fs::read_to_string(&doc)?.lines().count(), 1);
    repaired.close()?;
    let reopened = JsonlStorage::open(path)?;
    assert_eq!(
        exists(reopened.document(2, DocumentPoint::Current)?)?.value["n"],
        json!(8)
    );
    assert!(matches!(
        reopened.document(2, DocumentPoint::Seq(1)),
        Err(StorageError::Request(_))
    ));
    assert_eq!(
        exists(reopened.find_document(&record.address(), DocumentPoint::Seq(1))?)?.created_at,
        1
    );
    fs::remove_file(doc)?;
    assert!(matches!(
        JsonlStorage::open(path),
        Err(StorageError::JsonlCorruption(_))
    ));
    Ok(())
}

#[test]
fn jsonl_failed_marker_poison_and_reopen_do_not_publish_uncommitted_state() -> Result {
    let dir = tempdir()?;
    let path = dir.path();
    let storage = JsonlStorage::open(path)?;
    let marker = path.join("main.jsonl");
    fs::create_dir(&marker)?;
    let mut receipts = Vec::new();
    let writes = vec![
        root(),
        StorageWrite::DocumentCreate {
            record: create(2, DocumentScope::Session, None),
            content: base(1),
        },
    ];
    assert!(matches!(
        storage.commit(&[writes], &mut |index, seq| receipts.push((index, seq))),
        Err(StorageError::JsonlPoisoned(_))
    ));
    assert!(receipts.is_empty());
    assert!(matches!(
        storage.conversation(1),
        Err(StorageError::JsonlPoisoned(_))
    ));
    assert!(path.join("doc-2.jsonl").is_file());
    fs::remove_dir(marker)?;
    let recovered = JsonlStorage::open(path)?;
    assert!(recovered.conversation(1)?.is_none());
    assert_eq!(fs::metadata(path.join("doc-2.jsonl"))?.len(), 0);
    assert_eq!(commit(&recovered, &[vec![root()]])?.1, [(0, 1)]);
    Ok(())
}

#[test]
fn jsonl_reclaims_terminal_tasks_and_retired_current_only_documents() -> Result {
    let dir = tempdir()?;
    let path = dir.path();
    let storage = JsonlStorage::open(path)?;
    let pending: TaskRecord = serde_json::from_value(json!({
        "id":2,"conversationId":1,"kind":"worker","version":1,
        "input":null,"background":false,"abortRequested":false,
        "state":{"status":"pending","checkpoint":{}}
    }))?;
    let terminal = TaskRecord {
        state: TaskState::Terminal {
            outcome: TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: "controlled failure".into(),
                    detail: Some(Value::Null),
                },
                result: Some(Value::Null),
            },
        },
        ..pending.clone()
    };
    let old = create(3, DocumentScope::Session, None);
    let newer = create(4, DocumentScope::Session, None);
    let (_, receipts) = commit(
        &storage,
        &[
            vec![
                root(),
                StorageWrite::Task { value: pending },
                StorageWrite::DocumentCreate {
                    record: old.clone(),
                    content: base(1),
                },
            ],
            vec![
                StorageWrite::Task {
                    value: terminal.clone(),
                },
                StorageWrite::DocumentCreate {
                    record: newer,
                    content: base(2),
                },
                StorageWrite::DocumentRetire { id: 3 },
            ],
            vec![StorageWrite::DocumentChange {
                id: 4,
                content: delta(7.0),
            }],
        ],
    )?;
    assert_eq!(receipts, [(0, 1), (1, 2), (2, 3)]);
    assert!(!path.join("task-2.jsonl").exists());
    assert!(!path.join("doc-3.jsonl").exists());
    storage.close()?;
    let reopened = JsonlStorage::open(path)?;
    assert_eq!(reopened.task(2)?, Some(terminal));
    assert_eq!(
        exists(reopened.find_document(&old.address(), DocumentPoint::Seq(1))?)?.retired_at,
        Some(2)
    );
    assert_eq!(
        exists(reopened.find_document(&old.address(), DocumentPoint::Current)?)?.id,
        4
    );
    let stored = exists(reopened.document(4, DocumentPoint::Current)?)?;
    assert_eq!(stored.value["n"].as_f64(), Some(7.0));
    assert_eq!(stored.deltas_since_base, 1);
    Ok(())
}
