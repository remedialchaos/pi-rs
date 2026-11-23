use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use pi_rs_chord::{
    context::Context,
    delta::{Op, Segment},
    json::JsonValue,
    watch::WatchCall,
};
use pi_rs_durable::{
    documents::{
        DocDefinition, DocToken, DocumentSemantics, InitialDocument, LatestFork, define_doc,
        resolve_address,
    },
    errors::SessionError,
    ids::ROOT_CONVERSATION_ID,
    session::{
        CommitObserver, CommitOptions, CommitPersistence, DocumentObserver, Error, Session,
        SessionEnd, SessionOptions, TransactionScope,
    },
    storage::{Storage, memory::MemoryStorage, sqlite::SqliteStorage},
    storage_types::{
        CommitChange, ConversationOwnership::Ownerless, DocumentFork, DocumentPoint, EntryDraft,
        EntryHead, EntryQuery, JsonObject,
    },
    timers::{TimerCallback, TimerHandle},
};
use serde_json::json;

fn count_document(kind: &str, semantics: DocumentSemantics) -> Result<DocToken, Error> {
    Ok(define_doc(DocDefinition {
        kind: kind.into(),
        version: 1,
        semantics,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(JsonObject::from_iter([("count".into(), json!(0))]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, info| info.deltas_since_base == 1),
    })?)
}

fn text(content: &str) -> EntryDraft {
    EntryDraft {
        kind: "pi.user".into(),
        model: Some(vec![json!({"role":"user","content":content})]),
        ..EntryDraft::default()
    }
}

fn set_count(draft: &mut pi_rs_chord::delta::Change, value: u64) -> Result<(), Error> {
    Ok(draft.set(
        &[Segment::Key("count".into())],
        JsonValue::Number(value as f64),
    )?)
}

fn count(value: Option<Arc<JsonObject>>) -> Option<u64> {
    value.as_ref()?.get("count")?.as_u64()
}

#[tokio::test]
async fn multiwrite_preparation_and_backend_refusal_roll_back() -> Result<(), Error> {
    let temp = tempfile::tempdir().map_err(pi_rs_durable::storage::StorageError::from)?;
    let path = temp.path().join("session.sqlite");
    let storage = Arc::new(SqliteStorage::open(&path)?);
    let session = Session::new(storage, SessionOptions::default());
    let ctx = Context::background();
    let doc = count_document("test.atomic", DocumentSemantics::Session)?;
    let address = resolve_address(&doc, None, None)?;
    session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                set_count(tx.doc(&doc, &address, None).await?, 1)
            },
            &ctx,
        )
        .await?;
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let captured = notifications.clone();
    let _subscribe = session.observe_commits(CommitObserver::new(
        move |publication, _| {
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(publication.clone());
            Ok(())
        },
        |_| {},
    ))?;

    let failed = session
        .commit(
            async |tx| {
                tx.append_entry(
                    ROOT_CONVERSATION_ID,
                    text("discarded when callback rejects"),
                )
                .await?;
                set_count(tx.doc(&doc, &address, None).await?, 2)?;
                tx.conversation(ROOT_CONVERSATION_ID).await
            },
            &ctx,
        )
        .await;
    assert!(matches!(
        failed,
        Err(Error::Session(SessionError::ReadAfterWrite(
            "Tx.conversation()"
        )))
    ));
    assert_eq!(
        count(session.snapshot(&doc, &address, &ctx).await?),
        Some(1)
    );
    assert_eq!(
        session.latest_entry_id(ROOT_CONVERSATION_ID, &ctx).await?,
        None
    );

    let failed = session
        .commit(
            async |tx| {
                tx.append_entry(
                    ROOT_CONVERSATION_ID,
                    text("discarded because storage refused batch"),
                )
                .await?;
                tx.create_root_conversation().await?; // Reusing a durable ID refuses the entire batch.
                set_count(tx.doc(&doc, &address, None).await?, 3)
            },
            &ctx,
        )
        .await;
    assert!(matches!(failed, Err(Error::Storage(_))));
    assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
    assert!(
        notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    assert!(matches!(
        session.snapshot(&doc, &address, &ctx).await,
        Err(Error::Session(SessionError::Failed(_)))
    ));

    let reopened = Session::new(
        Arc::new(SqliteStorage::open(path)?),
        SessionOptions::default(),
    );
    assert_eq!(
        reopened.latest_entry_id(ROOT_CONVERSATION_ID, &ctx).await?,
        None
    );
    assert_eq!(
        count(reopened.snapshot(&doc, &address, &ctx).await?),
        Some(1)
    );
    reopened.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn fork_copy_visibility_and_search_use_the_actual_cutoff_owner() -> Result<(), Error> {
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let ctx = Context::background();
    let root = ROOT_CONVERSATION_ID;
    let doc = count_document(
        "test.timeline",
        DocumentSemantics::RewindableConversation {
            fork: DocumentFork::AsOf,
        },
    )?;
    let root_doc = resolve_address(&doc, Some(root), None)?;
    let cutoff = session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                set_count(tx.doc(&doc, &root_doc, None).await?, 10)?;
                tx.append_entry(
                    root,
                    EntryDraft {
                        head: Some(EntryHead::SelfEntry),
                        ..text("Café before the fork")
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    let later = session
        .commit(
            async |tx| {
                set_count(tx.doc(&doc, &root_doc, None).await?, 20)?;
                tx.append_entry(
                    root,
                    EntryDraft {
                        head: Some(EntryHead::SelfEntry),
                        ..text("Café after the fork")
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    let fork = session
        .commit(
            async |tx| tx.fork_conversation(root, cutoff.id, Ownerless).await,
            &ctx,
        )
        .await?;
    let fork_doc = resolve_address(&doc, Some(fork.id), None)?;
    assert_eq!(
        session.latest_entry_id(fork.id, &ctx).await?,
        Some(cutoff.id)
    );
    assert_eq!(
        session
            .latest_head_marker(fork.id, None, &ctx)
            .await?
            .map(|entry| entry.id),
        Some(cutoff.id)
    );
    assert!(
        session
            .visible_entry(fork.id, later.id, &ctx)
            .await?
            .is_none()
    );
    assert_eq!(
        count(session.snapshot(&doc, &fork_doc, &ctx).await?),
        Some(10)
    );
    let child_entry = session
        .commit(
            async |tx| {
                set_count(tx.doc(&doc, &fork_doc, None).await?, 30)?;
                tx.append_entry(
                    fork.id,
                    EntryDraft {
                        head: Some(EntryHead::SelfEntry),
                        ..text("Café on the fork")
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    assert_eq!(
        session.known_latest_entry_id(fork.id),
        Some(Some(child_entry.id))
    );
    assert_eq!(
        session
            .latest_head_marker(fork.id, Some(cutoff.id), &ctx)
            .await?
            .map(|entry| entry.id),
        Some(cutoff.id)
    );
    assert_eq!(
        session
            .latest_head_marker(fork.id, None, &ctx)
            .await?
            .map(|entry| entry.id),
        Some(child_entry.id)
    );
    assert_eq!(
        count(
            session
                .snapshot_as_of(&doc, &fork_doc, cutoff.id, &ctx)
                .await?
        ),
        Some(10)
    );
    assert_eq!(
        count(
            session
                .snapshot_as_of(&doc, &fork_doc, child_entry.id, &ctx)
                .await?
        ),
        Some(30)
    );

    let grandchild = session
        .commit(
            async |tx| tx.fork_conversation(fork.id, cutoff.id, Ownerless).await,
            &ctx,
        )
        .await?;
    let grandchild_doc = resolve_address(&doc, Some(grandchild.id), None)?;
    assert_eq!(
        count(session.snapshot(&doc, &grandchild_doc, &ctx).await?),
        Some(10)
    );
    assert!(
        session
            .visible_entry(grandchild.id, child_entry.id, &ctx)
            .await?
            .is_none()
    );
    let query = EntryQuery {
        conversation_id: fork.id,
        ..EntryQuery::default()
    };
    let hits = session.search_entries(&query, " CAFÉ ", 10, &ctx).await?;
    assert_eq!(
        hits.into_iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![child_entry.id, cutoff.id]
    );
    session.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn read_only_session_never_keeps_an_external_writer_old_tail_or_value() -> Result<(), Error> {
    let storage = Arc::new(MemoryStorage::new());
    let writer = Session::new(storage.clone(), SessionOptions::default());
    let reader = Session::new(
        storage,
        SessionOptions {
            read_only: true,
            ..SessionOptions::default()
        },
    );
    let ctx = Context::background();
    let root = ROOT_CONVERSATION_ID;
    let doc = count_document("test.external", DocumentSemantics::Session)?;
    let address = resolve_address(&doc, None, None)?;
    writer
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                set_count(tx.doc(&doc, &address, None).await?, 1)
            },
            &ctx,
        )
        .await?;
    assert_eq!(count(reader.snapshot(&doc, &address, &ctx).await?), Some(1));
    assert_eq!(reader.latest_entry_id(root, &ctx).await?, None);
    let revised = writer
        .commit(
            async |tx| {
                set_count(tx.doc(&doc, &address, None).await?, 2)?;
                tx.append_entry(root, text("external write")).await
            },
            &ctx,
        )
        .await?;
    assert_eq!(reader.latest_entry_id(root, &ctx).await?, Some(revised.id));
    assert_eq!(count(reader.snapshot(&doc, &address, &ctx).await?), Some(2));
    assert!(matches!(
        reader
            .commit(
                async |tx| tx.append_entry(root, text("not allowed")).await,
                &ctx
            )
            .await,
        Err(Error::Session(SessionError::ReadOnly(_)))
    ));
    assert!(reader.conversation(root, &ctx).await?.is_some());
    reader.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn held_changes_are_visible_but_persist_with_folded_ops_and_refresh_retention()
-> Result<(), Error> {
    let storage = Arc::new(MemoryStorage::new());
    let now = Arc::new(AtomicU64::new(100));
    let (send_timer, mut timer) = tokio::sync::mpsc::unbounded_channel::<TimerCallback>();
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            now: {
                let now = now.clone();
                Arc::new(move || now.load(Ordering::SeqCst))
            },
            document_retention: Duration::from_millis(50),
            set_timer: Some(Arc::new(move |callback, _, options| {
                assert!(options.background);
                let callback = Arc::new(Mutex::new(Some(callback)));
                let fires = callback.clone();
                send_timer
                    .send(Box::new(move || {
                        let callback = fires
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        if let Some(callback) = callback {
                            callback();
                        }
                    }))
                    .map_err(|_| std::io::Error::other("test timer receiver dropped"))?;
                Ok(Some(TimerHandle::new(Box::new(move || {
                    drop(
                        callback
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take(),
                    );
                }))))
            })),
            ..SessionOptions::default()
        },
    );
    let context = Context::background();
    let root = ROOT_CONVERSATION_ID;
    let doc = count_document(
        "test.progress",
        DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
    )?;
    let address = resolve_address(&doc, Some(root), None)?;
    session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                set_count(tx.doc(&doc, &address, None).await?, 1)
            },
            &context,
        )
        .await?;
    let record = storage
        .find_document(&address, DocumentPoint::Current)?
        .ok_or_else(|| Error::invalid("not created"))?;
    let updates = Arc::new(Mutex::new(Vec::new()));
    let recorded = updates.clone();
    let _subscription = session.observe_commits(CommitObserver::new(
        move |publication, _| {
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(publication.clone());
            Ok(())
        },
        |_| {},
    ))?;
    let options = CommitOptions {
        persistence: CommitPersistence::Deferred {
            within: Some(Duration::MAX),
        },
    };
    for next in [2, 3] {
        session
            .commit_with(
                async |tx| set_count(tx.doc(&doc, &address, None).await?, next),
                &context,
                TransactionScope::default(),
                options,
            )
            .await?;
    }
    assert_eq!(session.document_cache_sizes().held, 1);
    assert_eq!(
        count(session.snapshot(&doc, &address, &context).await?),
        Some(3)
    );
    let persisted = storage
        .document(record.id, DocumentPoint::Current)?
        .ok_or_else(|| Error::invalid("not persisted"))?;
    assert_eq!(persisted.value["count"], json!(1));
    assert!(
        updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );

    session
        .commit(
            async |tx| {
                tx.doc(&doc, &address, None).await?;
                tx.append_entry(root, text("durable dependency on progress"))
                    .await
            },
            &context,
        )
        .await?;
    assert_eq!(session.document_cache_sizes().held, 0);
    let persisted = storage
        .document(record.id, DocumentPoint::Current)?
        .ok_or_else(|| Error::invalid("not persisted"))?;
    assert_eq!(persisted.value["count"], json!(3));
    {
        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].changes.len(), 2);
        assert!(
            matches!(updates[0].changes[1], CommitChange::Document(pi_rs_durable::storage_types::DocumentCommitChange::Change { ref ops, .. }) if matches!(&ops[..], [Op::Set(_, JsonValue::Number(value))] if *value == 3.0))
        );
    }
    now.store(200, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.document_cache_sizes().cached != 0 {
            tokio::select! {
                Some(callback) = timer.recv() => callback(),
                () = tokio::task::yield_now() => {}
            }
        }
    })
    .await
    .map_err(|error| Error::invalid(error.to_string()))?;
    assert_eq!(
        count(session.snapshot(&doc, &address, &context).await?),
        Some(3)
    );
    session.close(&context).await?;
    Ok(())
}

#[tokio::test]
async fn document_observer_receives_retired_old_value_then_replacement() -> Result<(), Error> {
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let ctx = Context::background();
    let doc = count_document("test.incarnation", DocumentSemantics::Session)?;
    let address = resolve_address(&doc, None, None)?;
    session
        .commit(
            async |tx| set_count(tx.doc(&doc, &address, None).await?, 1),
            &ctx,
        )
        .await?;
    let changes = Arc::new(Mutex::new(Vec::new()));
    let calls = changes.clone();
    let subscription = session
        .observe_doc(
            &doc,
            &address,
            DocumentObserver::new(
                move |call, _| {
                    calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(call.clone());
                    Ok(())
                },
                |_| {},
            ),
            &ctx,
        )
        .await?
        .ok_or_else(|| Error::invalid("watch did not attach"))?;
    assert_eq!(subscription.value["count"], json!(1));
    session
        .commit(
            async |tx| {
                set_count(tx.doc(&doc, &address, None).await?, 2)?;
                tx.retire_doc(&doc, &address).await?;
                set_count(tx.doc(&doc, &address, None).await?, 3)
            },
            &ctx,
        )
        .await?;
    let calls = changes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        matches!(&calls[..], [WatchCall::Value { retired: true, value: old, ops: first }, WatchCall::Value { retired: false, value: new, ops: second }]
        if old["count"] == json!(1) && first.is_empty() && new["count"] == json!(3) && matches!(&second[..], [Op::Replace(_)]))
    );
    Ok(())
}

#[tokio::test]
async fn task_and_submission_candidates_publish_atomically_and_retire_the_task_document()
-> Result<(), Error> {
    use pi_rs_chord::watch::WatchEnd;
    use pi_rs_durable::{
        session::TaskOptions,
        signals::{SignalState, read_signal, submission_settled},
        storage_types::{
            SubmissionCreate, SubmissionKind, SubmissionNotify, SubmissionStatus, TaskOutcome,
            TaskOwnership, TaskState,
        },
        tasks::{TaskDefinition, TaskRuntime, define_task},
    };
    let storage = Arc::new(MemoryStorage::new());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            now: Arc::new(|| 39),
            ..SessionOptions::default()
        },
    );
    let context = Context::background();
    let root = ROOT_CONVERSATION_ID;
    let task = define_task(TaskDefinition::<dyn TaskRuntime> {
        name: "test.owner".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({"phase":"first"}))),
        phases: indexmap::IndexMap::new(),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: (),
    });
    let doc = count_document("test.task", DocumentSemantics::Task)?;
    let (task_id, submission, entry) = session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                let task_id = tx.create_task(&task, json!(null), options).await?;
                let address = resolve_address(&doc, Some(task_id), None)?;
                set_count(tx.doc(&doc, &address, None).await?, 3)?;
                let submission = tx
                    .create_submission(SubmissionCreate {
                        trace: None,
                        conversation_id: root,
                        request_id: Some("once".into()),
                        notify: Some(SubmissionNotify {
                            task_id,
                            key: "receipt".into(),
                        }),
                        kind: SubmissionKind::Write,
                        status: SubmissionStatus::Queued,
                        entry: None,
                        answer: None,
                        reason: None,
                        detail: None,
                    })
                    .await?;
                let entry = tx.append_entry(root, text("submission placed")).await?;
                tx.place_submission(submission.id, entry.id)?;
                Ok((task_id, submission.id, entry.id))
            },
            &context,
        )
        .await?;
    let record = session
        .task(task_id, &context)
        .await?
        .ok_or_else(|| Error::invalid("task missing"))?;
    let token = submission_settled().map_err(|error| Error::invalid(error.to_string()))?;
    let signal = read_signal(record.memos.as_ref(), token, "receipt")
        .map_err(|error| Error::invalid(error.to_string()))?;
    assert!(
        matches!(signal, Some(SignalState::Value { value }) if value["id"] == json!(submission) && value["entry"] == json!(entry))
    );
    assert_eq!(
        session
            .submission_by_request(root, "once", &context)
            .await?
            .map(|value| value.status),
        Some(SubmissionStatus::Done)
    );
    let address = resolve_address(&doc, Some(task_id), None)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let advanced = events.clone();
    let ended = events.clone();
    let _observer = session
        .observe_doc(
            &doc,
            &address,
            DocumentObserver::new(
                move |call, _| {
                    if matches!(call, WatchCall::Value { retired: true, .. }) {
                        advanced
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("retired");
                    }
                    Ok(())
                },
                move |end| {
                    if end == WatchEnd::Ended {
                        ended
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("ended");
                    }
                },
            ),
            &context,
        )
        .await?
        .ok_or_else(|| Error::invalid("document observer not attached"))?;
    session
        .commit(
            async |tx| {
                let mut record = (*tx
                    .task(task_id)
                    .await?
                    .ok_or_else(|| Error::invalid("task missing"))?)
                .clone();
                record.state = TaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: json!(true),
                    },
                };
                tx.set_task(record)
            },
            &context,
        )
        .await?;
    assert!(
        storage
            .find_document(&address, DocumentPoint::Current)?
            .is_none()
    );
    let record = session
        .task(task_id, &context)
        .await?
        .ok_or_else(|| Error::invalid("terminal task missing"))?;
    assert_eq!(record.ended_at, Some(39));
    assert!(record.memos.is_none());
    assert_eq!(
        &*events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        &["retired", "ended"]
    );
    session.close(&context).await?;
    Ok(())
}

mod grouped {
    use super::*;
    use pi_rs_durable::{
        ids::*,
        storage::{StorageError, StorageResult},
        storage_types::*,
    };
    use std::sync::{atomic::AtomicBool, mpsc};
    use tokio::sync::Semaphore;

    struct PausedStorage {
        backing: SqliteStorage,
        pause: AtomicBool,
        started: Semaphore,
        resume: Mutex<mpsc::Receiver<()>>,
        calls: Mutex<Vec<usize>>,
    }

    macro_rules! delegate {
        ($($name:ident($($argument:ident : $type:ty),*) -> $result:ty;)*) => {$(
            fn $name(&self, $($argument: $type),*) -> StorageResult<$result> {
                self.backing.$name($($argument),*)
            }
        )*};
    }

    impl Storage for PausedStorage {
        fn commit(
            &self,
            batches: &[Vec<StorageWrite>],
            receive: &mut dyn FnMut(usize, Seq),
        ) -> StorageResult<CommitResult> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(batches.len());
            if self.pause.swap(false, Ordering::AcqRel) {
                self.started.add_permits(1);
                self.resume
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| StorageError::Backend(error.to_string()))?;
            }
            self.backing.commit(batches, receive)
        }

        delegate! {
            mint_id() -> u64;
            conversation(id: ConversationId) -> Option<ConversationRecord>;
            scan_conversations(query: &ConversationQuery, limit: usize, cursor: Option<&Cursor>) -> Page<ConversationRecord>;
            entry(id: EntryId) -> Option<StoredEntry>;
            visible_entry(conversation_id: ConversationId, id: EntryId) -> Option<StoredEntry>;
            find_latest_head_marker(conversation_id: ConversationId, at: Option<EntryId>) -> Option<EntryRecord>;
            scan_entries(query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Page<EntryRecord>;
            task(id: TaskId) -> Option<TaskRecord>;
            scan_tasks(query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Page<TaskRecord>;
            submission(id: SubmissionId) -> Option<SubmissionRecord>;
            scan_submissions(query: &SubmissionQuery, limit: usize, cursor: Option<&Cursor>) -> Page<SubmissionRecord>;
            submission_by_request(conversation_id: ConversationId, request_id: &str) -> Option<SubmissionRecord>;
            find_document(address: &DocumentAddress, at: DocumentPoint) -> Option<DocumentRecord>;
            document(id: DocumentId, at: DocumentPoint) -> Option<StoredDocument>;
            scan_documents(query: &DocumentQuery, limit: usize, cursor: Option<&Cursor>) -> Page<DocumentRecord>;
            close() -> ();
        }
    }

    #[tokio::test]
    async fn publishes_only_received_prefix_and_rejects_the_remainder() -> Result<(), Error> {
        let dir = tempfile::tempdir().map_err(StorageError::from)?;
        let path = dir.path().join("group.sqlite");
        let (release, resume) = mpsc::channel();
        let storage = Arc::new(PausedStorage {
            backing: SqliteStorage::open(&path)?,
            pause: AtomicBool::new(false),
            started: Semaphore::new(0),
            resume: Mutex::new(resume),
            calls: Mutex::default(),
        });
        let session = Session::new(storage.clone(), SessionOptions::default());
        let ctx = Context::background();
        let root = ROOT_CONVERSATION_ID;
        session
            .commit(async |tx| tx.create_root_conversation().await, &ctx)
            .await?;
        storage.pause.store(true, Ordering::Release);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let track = seen.clone();
        let _obs = session.observe_commits(CommitObserver::new(
            move |commit, _| {
                track
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(commit.seq);
                Ok(())
            },
            |_| {},
        ))?;
        let staging_first = Semaphore::new(0);
        let staging_second = Semaphore::new(0);
        let staging_third = Semaphore::new(0);
        let in_flight = session.commit(
            async |tx| tx.append_entry(root, text("in flight")).await,
            &ctx,
        );
        let valid = async {
            storage
                .started
                .acquire()
                .await
                .map_err(|_| Error::Closed)?
                .forget();
            session
                .commit(
                    async |tx| {
                        let entry = tx.append_entry(root, text("valid prefix")).await?;
                        staging_first.add_permits(1);
                        Ok(entry)
                    },
                    &ctx,
                )
                .await
        };
        let invalid = async {
            staging_first
                .acquire()
                .await
                .map_err(|_| Error::Closed)?
                .forget();
            session
                .commit(
                    async |tx| {
                        let entry = tx.append_entry(root, text("invalid batch")).await?;
                        tx.create_root_conversation().await?;
                        staging_second.add_permits(1);
                        Ok(entry)
                    },
                    &ctx,
                )
                .await
        };
        let later = async {
            staging_second
                .acquire()
                .await
                .map_err(|_| Error::Closed)?
                .forget();
            session
                .commit(
                    async |tx| {
                        // No write of an unpublished key is read here, so all three jobs can be admitted.
                        let created = tx.create_conversation(Ownerless).await?;
                        tx.append_entry(created.id, text("not durable")).await?;
                        staging_third.add_permits(1);
                        Ok(created)
                    },
                    &ctx,
                )
                .await
        };
        let release = async {
            staging_third
                .acquire()
                .await
                .map_err(|_| Error::Closed)?
                .forget();
            assert!(
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
            );
            release
                .send(())
                .map_err(|error| Error::invalid(error.to_string()))
        };
        let outcomes = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(in_flight, valid, invalid, later, release)
        })
        .await
        .map_err(|error| Error::invalid(error.to_string()))?;
        let (in_flight, valid, invalid, later, release) = outcomes;
        release?;
        let in_flight = in_flight?;
        let valid = valid?;
        assert!(matches!(invalid, Err(Error::Storage(_))));
        assert!(matches!(
            later,
            Err(Error::Session(SessionError::Failed(_)))
        ));
        assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
        assert_eq!(
            &*storage
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &[1, 1, 3]
        );
        {
            let seen = seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.len(), 2);
            assert!(seen[0] < seen[1]);
        }
        let reopened = SqliteStorage::open(path)?;
        let query = EntryQuery {
            conversation_id: root,
            ..EntryQuery::default()
        };
        let visible = reopened
            .scan_entries(&query, 20, None)?
            .items
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(visible, vec![valid.id, in_flight.id]);
        assert_eq!(
            reopened
                .scan_conversations(&ConversationQuery::default(), 20, None)?
                .items
                .len(),
            1
        );
        Ok(())
    }
}
