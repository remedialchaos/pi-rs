#![forbid(unsafe_code)]

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_rs_durable::documents::{
    DocumentError, DocumentSemantics, InitialDocument, address_id, document_create,
    materialize_document_value, resolve_address,
};
use pi_rs_durable::errors::{BoxError, SessionError, error_message};
use pi_rs_durable::search::{SearchDocument, rank_documents, tokenize};
use pi_rs_durable::signals::{check_sleep, deliver_signal, read_signal, sleep_due, sleep_wake};
use pi_rs_durable::tasks::{PhaseHandler, TaskDefinition};
use pi_rs_durable::timers::{SetTimer, host_timers};
use pi_rs_durable::truncate::{
    TextTotals, TruncationLimit, TruncationOptions, format_size, truncate_head, truncate_head_of,
};
use pi_rs_durable::*;
use serde_json::{Value, json};

type TestResult = Result<(), BoxError>;

#[test]
fn id_and_entry_wire_preserve_safe_integer_null_and_custom_roles() -> TestResult {
    let input = json!({"id":MAX_SAFE_ID,"conversationId":1,"kind":"custom.message","data":null,"model":[{"role":"custom", "note":9}]});
    let record: EntryRecord = serde_json::from_value(input.clone())?;
    assert_eq!(record.id, 9_007_199_254_740_991);
    assert_eq!(record.conversation_id, ROOT_CONVERSATION_ID);
    assert_eq!(serde_json::to_value(&record)?, input);
    let token = define_entry::<Value>("custom.message")?;
    assert_eq!(token.payload(&record)?, Some(Value::Null));
    assert!(!ASSISTANT_ENTRY.matches(Some(&record)));
    assert!(define_entry::<()>("").is_err());
    Ok(())
}

#[test]
fn storage_failure_exposes_cause_but_not_in_public_message() {
    let inner = std::io::Error::other("disk fault");
    let error = SessionError::Failed(Box::new(inner));
    assert_eq!(
        error_message(&error),
        "Session failed after a storage error; close and reopen it"
    );
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("disk fault")
    );
    assert_eq!(
        error_message(&SessionError::ReadAfterWrite("Tx.task()")),
        "Tx.task() cannot read tables after the first table write"
    );
}

#[test]
fn truncation_keeps_whole_utf8_lines_and_tracks_trailing_lf_as_bytes() {
    let options = |lines, bytes| TruncationOptions {
        max_lines: Some(lines),
        max_bytes: Some(bytes),
    };
    let result = truncate_head("hello\nworld\n", options(2, 11));
    assert_eq!(
        (
            result.content,
            result.total_lines,
            result.output_lines,
            result.truncated_by
        ),
        ("hello\nworld", 2, 2, Some(TruncationLimit::Bytes))
    );
    let result = truncate_head("éé\nabc", options(3, 3));
    assert!(result.first_line_exceeds_limit);
    assert!(result.content.is_empty());
    let result = truncate_head_of(
        "éé\nabc",
        TextTotals {
            lines: 4,
            bytes: 25,
        },
        options(3, 4),
    );
    assert_eq!(
        (result.content, result.output_bytes, result.truncated_by),
        ("éé", 4, Some(TruncationLimit::Bytes))
    );
    let result = truncate_head("\n\n", options(1, 3));
    assert_eq!(
        (result.content, result.output_lines, result.truncated_by),
        ("", 1, Some(TruncationLimit::Lines))
    );
    assert_eq!(format_size(1_280), "1.3KB");
}

#[test]
fn document_family_migrates_detached_data_without_changing_fork_semantics() -> TestResult {
    let definition = DocDefinition {
        kind: "acme/doc".into(),
        version: 2,
        semantics: DocumentSemantics::RewindableConversation {
            fork: DocumentFork::AsOf,
        },
        initial: InitialDocument::Family(Arc::new(|seed| {
            Ok([("seed".into(), seed.clone())].into_iter().collect())
        })),
        migrate: Some(Arc::new(|mut old, from| {
            old.insert("from".into(), json!(from));
            Ok(old)
        })),
        checkpoint_when: Arc::new(|_, _, info| info.deltas_since_base == 4),
    };
    let doc = define_doc_family(definition)?;
    let address = resolve_address(&doc, Some(8), Some("x\"y".into()))?;
    assert_eq!(
        address_id(&address),
        r#"["acme/doc","conversation",8,"x\"y"]"#
    );
    let create = document_create(&doc, &address, 9)?;
    let original = doc.initial(Some(&json!({"stays":"intact"})))?;
    let migrated = materialize_document_value(&doc, &create, 1, original.clone())?;
    assert_eq!(migrated.get("from"), Some(&json!(1)));
    assert!(!original.contains_key("from"));
    assert!(matches!(
        materialize_document_value(&doc, &create, 3, original.clone()),
        Err(DocumentError::Newer { .. })
    ));
    let wrong_fork = DocumentCreate {
        fork: Some(DocumentFork::Current),
        ..create
    };
    assert!(matches!(
        materialize_document_value(&doc, &wrong_fork, 2, original),
        Err(DocumentError::Semantics { .. })
    ));
    Ok(())
}

fn signal(
    name: &str,
    version: u64,
    schema: Value,
) -> Result<SignalToken, pi_rs_durable::signals::SignalError> {
    define_signal(SignalDefinition {
        name: name.into(),
        version,
        schema,
        migrate: None,
    })
}

fn sleepy(signals: Vec<SignalKey>) -> Result<TaskRecord, serde_json::Error> {
    serde_json::from_value(
        json!({"id":2,"conversationId":1,"kind":"sleepy","version":1,"input":null,
        "background":false,"abortRequested":false,"state":{"status":"sleeping","checkpoint":{},"until":10,"signals":signals}}),
    )
}

#[test]
fn a_sleep_consumes_list_order_before_deadline_and_rejects_key_reuse() -> TestResult {
    let token = signal("save", 1, json!({"type":"integer"}))?;
    let task = sleepy(vec![token.key("first")?, token.key("second")?])?;
    assert_eq!(
        deliver_signal(Some(&task), &token, "first", json!("no"), 4).result,
        SignalResult::Invalid
    );
    let second = deliver_signal(Some(&task), &token, "second", json!(2), 5);
    let first = deliver_signal(second.record.as_ref(), &token, "first", json!(1), 6);
    let mut task = first
        .record
        .ok_or_else(|| std::io::Error::other("no delivered signal"))?;
    assert_eq!(
        deliver_signal(Some(&task), &token, "first", json!(99), 7).result,
        SignalResult::Duplicate
    );
    assert!(sleep_due(&task.state, task.memos.as_ref(), 8));
    let wake = sleep_wake(&task.state, task.memos.as_ref(), 10)
        .ok_or_else(|| std::io::Error::other("did not wake"))?;
    assert_eq!(
        wake.wake,
        WakeReason::Signal {
            signal: "save".into(),
            key: "first".into()
        }
    );
    task.memos
        .get_or_insert_with(JsonObject::new)
        .extend(wake.memos);
    assert!(check_sleep(&task.state, task.memos.as_ref(), task.id).is_err());
    assert_eq!(
        read_signal(task.memos.as_ref(), &token, "first")?,
        Some(SignalState::Value { value: json!(1) })
    );
    assert_eq!(
        read_signal(task.memos.as_ref(), &token, "second")?,
        Some(SignalState::Value { value: json!(2) })
    );
    Ok(())
}

#[test]
fn expired_keys_cannot_take_late_deliveries_and_only_valid_settlements_signal() -> TestResult {
    let token = signal("save", 1, json!({"type":"integer"}))?;
    let task = sleepy(vec![token.key("deadline")?])?;
    assert!(!sleep_due(&task.state, None, 9));
    let late = deliver_signal(Some(&task), &token, "deadline", json!(1), 10);
    assert_eq!(late.result, SignalResult::Closed);
    let expired = late
        .record
        .ok_or_else(|| std::io::Error::other("deadline was not marked"))?;
    assert_eq!(
        read_signal(expired.memos.as_ref(), &token, "deadline")?,
        Some(SignalState::Expired)
    );
    let settled = submission_settled()?;
    let mut value = json!({"id":5,"conversationId":1,"type":"input","status":"done","entry":3});
    assert_eq!(
        deliver_signal(Some(&task), settled, "receipt", value.clone(), 9).result,
        SignalResult::Invalid
    );
    value["answer"] = json!(4);
    assert_eq!(
        deliver_signal(Some(&task), settled, "receipt", value, 9).result,
        SignalResult::Delivered
    );
    assert!(signal("x", 1, json!({"items":{"type":["integer","bigint"]}})).is_err());
    Ok(())
}

#[test]
fn signal_migration_rechecks_the_value_against_the_new_schema() -> TestResult {
    let token = define_signal(SignalDefinition {
        name: "migrate".into(),
        version: 2,
        schema: json!({"type":"integer"}),
        migrate: Some(Arc::new(|input, _| {
            Ok(input
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .map_or(Value::Null, Value::from))
        })),
    })?;
    let memos = |value| {
        [(
            "pi.signal.migrate/key".to_owned(),
            json!({"value":value,"version":1}),
        )]
        .into_iter()
        .collect()
    };
    assert_eq!(
        read_signal(Some(&memos("42")), &token, "key")?,
        Some(SignalState::Value { value: json!(42) })
    );
    assert!(read_signal(Some(&memos("not-an-integer")), &token, "key").is_err());
    Ok(())
}

#[test]
fn multilingual_search_uses_dictionary_breaks_and_exact_name_priority() {
    for (text, expected) in [
        ("搜索仓库问题", vec!["搜索", "仓库", "问题"]),
        ("文書を検索", vec!["文書", "を", "検索"]),
        ("문서 검색", vec!["문서", "검색"]),
        ("ค้นหาเอกสาร", vec!["ค้นหา", "เอกสาร"]),
        ("fooค้นหาเอกสาร123bar", vec!["fooค้นหา", "เอกสาร123bar"]),
        ("foo๒ไทย123", vec!["foo๒ไทย123"]),
        ("한글검색foo", vec!["한글검색", "foo"]),
        ("ｶﾞｯﾂﾎﾟｰｽﾞ", vec!["ｶﾞｯﾂﾎﾟｰｽﾞ"]),
        ("Поиск документов", vec!["поиск", "документов"]),
        ("البحث عن الملفات", vec!["البحث", "عن", "الملفات"]),
        ("फ़ाइल खोजें", vec!["फ़ाइल", "खोजें"]),
        ("CAFÉ cafe\u{301}", vec!["café", "café"]),
        (
            "listIssues and HTTPServer searches",
            vec!["list", "issue", "http", "server", "search"],
        ),
    ] {
        assert_eq!(tokenize(text), expected, "{text}");
    }
    let docs = [
        SearchDocument {
            name: "mcp__app__queue_read",
            text: "Read a thread's queued messages and its latest thread read state",
            namespace_text: None,
        },
        SearchDocument {
            name: "mcp__dev-app__thread_read",
            text: "Locate durable state",
            namespace_text: Some("搜索仓库问题"),
        },
    ];
    assert_eq!(
        rank_documents("thread_read", &docs, 1),
        ["mcp__dev-app__thread_read"]
    );
    assert_eq!(
        rank_documents("问题", &docs, 8),
        ["mcp__dev-app__thread_read"]
    );
    assert!(rank_documents("未知", &docs, 8).is_empty());
}

#[tokio::test]
async fn a_declined_foreground_timer_falls_back_once_and_background_does_not() -> TestResult {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let host: SetTimer = Arc::new(|_, _, _| Ok(None));
    let tim = host_timers(Some(host), {
        let reports = Arc::clone(&reports);
        Arc::new(move |error| {
            reports
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(error.to_string())
        })
    });
    let spawn = || {
        let (send, recv) = tokio::sync::oneshot::channel();
        let handle = tim.foreground(
            Box::new(move || {
                let _ = send.send(7);
            }),
            Duration::ZERO,
        );
        (handle, recv)
    };
    let (first, a) = spawn();
    let _first = first?;
    let (second, b) = spawn();
    let _second = second?;
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), a).await??, 7);
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), b).await??, 7);
    assert_eq!(
        reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    let (send, recv) = tokio::sync::oneshot::channel::<()>();
    assert!(
        tim.background(
            Box::new(move || {
                let _ = send.send(());
            }),
            Duration::ZERO
        )?
        .is_none()
    );
    assert!(recv.await.is_err());
    Ok(())
}

#[tokio::test]
async fn a_panicking_host_timer_is_reported_and_foreground_work_still_runs() -> TestResult {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::clone(&messages);
    // An intentional panic represents an exception from the application callback.
    let host: SetTimer = Arc::new(|_, _, _| std::panic::panic_any("callback failed"));
    let timers = host_timers(
        Some(host),
        Arc::new(move |error| {
            reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(error.to_string());
        }),
    );
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let _timer = timers.foreground(
        Box::new(move || {
            let _ = sender.send(99);
        }),
        Duration::ZERO,
    )?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), receiver).await??,
        99
    );
    let messages = messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        messages.as_slice(),
        ["HarnessOptions.setTimer panicked: callback failed"]
    );
    Ok(())
}

#[tokio::test]
async fn a_task_defers_phase_execution_and_maps_the_selected_checkpoint() -> TestResult {
    let phase: PhaseHandler<Mutex<Vec<Value>>> = Arc::new(|record, runtime, _| {
        Box::pin(async move {
            let TaskState::Running { checkpoint, .. } = &record.state else {
                return Err(std::io::Error::other("task was not reserved").into());
            };
            runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(checkpoint.clone());
            Ok(())
        })
    });
    let task = define_task(TaskDefinition {
        name: "counter".into(),
        version: 3,
        initial: Arc::new(|input| Ok(json!({"phase":"run", "initial":input}))),
        phases: [("run".to_owned(), Arc::clone(&phase))]
            .into_iter()
            .collect(),
        abort: phase,
        migrate: None,
        hooks: (),
    });
    let checkpoint = task.initial(&json!(14))?;
    let mut record = sleepy(Vec::new())?;
    record.state = TaskState::Running {
        checkpoint: checkpoint.clone(),
        wake: None,
    };
    let runtime = Mutex::new(Vec::new());
    let run = task
        .definition
        .phases
        .get("run")
        .ok_or_else(|| std::io::Error::other("missing phase"))?;
    run(
        &record,
        &runtime,
        &pi_rs_chord::context::Context::background(),
    )
    .await?;
    assert_eq!(runtime.into_inner()?, [json!({"phase":"run","initial":14})]);
    Ok(())
}
