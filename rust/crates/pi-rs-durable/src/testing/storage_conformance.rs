//! Backend-independent checks of Durable's record, history, and receipt contracts.

use serde_json::{Value, json};

use crate::{
    ids::{MAX_SAFE_ID, ROOT_CONVERSATION_ID, Seq},
    storage::{Storage, StorageError, StorageResult},
    storage_types::*,
};

type Case = fn(&dyn Storage) -> StorageResult<()>;

/// Runs the storage contract against independent, empty fixtures. The constructor is
/// called once per case; callers own any external fixture cleanup.
pub fn run_storage_conformance<S: Storage>(
    mut fresh: impl FnMut() -> StorageResult<S>,
) -> StorageResult<()> {
    let cases: &[Case] = &[
        identities,
        receipts,
        record_indexes,
        table_cursor_orders,
        task_waits_and_wakes,
        submission_settlement_shapes,
        fork_pages,
        rewindable_documents,
        latest_documents,
        copy_and_lifecycle_failures,
        current_only_copy,
        exact_document_scope,
        closed,
    ];
    for case in cases {
        case(&fresh()?)?;
    }
    Ok(())
}

fn write(value: Value) -> StorageResult<StorageWrite> {
    Ok(serde_json::from_value(value)?)
}

fn batch(values: impl IntoIterator<Item = Value>) -> StorageResult<Vec<StorageWrite>> {
    values.into_iter().map(write).collect()
}

fn one(s: &dyn Storage, writes: Vec<StorageWrite>) -> StorageResult<Seq> {
    let mut receipts = Vec::new();
    let result = s.commit(&[writes], &mut |index, seq| receipts.push((index, seq)))?;
    match result {
        CommitResult::Stored { stored: 1 } => {
            assert_eq!(
                receipts.len(),
                1,
                "one stored batch must receive exactly one receipt"
            );
            let (index, seq) = receipts[0];
            assert_eq!(index, 0);
            assert!(seq > 0);
            Ok(seq)
        }
        CommitResult::Stored { stored } => Err(StorageError::Backend(format!(
            "expected one stored batch, got {stored}"
        ))),
        CommitResult::Refused { stored, refused } => {
            assert_eq!(stored, 0);
            assert!(receipts.is_empty());
            Err(refused)
        }
    }
}

fn put(s: &dyn Storage, values: impl IntoIterator<Item = Value>) -> StorageResult<Seq> {
    one(s, batch(values)?)
}

fn root(s: &dyn Storage) -> StorageResult<Seq> {
    put(s, [conversation(ROOT_CONVERSATION_ID)])
}

fn conversation(id: u64) -> Value {
    json!({"type":"conversation","value":{"id":id}})
}

fn entry(id: u64, conversation: u64, head: Option<u64>) -> Value {
    let mut value =
        json!({"type":"entry","value":{"id":id,"conversationId":conversation,"kind":"note"}});
    if let Some(head) = head {
        value["value"]["head"] = json!(head);
    }
    value
}

fn task(id: u64, conversation: u64, kind: &str, status: &str, background: bool) -> Value {
    json!({"type":"task","value":{"id":id,"conversationId":conversation,"kind":kind,"version":1,
        "input":{"id":id},"state":{"status":status,"checkpoint":{"phase":"ready"}},
        "background":background,"abortRequested":false,"memos":{"winner":"first"}}})
}

fn submission(id: u64, conversation: u64, request: Option<&str>, status: &str) -> Value {
    let mut value = json!({"type":"submission","value":{"id":id,"conversationId":conversation,"type":"input","status":status}});
    if let Some(request) = request {
        value["value"]["requestId"] = json!(request);
    }
    value
}

fn refuse<T: std::fmt::Debug>(result: StorageResult<T>, message: &str) {
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.to_string().contains(message)),
        "expected {message:?}, got {result:?}"
    );
}

fn identities(s: &dyn Storage) -> StorageResult<()> {
    assert_eq!(s.mint_id()?, 2);
    root(s)?;
    assert_eq!(s.conversation(1)?.map(|record| record.id), Some(1));
    refuse(root(s), "ID 1 already belongs to conversation");
    put(s, [entry(100, 1, None)])?;
    assert_eq!(s.mint_id()?, 101);
    refuse(
        put(s, [task(100, 1, "pending", "pending", false)]),
        "ID 100 already belongs to entry",
    );
    refuse(
        put(s, [conversation(200), conversation(200)]),
        "ID 200 is written more than once",
    );
    assert!(s.conversation(200)?.is_none());
    put(s, [entry(MAX_SAFE_ID, 1, None)])?;
    assert_eq!(
        s.entry(MAX_SAFE_ID)?.map(|record| record.entry.id),
        Some(MAX_SAFE_ID)
    );
    refuse(s.mint_id(), "ID space is exhausted");
    refuse(s.mint_id(), "ID space is exhausted");
    Ok(())
}

fn receipts(s: &dyn Storage) -> StorageResult<()> {
    let base = root(s)?;
    let mut receipts = Vec::new();
    let mut visible = None;
    let result = s.commit(
        &[batch([entry(10, 1, None)])?, batch([entry(11, 1, None)])?],
        &mut |i, seq| {
            receipts.push((i, seq));
            if i == 0 {
                visible = Some(s.entry(11));
            }
        },
    )?;
    assert!(matches!(result, CommitResult::Stored { stored: 2 }));
    assert_eq!(receipts.len(), 2);
    assert_eq!((receipts[0].0, receipts[1].0), (0, 1));
    assert!(base < receipts[0].1 && receipts[0].1 < receipts[1].1);
    assert_eq!(
        visible.transpose()?.flatten().map(|entry| entry.commit_seq),
        Some(receipts[1].1)
    );
    assert_eq!(
        s.entry(10)?.map(|entry| entry.commit_seq),
        Some(receipts[0].1)
    );

    receipts.clear();
    let result = s.commit(
        &[
            batch([entry(20, 1, None)])?,
            batch([task(21, 1, "refused", "pending", false), entry(20, 1, None)])?,
            batch([entry(22, 1, None)])?,
        ],
        &mut |i, seq| receipts.push((i, seq)),
    )?;
    assert!(
        matches!(result, CommitResult::Refused { stored: 1, ref refused } if refused.to_string().contains("ID 20 already belongs to entry"))
    );
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        s.entry(20)?.map(|entry| entry.commit_seq),
        Some(receipts[0].1)
    );
    assert!(s.task(21)?.is_none());
    assert!(s.entry(22)?.is_none());
    assert!(put(s, [entry(22, 1, None)])? > receipts[0].1);

    put(
        s,
        [create(
            30,
            "staged",
            None,
            DocumentScope::Session,
            None,
            1,
            json!({"n":1}),
        )?],
    )?;
    receipts.clear();
    let result = s.commit(
        &[
            batch([entry(31, 1, None)])?,
            batch([entry(32, 1, None), retire(30), retire(30)])?,
            batch([entry(33, 1, None)])?,
        ],
        &mut |i, seq| receipts.push((i, seq)),
    )?;
    assert!(
        matches!(result, CommitResult::Refused { stored: 1, ref refused } if refused.to_string().contains("retired more than once"))
    );
    assert_eq!(receipts.len(), 1);
    assert!(s.entry(31)?.is_some());
    assert!(s.entry(32)?.is_none());
    assert!(s.entry(33)?.is_none());
    assert!(s.document(30, DocumentPoint::Current)?.is_some());
    Ok(())
}

fn record_indexes(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    const TASK_KIND: &str = "task\0\u{1f980}";
    const REQUEST: &str = "x\0\u{1f980}";
    let mut null_entry = entry(13, 1, None);
    null_entry["value"]["data"] = Value::Null;
    let mut detached_entry = entry(14, 1, None);
    let data =
        json!({"__proto__":{"polluted":false},"constructor":{"label":"stored"},"toString":"value"});
    detached_entry["value"]["data"] = data.clone();
    let pending = task(7, 1, TASK_KIND, "pending", true);
    let waiting = json!({"type":"task","value":{"id":8,"conversationId":1,"kind":"waiting","version":1,
        "input":null,"owner":7,"state":{"status":"waiting","checkpoint":{"next":true},"on":[7],"policy":"any"},
        "background":false,"abortRequested":true}});
    let sleeping = json!({"type":"task","value":{"id":9,"conversationId":1,"kind":"sleeping","version":1,
        "input":null,"state":{"status":"sleeping","checkpoint":{"next":true},"signals":[{"signal":"app.answer","key":"a"}]},
        "background":false,"abortRequested":false}});
    put(
        s,
        [
            conversation(2),
            json!({"type":"conversation","value":{"id":3,"owner":{"conversationId":1,"taskId":7}}}),
            json!({"type":"conversation","value":{"id":4,"owner":{"conversationId":1,"taskId":8}}}),
            json!({"type":"conversation","value":{"id":5,"owner":{"conversationId":2,"taskId":7}}}),
            pending.clone(),
            waiting,
            sleeping,
            null_entry,
            detached_entry,
            submission(10, 1, Some(REQUEST), "queued"),
            submission(11, 2, Some(REQUEST), "queued"),
            submission(12, 1, None, "queued"),
            submission(15, 1, Some(""), "queued"),
        ],
    )?;
    assert_eq!(
        s.entry(13)?.and_then(|stored| stored.entry.data),
        Some(Value::Null)
    );
    let mut detached = s
        .entry(14)?
        .and_then(|stored| stored.entry.data)
        .ok_or_else(|| StorageError::Backend("storage omitted the detached data fixture".into()))?;
    detached["__proto__"]["polluted"] = Value::Bool(true);
    assert_ne!(detached, data);
    assert_eq!(
        s.entry(14)?.and_then(|stored| stored.entry.data),
        Some(data)
    );
    let filter = ConversationQuery {
        owner_conversation_id: Some(1),
        order: Some(ScanOrder::Descending),
        ..Default::default()
    };
    let first = s.scan_conversations(&filter, 1, None)?;
    assert_eq!(first.items.iter().map(|r| r.id).collect::<Vec<_>>(), [4]);
    assert!(first.next.is_some());
    let encoded = serde_json::to_string(&first.next)?;
    let cursor: Option<Cursor> = serde_json::from_str(&encoded)?;
    let query = ConversationQuery {
        order: None,
        ..filter.clone()
    };
    let second = s.scan_conversations(&query, 1, cursor.as_ref())?;
    assert_eq!(second.items.iter().map(|r| r.id).collect::<Vec<_>>(), [3]);
    assert!(second.next.is_none());
    let wrong = ConversationQuery {
        order: Some(ScanOrder::Ascending),
        ..filter
    };
    refuse(s.scan_conversations(&wrong, 1, cursor.as_ref()), "cursor");
    let other = ConversationQuery {
        owner_task_id: Some(7),
        ..Default::default()
    };
    assert_eq!(
        s.scan_conversations(&other, 10, None)?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [3, 5]
    );

    let filter = TaskQuery {
        conversation_id: Some(1),
        kind: Some(TASK_KIND.into()),
        status: Some(TaskStatus::Pending),
        background: Some(true),
        ..Default::default()
    };
    assert_eq!(
        s.scan_tasks(&filter, 10, None)?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [7]
    );
    for (status, id) in [(TaskStatus::Waiting, 8), (TaskStatus::Sleeping, 9)] {
        let filter = TaskQuery {
            status: Some(status),
            ..Default::default()
        };
        assert_eq!(
            s.scan_tasks(&filter, 10, None)?
                .items
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [id]
        );
    }
    assert_eq!(s.submission_by_request(1, REQUEST)?.map(|r| r.id), Some(10));
    assert_eq!(s.submission_by_request(2, REQUEST)?.map(|r| r.id), Some(11));
    assert_eq!(s.submission_by_request(1, "")?.map(|r| r.id), Some(15));

    let mut next = pending;
    next["value"]["state"] =
        json!({"status":"terminal","outcome":{"status":"completed","result":{"entryId":14}}});
    if let Some(value) = next["value"].as_object_mut() {
        value.remove("memos");
        value.insert("abortRequested".into(), Value::Bool(true));
    }
    let terminal = TaskQuery {
        status: Some(TaskStatus::Terminal),
        abort_requested: Some(true),
        ..Default::default()
    };
    put(
        s,
        [create(
            17,
            "single",
            None,
            DocumentScope::Session,
            None,
            1,
            json!({"n":1}),
        )?],
    )?;
    let mut moved = submission(10, 1, Some("moved"), "placed");
    moved["value"]["entry"] = json!(14);
    refuse(
        put(
            s,
            [
                next.clone(),
                moved.clone(),
                entry(16, 1, None),
                create(
                    18,
                    "single",
                    None,
                    DocumentScope::Session,
                    None,
                    1,
                    json!({}),
                )?,
            ],
        ),
        "already has a current incarnation",
    );
    assert!(s.entry(16)?.is_none());
    assert!(s.document(18, DocumentPoint::Current)?.is_none());
    assert!(s.scan_tasks(&terminal, 10, None)?.items.is_empty());
    assert_eq!(s.submission_by_request(1, REQUEST)?.map(|r| r.id), Some(10));
    assert_eq!(
        s.task(7)?.map(|r| r.state.status()),
        Some(TaskStatus::Pending)
    );
    put(s, [next, moved])?;
    assert!(s.scan_tasks(&filter, 10, None)?.items.is_empty());
    let rows = s.scan_tasks(&terminal, 10, None)?.items;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].memos.is_none());
    assert_eq!(s.submission_by_request(1, "moved")?.map(|r| r.id), Some(10));
    assert!(s.submission_by_request(1, REQUEST)?.is_none());
    let placed = SubmissionQuery {
        conversation_id: Some(1),
        status: Some(SubmissionStatus::Placed),
        ..Default::default()
    };
    assert_eq!(
        s.scan_submissions(&placed, 10, None)?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [10]
    );
    Ok(())
}

fn fork_pages(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let root_entries = put(
        s,
        [
            entry(21, 1, None),
            entry(10, 1, None),
            entry(20, 1, Some(10)),
        ],
    )?;
    put(
        s,
        [
            json!({"type":"conversation","value":{"id":30,"parent":{"conversationId":1,"at":20}}}),
            entry(31, 30, None),
            entry(32, 30, Some(31)),
        ],
    )?;
    let tail = put(
        s,
        [
            json!({"type":"conversation","value":{"id":40,"parent":{"conversationId":30,"at":31}}}),
            entry(41, 40, Some(41)),
            entry(42, 40, None),
        ],
    )?;
    let query = EntryQuery {
        conversation_id: 40,
        ..Default::default()
    };
    let mut cursor = None;
    let mut ids = Vec::new();
    for (ordinal, expected) in [[42, 41].as_slice(), &[31, 20], &[10]]
        .into_iter()
        .enumerate()
    {
        let page = s.scan_entries(&query, 2, cursor.as_ref())?;
        let found: Vec<_> = page.items.iter().map(|e| e.id).collect();
        assert_eq!(found, expected);
        ids.extend(found);
        cursor = page.next;
        if ordinal == 0 {
            put(s, [entry(90, 40, None)])?;
        }
    }
    assert!(cursor.is_none());
    assert_eq!(ids, [42, 41, 31, 20, 10]);
    let ascending = EntryQuery {
        order: Some(ScanOrder::Ascending),
        min_entry_id: Some(20),
        max_entry_id: Some(41),
        ..query.clone()
    };
    let first = s.scan_entries(&ascending, 2, None)?;
    assert_eq!(
        first.items.iter().map(|r| r.id).collect::<Vec<_>>(),
        [20, 31]
    );
    let remaining = EntryQuery {
        order: None,
        ..ascending
    };
    assert_eq!(
        s.scan_entries(&remaining, 2, first.next.as_ref())?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [41]
    );
    assert_eq!(
        s.find_latest_head_marker(40, None)?.map(|r| r.head),
        Some(Some(41))
    );
    assert_eq!(
        s.find_latest_head_marker(40, Some(31))?.map(|r| r.id),
        Some(20)
    );
    assert!(s.find_latest_head_marker(40, Some(10))?.is_none());
    assert_eq!(
        s.visible_entry(40, 10)?.map(|r| r.commit_seq),
        Some(root_entries)
    );
    assert_eq!(s.visible_entry(40, 42)?.map(|r| r.commit_seq), Some(tail));
    for id in [21, 32] {
        assert!(s.visible_entry(40, id)?.is_none());
    }
    assert!(s.visible_entry(1, 42)?.is_none());
    // A second fork branches from an entry inherited by its parent, before the parent's own fork point.
    put(
        s,
        [json!({"type":"conversation","value":{"id":50,"parent":{"conversationId":30,"at":10}}})],
    )?;
    let inherited = EntryQuery {
        conversation_id: 50,
        ..Default::default()
    };
    assert_eq!(
        s.scan_entries(&inherited, 10, None)?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [10]
    );
    assert!(s.find_latest_head_marker(50, None)?.is_none());
    assert!(s.visible_entry(50, 20)?.is_none());
    refuse(s.visible_entry(99, 10), "Unknown conversation");
    let malformed = serde_json::from_value(json!({"after":"not-an-id"}))?;
    refuse(
        s.scan_entries(&query, 10, Some(&malformed)),
        "Invalid storage cursor",
    );
    Ok(())
}

fn create(
    id: u64,
    kind: &str,
    key: Option<&str>,
    scope: DocumentScope,
    mode: Option<(&str, &str)>,
    version: u64,
    value: Value,
) -> StorageResult<Value> {
    let mut record = json!({"id":id,"kind":kind,"scope":serde_json::to_value(scope)?});
    if let Some(key) = key {
        record["key"] = json!(key);
    }
    if let Some((history, fork)) = mode {
        record["history"] = json!(history);
        record["fork"] = json!(fork);
    }
    Ok(
        json!({"type":"document.create","record":record,"content":{"kind":"base","version":version,"value":value}}),
    )
}

fn change(id: u64, version: u64, ops: Value) -> Value {
    json!({"type":"document.change","id":id,"content":{"kind":"delta","version":version,"ops":ops}})
}

fn retire(id: u64) -> Value {
    json!({"type":"document.retire","id":id})
}

fn copy(
    record: &Value,
    id: u64,
    conversation: u64,
    source: u64,
    at: DocumentPoint,
) -> StorageResult<Value> {
    let mut record = record["record"].clone();
    record["id"] = json!(id);
    record["scope"]["conversationId"] = json!(conversation);
    Ok(
        json!({"type":"document.copy","record":record,"source":{"id":source,"at":serde_json::to_value(at)?}}),
    )
}

fn rewindable_documents(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let scope = DocumentScope::Conversation { conversation_id: 1 };
    let mode = Some(("rewindable", "asOf"));
    let created = put(
        s,
        [create(
            2,
            "notes",
            None,
            scope,
            mode,
            1,
            json!({"items":["a"],"nested":{"n":1}}),
        )?],
    )?;
    let changed = put(
        s,
        [change(
            2,
            1,
            json!([["p", ["items"], 1, 0, ["b"]], ["s", ["nested", "n"], 2]]),
        )],
    )?;
    let checkpoint = put(
        s,
        [
            json!({"type":"document.change","id":2,"content":{"kind":"base","version":2,"value":{"n":3}}}),
        ],
    )?;
    for n in 4..12 {
        put(s, [change(2, 2, json!([["s", ["n"], n]]))])?;
    }
    let replaced = put(s, [change(2, 2, json!([["r",{"n":12}]]))])?;
    for n in 13..16 {
        put(s, [change(2, 2, json!([["s", ["n"], n]]))])?;
    }
    for (seq, version, deltas, expected) in [
        (created, 1, 0, json!({"items":["a"],"nested":{"n":1}})),
        (changed, 1, 1, json!({"items":["a","b"],"nested":{"n":2}})),
        (checkpoint, 2, 0, json!({"n":3})),
        (replaced, 2, 9, json!({"n":12})),
    ] {
        let found = s
            .document(2, DocumentPoint::Seq(seq))?
            .map(|d| (d.version, d.deltas_since_base, Value::Object(d.value)));
        assert_eq!(found, Some((version, deltas, expected)));
    }
    let current = s
        .document(2, DocumentPoint::Current)?
        .map(|d| (d.deltas_since_base, Value::Object(d.value)));
    assert_eq!(current, Some((12, json!({"n":15}))));
    let retired = put(
        s,
        [
            create(3, "notes", None, scope, mode, 1, json!({"n":99}))?,
            retire(2),
            change(2, 2, json!([["s", ["n"], 16]])),
        ],
    )?;
    assert!(s.document(2, DocumentPoint::Seq(retired))?.is_none());
    let address = DocumentAddress {
        scope,
        kind: "notes".into(),
        key: None,
    };
    assert_eq!(
        s.find_document(&address, DocumentPoint::Seq(changed))?
            .map(|r| (r.id, r.retired_at)),
        Some((2, Some(retired)))
    );
    assert_eq!(
        s.find_document(&address, DocumentPoint::Seq(retired))?
            .map(|r| (r.id, r.created_at)),
        Some((3, retired))
    );
    let query = DocumentQuery {
        scope,
        at: DocumentPoint::Seq(changed),
        kind: None,
    };
    assert_eq!(
        s.scan_documents(&query, 10, None)?
            .items
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [2]
    );
    Ok(())
}

fn latest_documents(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let start = put(
        s,
        [create(
            2,
            "latest",
            None,
            DocumentScope::Session,
            None,
            1,
            json!({"n":1}),
        )?],
    )?;
    put(s, [change(2, 1, json!([["s", ["n"], 2]]))])?;
    put(
        s,
        [
            json!({"type":"document.change","id":2,"content":{"kind":"base","version":2,"value":{"n":3}}}),
        ],
    )?;
    refuse(
        s.document(2, DocumentPoint::Seq(start)),
        "does not retain historical content",
    );
    refuse(
        put(s, [change(2, 1, json!([["s", ["n"], 9]]))]),
        "version transition requires a base",
    );
    assert_eq!(
        s.document(2, DocumentPoint::Current)?.map(|d| (
            d.version,
            d.deltas_since_base,
            Value::Object(d.value)
        )),
        Some((2, 0, json!({"n":3})))
    );
    let retired = put(s, [retire(2)])?;
    assert!(s.document(2, DocumentPoint::Current)?.is_none());
    let address = DocumentAddress {
        scope: DocumentScope::Session,
        kind: "latest".into(),
        key: None,
    };
    assert_eq!(
        s.find_document(&address, DocumentPoint::Seq(start))?
            .map(|r| r.retired_at),
        Some(Some(retired))
    );
    Ok(())
}

fn copy_and_lifecycle_failures(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let scope = DocumentScope::Conversation { conversation_id: 1 };
    let original = create(
        2,
        "forked",
        None,
        scope,
        Some(("rewindable", "asOf")),
        2,
        json!({"n":1}),
    )?;
    let original_seq = put(s, [original.clone()])?;
    let mut receipts = Vec::new();
    let result = s.commit(
        &[
            batch([change(2, 2, json!([["s", ["n"], 2]]))])?,
            batch([
                conversation(3),
                copy(&original, 4, 3, 2, DocumentPoint::Current)?,
                conversation(5),
                copy(&original, 6, 5, 2, DocumentPoint::Seq(original_seq))?,
            ])?,
        ],
        &mut |index, seq| receipts.push((index, seq)),
    )?;
    assert!(matches!(result, CommitResult::Stored { stored: 2 }));
    assert_eq!(receipts.len(), 2);
    assert_eq!(
        s.document(4, DocumentPoint::Current)?.map(|d| (
            d.version,
            d.deltas_since_base,
            Value::Object(d.value)
        )),
        Some((2, 0, json!({"n":2})))
    );
    assert_eq!(
        s.document(6, DocumentPoint::Current)?
            .map(|d| Value::Object(d.value)),
        Some(json!({"n":1}))
    );
    let rejected = put(
        s,
        [
            conversation(7),
            copy(&original, 8, 7, 2, DocumentPoint::Current)?,
            retire(2),
        ],
    );
    assert!(rejected.is_err());
    assert!(s.conversation(7)?.is_none());
    assert!(s.document(8, DocumentPoint::Current)?.is_none());
    assert!(s.document(2, DocumentPoint::Current)?.is_some());
    put(s, [change(2, 2, json!([["s", ["n"], 9]])), retire(2)])?;
    assert_eq!(
        s.document(4, DocumentPoint::Current)?
            .map(|d| Value::Object(d.value)),
        Some(json!({"n":2}))
    );

    let mut empty = original;
    empty["record"]["id"] = json!(9);
    let empty_at = put(s, [empty.clone(), retire(9)])?;
    assert!(s.document(9, DocumentPoint::Seq(empty_at))?.is_none());
    assert!(s.document(9, DocumentPoint::Current)?.is_none());
    empty["record"]["id"] = json!(10);
    let change = change(10, 2, json!([]));
    refuse(
        put(s, [empty.clone(), change.clone()]),
        "more than one content command",
    );
    refuse(put(s, [change, empty]), "more than one content command");
    assert!(s.document(10, DocumentPoint::Current)?.is_none());
    Ok(())
}

fn exact_document_scope(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    const KIND: &str = "cache\0\u{1f980}";
    let session = DocumentScope::Session;
    let task_scope = DocumentScope::Task { task_id: 12 };
    let convo = DocumentScope::Conversation { conversation_id: 1 };
    put(
        s,
        [
            task(12, 1, "task.owner", "pending", false),
            create(2, KIND, None, session, None, 1, json!({}))?,
            create(3, KIND, Some(""), session, None, 1, json!({}))?,
            create(4, KIND, Some("__proto__"), session, None, 1, json!({}))?,
            create(
                5,
                KIND,
                Some("__proto__"),
                convo,
                Some(("latest", "current")),
                1,
                json!({}),
            )?,
            create(6, KIND, Some("__proto__"), task_scope, None, 1, json!({}))?,
        ],
    )?;
    let lookup = |scope, key: Option<&str>| {
        s.find_document(
            &DocumentAddress {
                scope,
                kind: KIND.into(),
                key: key.map(str::to_owned),
            },
            DocumentPoint::Current,
        )
    };
    for (scope, key, id) in [
        (session, None, 2),
        (session, Some(""), 3),
        (session, Some("__proto__"), 4),
        (convo, Some("__proto__"), 5),
        (task_scope, Some("__proto__"), 6),
    ] {
        assert_eq!(lookup(scope, key)?.map(|r| r.id), Some(id));
    }
    let query = DocumentQuery {
        scope: session,
        at: DocumentPoint::Current,
        kind: Some(KIND.into()),
    };
    let first = s.scan_documents(&query, 2, None)?;
    assert_eq!(first.items.iter().map(|r| r.id).collect::<Vec<_>>(), [2, 3]);
    assert!(first.next.is_some());
    let next = s.scan_documents(&query, 2, first.next.as_ref())?;
    assert_eq!(next.items.iter().map(|r| r.id).collect::<Vec<_>>(), [4]);
    assert!(next.next.is_none());
    Ok(())
}

type IdScan<'a> = &'a dyn Fn(Option<ScanOrder>, Option<&Cursor>) -> StorageResult<Page<u64>>;

fn ids<T>(page: Page<T>, id: impl Fn(T) -> u64) -> Page<u64> {
    Page {
        items: page.items.into_iter().map(id).collect(),
        next: page.next,
    }
}

fn table_cursor_orders(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    put(
        s,
        [
            conversation(4),
            conversation(2),
            conversation(3),
            task(7, 1, "cursor", "pending", false),
            task(5, 1, "cursor", "pending", false),
            task(6, 2, "cursor", "pending", false),
            submission(10, 1, None, "queued"),
            submission(9, 1, None, "queued"),
            submission(8, 2, None, "queued"),
        ],
    )?;
    let conversation = |order, cursor: Option<&Cursor>| {
        s.scan_conversations(
            &ConversationQuery {
                order,
                ..Default::default()
            },
            2,
            cursor,
        )
        .map(|page| ids(page, |record| record.id))
    };
    let task = |order, cursor: Option<&Cursor>| {
        s.scan_tasks(
            &TaskQuery {
                order,
                ..Default::default()
            },
            2,
            cursor,
        )
        .map(|page| ids(page, |record| record.id))
    };
    let submission = |order, cursor: Option<&Cursor>| {
        s.scan_submissions(
            &SubmissionQuery {
                order,
                ..Default::default()
            },
            2,
            cursor,
        )
        .map(|page| ids(page, |record| record.id))
    };
    let cases: [(IdScan<'_>, &[u64]); 3] = [
        (&conversation, &[1, 2, 3, 4]),
        (&task, &[5, 6, 7]),
        (&submission, &[8, 9, 10]),
    ];
    for (scan, expected) in cases {
        for order in [
            None,
            Some(ScanOrder::Ascending),
            Some(ScanOrder::Descending),
        ] {
            let first = scan(order, None)?;
            let cursor: Option<Cursor> =
                serde_json::from_str(&serde_json::to_string(&first.next)?)?;
            let second = scan(None, cursor.as_ref())?;
            let mut expected = expected.to_vec();
            if order == Some(ScanOrder::Descending) {
                expected.reverse();
                refuse(scan(Some(ScanOrder::Ascending), cursor.as_ref()), "cursor");
            }
            assert_eq!(first.items, expected[..2]);
            assert_eq!(second.items, expected[2..]);
            assert!(second.next.is_none());
        }
    }
    let filtered_tasks = TaskQuery {
        conversation_id: Some(1),
        kind: Some("cursor".into()),
        ..Default::default()
    };
    let filtered_submissions = SubmissionQuery {
        conversation_id: Some(1),
        ..Default::default()
    };
    let page = s.scan_tasks(&filtered_tasks, 1, None)?;
    assert_eq!(page.items[0].id, 5);
    assert_eq!(
        s.scan_tasks(&filtered_tasks, 1, page.next.as_ref())?.items[0].id,
        7
    );
    let page = s.scan_submissions(&filtered_submissions, 1, None)?;
    assert_eq!(page.items[0].id, 9);
    assert_eq!(
        s.scan_submissions(&filtered_submissions, 1, page.next.as_ref())?
            .items[0]
            .id,
        10
    );
    Ok(())
}

fn task_waits_and_wakes(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let values: Vec<_> = [
        json!({"status":"sleeping","checkpoint":{"phase":"resume"},"until":61000,"signals":[{"signal":"app.answer","key":"a"}]}),
        json!({"status":"waiting","checkpoint":{"phase":"resume"},"on":[2],"policy":"any"}),
        json!({"status":"pending","checkpoint":{"phase":"resume"},"wake":{"reason":"tasks","ended":[2]}}),
        json!({"status":"running","checkpoint":{"phase":"resume"},"wake":{"reason":"signal","signal":"app.answer","key":"a"}}),
        json!({"status":"completing","outcome":{"status":"failed","error":{"message":"held","detail":null}}}),
    ].into_iter().enumerate().map(|(i, state)| {
        let mut value = task(i as u64 + 2, 1, "state", "pending", false);
        value["value"]["state"] = state;
        if i > 0 { value["value"]["owner"] = json!(2); }
        if i == 4 && let Some(record) = value["value"].as_object_mut() { record.remove("memos"); }
        value
    }).collect();
    put(s, values.iter().cloned())?;
    for (id, value) in (2..).zip(&values) {
        assert_eq!(
            s.task(id)?.map(serde_json::to_value).transpose()?,
            Some(value["value"].clone())
        );
    }
    let completing = TaskQuery {
        status: Some(TaskStatus::Completing),
        ..Default::default()
    };
    assert_eq!(s.scan_tasks(&completing, 10, None)?.items[0].id, 6);
    let mut terminal = values[4].clone();
    terminal["value"]["state"]["status"] = json!("terminal");
    put(s, [terminal.clone()])?;
    assert!(s.scan_tasks(&completing, 10, None)?.items.is_empty());
    assert_eq!(
        s.task(6)?.map(serde_json::to_value).transpose()?,
        Some(terminal["value"].clone())
    );
    Ok(())
}

fn submission_settlement_shapes(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let mut drafts = [
        submission(2, 1, Some("passive-done"), "queued"),
        submission(3, 1, Some("passive-closed"), "queued"),
        submission(4, 1, Some("continue-done"), "placed"),
        submission(5, 1, Some("continue-unanswered"), "placed"),
    ];
    for (index, draft) in drafts.iter_mut().enumerate() {
        draft["value"]["type"] = json!(if index < 2 { "write" } else { "continue" });
    }
    put(s, drafts.iter().cloned())?;
    for (state, record) in [(SubmissionStatus::Queued, 2), (SubmissionStatus::Placed, 4)] {
        let found = s.scan_submissions(
            &SubmissionQuery {
                status: Some(state),
                ..Default::default()
            },
            10,
            None,
        )?;
        assert_eq!(
            found.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [record, record + 1]
        );
    }
    drafts[0]["value"]["status"] = json!("done");
    drafts[0]["value"]["entry"] = json!(8);
    drafts[1]["value"]["status"] = json!("unanswered");
    drafts[1]["value"]["reason"] = json!("closed");
    drafts[1]["value"]["detail"] = json!({"retryable":false});
    drafts[2]["value"]["status"] = json!("done");
    drafts[2]["value"]["answer"] = json!(9);
    drafts[3]["value"]["status"] = json!("unanswered");
    drafts[3]["value"]["reason"] = json!("nothing_to_answer");
    put(s, drafts.iter().cloned())?;
    for (row, request) in drafts.iter().zip([
        "passive-done",
        "passive-closed",
        "continue-done",
        "continue-unanswered",
    ]) {
        assert_eq!(
            s.submission_by_request(1, request)?
                .map(serde_json::to_value)
                .transpose()?,
            Some(row["value"].clone())
        );
    }
    assert!(
        s.scan_submissions(
            &SubmissionQuery {
                status: Some(SubmissionStatus::Placed),
                ..Default::default()
            },
            10,
            None
        )?
        .items
        .is_empty()
    );
    Ok(())
}

fn current_only_copy(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    let scope = DocumentScope::Conversation { conversation_id: 1 };
    let latest = create(
        2,
        "current-only",
        None,
        scope,
        Some(("latest", "current")),
        4,
        json!({"n":1}),
    )?;
    let start = put(s, [latest.clone(), conversation(3)])?;
    put(s, [change(2, 4, json!([["s", ["n"], 2]]))])?;
    let asof = copy(&latest, 4, 3, 2, DocumentPoint::Seq(start))?;
    assert!(put(s, [asof]).is_err());
    let mut wrong_kind = copy(&latest, 4, 3, 2, DocumentPoint::Current)?;
    wrong_kind["record"]["kind"] = json!("different");
    assert!(put(s, [wrong_kind]).is_err());
    assert!(s.document(4, DocumentPoint::Current)?.is_none());
    put(s, [copy(&latest, 4, 3, 2, DocumentPoint::Current)?])?;
    put(s, [change(2, 4, json!([["s", ["n"], 3]])), retire(2)])?;
    assert_eq!(
        s.document(4, DocumentPoint::Current)?.map(|doc| (
            doc.version,
            doc.deltas_since_base,
            Value::Object(doc.value)
        )),
        Some((4, 0, json!({"n":2})))
    );
    Ok(())
}

fn closed(s: &dyn Storage) -> StorageResult<()> {
    root(s)?;
    s.close()?;
    s.close()?;
    refuse(s.conversation(ROOT_CONVERSATION_ID), "closed");
    refuse(root(s), "closed");
    refuse(s.mint_id(), "closed");
    Ok(())
}
