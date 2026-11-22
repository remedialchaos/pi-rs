use pi_rs_chord::{
    Error, Result, context::Context, contracts::schema as s, delta::Segment, documents::*,
    json::JsonValue, sqlite::SqliteDatabase, state::Batch,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn j(text: &str) -> JsonValue {
    // INFALLIBLE: all callers pass authored literal JSON fixtures.
    serde_json::from_str(text).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn def(name: &str, keyed: bool) -> Result<Arc<Definition>> {
    Ok(Arc::new(
        Definition::new(name, s::obj([("n", s::required(s::int()))]), 1, || {
            Ok(j(r#"{"n":0}"#))
        })?
        .keyed(if keyed {
            KeyedBy::String
        } else {
            KeyedBy::Singleton
        })
        .persisted(true, |_, _, _| false),
    ))
}
fn docs(db: &SqliteDatabase) -> Documents {
    Documents::new(
        "owner",
        Batch::new(|_| {}),
        Some(DocumentStore::new(db.clone(), "owner")),
        |_| {},
    )
}
async fn database() -> Result<SqliteDatabase> {
    let db = SqliteDatabase::open(":memory:", Default::default()).await?;
    pi_rs_chord::store::apply_store_schema(&db).await?;
    Ok(db)
}
async fn write(docs: &Documents, handle: &Handle, key: Option<DocumentKey>, n: f64) -> Result<()> {
    let h = handle.clone();
    docs.transaction(
        handle.token(),
        &Context::background(),
        move |tx| async move {
            tx.draft(&h, key)
                .await?
                .set(&[Segment::Key("n".into())], JsonValue::Number(n))
        },
    )
    .await
}
#[tokio::test]
async fn dropped_queued_commit_cannot_mutate_sql_or_keep_the_line() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("list", false)?)?;
    a.activate(&Context::background()).await?;
    let (release, gate) = std::sync::mpsc::channel();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let blocked = db.call(move |_| {
        let _ = entered.send(());
        gate.recv().map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(())
    });
    ready.await.map_err(|_| Error::Closed)?;
    let (finished, wait) = tokio::sync::oneshot::channel();
    let d = owner.clone();
    let handle = h.clone();
    let token = handle.token().clone();
    let task = tokio::spawn(async move {
        d.transaction(&token, &Context::background(), move |tx| {
            let h = handle.clone();
            async move {
                tx.draft(&h, None)
                    .await?
                    .set(&[Segment::Key("n".into())], j("9"))?;
                let _ = finished.send(());
                Ok(())
            }
        })
        .await
    });
    wait.await.map_err(|_| Error::Closed)?;
    task.abort();
    assert!(task.await.is_err());
    release
        .send(())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    blocked.await?;
    db.call(|_| Ok(())).await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), owner.settled())
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
    assert_eq!(h.value()?.to_json(), j(r#"{"n":0}"#));
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT COUNT(*) FROM chord_documents", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        0
    );
    db.close().await?;
    Ok(())
}
struct PausedAdoption {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
    commits: Arc<AtomicUsize>,
}
impl Staged for PausedAdoption {
    fn adopt(&self) -> Result<()> {
        if let Some(entered) = self.entered.lock().map_err(|_| Error::Closed)?.take() {
            let _ = entered.send(());
        }
        self.gate
            .lock()
            .map_err(|_| Error::Closed)?
            .recv()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(())
    }
    fn committed(&self) {
        self.commits.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn admitted_store_commit_finishes_after_caller_drop_and_fence() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("list", false)?)?;
    a.activate(&Context::background()).await?;
    let (release, gate) = std::sync::mpsc::channel();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let commits = Arc::new(AtomicUsize::new(0));
    let staged = Arc::new(PausedAdoption {
        entered: Mutex::new(Some(entered)),
        gate: Mutex::new(gate),
        commits: commits.clone(),
    });
    let d = owner.clone();
    let handle = h.clone();
    let token = handle.token().clone();
    let task = tokio::spawn(async move {
        d.transaction(&token, &Context::background(), move |tx| async move {
            tx.draft(&handle, None)
                .await?
                .set(&[Segment::Key("n".into())], j("7"))?;
            tx.stage(staged)?;
            Ok(())
        })
        .await
    });
    ready.await.map_err(|_| Error::Closed)?;
    task.abort();
    let _ = task.await;
    owner.fence(h.token());
    release
        .send(())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    tokio::time::timeout(std::time::Duration::from_secs(2), owner.settled())
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(h.value()?.to_json(), j(r#"{"n":7}"#));
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT COUNT(*) FROM chord_documents", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        1
    );
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn failed_sql_write_rolls_back_all_documents_metadata_and_publication() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let first = a.declare(def("first", false)?)?;
    let second = a.declare(def("second", false)?)?;
    a.activate(&Context::background()).await?;
    db.call(|c|{c.execute_batch("CREATE TRIGGER reject_nine BEFORE INSERT ON chord_revisions WHEN NEW.content LIKE '%9%' BEGIN SELECT RAISE(ABORT,'rejected'); END")?;Ok(())}).await?;
    let x = first.clone();
    let y = second.clone();
    let result = owner
        .transaction(
            first.token(),
            &Context::background(),
            move |tx| async move {
                tx.draft(&x, None)
                    .await?
                    .set(&[Segment::Key("n".into())], j("1"))?;
                tx.draft(&y, None)
                    .await?
                    .set(&[Segment::Key("n".into())], j("9"))?;
                Ok(())
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(first.value()?.to_json(), j(r#"{"n":0}"#));
    assert_eq!(second.value()?.to_json(), j(r#"{"n":0}"#));
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT COUNT(*) FROM chord_documents", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        0
    );
    assert_eq!(
        db.call(
            |c| Ok(c.query_row("SELECT last_seq FROM chord_meta", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        0
    );
    owner.settled().await?;
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn checkpoint_prunes_old_revisions_and_outbox_retains_exact_changes() -> Result<()> {
    let db = database().await?;
    db.call(|c| {
        c.execute("UPDATE chord_meta SET outbox=1", [])?;
        Ok(())
    })
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let definition = Arc::new(
        Definition::new("list", s::obj([("n", s::required(s::int()))]), 1, || {
            Ok(j(r#"{"n":0}"#))
        })?
        .persisted(true, move |_, _, deltas| {
            counted.fetch_add(1, Ordering::SeqCst);
            deltas >= 1
        }),
    );
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(definition)?;
    a.activate(&Context::background()).await?;
    for n in [1.0, 2.0, 3.0] {
        write(&owner, &h, None, n).await?;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let rows = db
        .call(|c| {
            let mut s = c.prepare("SELECT kind,content FROM chord_revisions")?;
            Ok(
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            )
        })
        .await?;
    assert_eq!(rows, vec![("base".into(), r#"{"n":3}"#.into())]);
    let writes = db
        .call(|c| {
            let mut s = c.prepare("SELECT writes FROM chord_outbox ORDER BY seq")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await?;
    let changes = writes
        .iter()
        .map(|text| serde_json::from_str::<JsonValue>(text))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(changes.len(), 3);
    let JsonValue::Array(w) = &changes[1] else {
        return Err(Error::Invalid("writes fixture".into()));
    };
    assert_eq!(w[0].get("kind"), Some(&j(r#""document.change""#)));
    assert_eq!(w[0].get("revision"), Some(&j("1")));
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn cold_retirement_uses_metadata_without_parsing_invalid_document_content() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    write(&owner, &h, Some("x".into()), 1.0).await?;
    owner.end();
    db.call(|c| {
        c.execute("UPDATE chord_revisions SET content='not json'", [])?;
        Ok(())
    })
    .await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    let handle = h.clone();
    owner
        .transaction(h.token(), &Context::background(), move |tx| async move {
            tx.retire(&handle, Some("x".into())).await
        })
        .await?;
    assert!(h.keys(&Context::background()).await?.is_empty());
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn pinned_persisted_keys_survive_retention_and_idle_keys_reload() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    let pin = a.at(&h, "pinned")?;
    a.activate(&Context::background()).await?;
    write(&owner, &h, Some("pinned".into()), 1.0).await?;
    write(&owner, &h, Some("idle".into()), 2.0).await?;
    assert_eq!(owner.resident_count(), 2);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(601)).await;
    owner.evict().await?;
    assert_eq!(owner.resident_count(), 1);
    assert_eq!(pin.value()?.to_json(), j(r#"{"n":1}"#));
    tokio::time::resume();
    assert_eq!(
        h.get("idle", &Context::background())
            .await?
            .map(|v| v.to_json()),
        Some(j(r#"{"n":2}"#))
    );
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn stored_key_pages_include_empty_key_and_code_point_order_without_caching_absence()
-> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    let handle = h.clone();
    owner
        .transaction(h.token(), &Context::background(), move |tx| async move {
            for i in 0..105 {
                let key = if i == 0 {
                    String::new()
                } else {
                    format!("k{i:03}")
                };
                tx.draft(&handle, Some(key.into()))
                    .await?
                    .set(&[Segment::Key("n".into())], j("1"))?;
            }
            Ok(())
        })
        .await?;
    let keys = h.keys(&Context::background()).await?;
    assert_eq!(keys.len(), 105);
    assert_eq!(keys[0], "");
    assert_eq!(keys[104], "k104");
    assert!(h.get("absent", &Context::background()).await?.is_none());
    write(&owner, &h, Some("absent".into()), 2.0).await?;
    assert!(h.get("absent", &Context::background()).await?.is_some());
    assert!(
        h.get(
            pi_rs_chord::json::JsonString(vec![0]),
            &Context::background()
        )
        .await
        .is_err()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn queued_shared_load_refuses_only_tokens_sealed_before_admission() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    let second = owner.attempt();
    let other = second.declare(def("items", true)?)?;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let d = owner.clone();
    let token = owner.token().clone();
    let blocker = tokio::spawn(async move {
        d.transaction(&token, &Context::background(), move |_| async move {
            let _ = entered.send(());
            let _ = gate.await;
            Ok(())
        })
        .await
    });
    ready.await.map_err(|_| Error::Closed)?;
    let context = Context::background();
    let mut first = Box::pin(h.get("missing", &context));
    let mut second = Box::pin(other.get("missing", &context));
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    owner.fence(h.token());
    let _ = release.send(());
    blocker.await.map_err(|_| Error::Closed)??;
    assert!(first.await.is_err());
    assert!(second.await?.is_none());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn unreadable_persisted_version_fails_the_live_owner_without_rewriting_data() -> Result<()> {
    let db = database().await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    write(&owner, &h, Some("x".into()), 1.0).await?;
    owner.end();
    db.call(|c| {
        c.execute("UPDATE chord_documents SET fingerprint='different'", [])?;
        Ok(())
    })
    .await?;
    let owner = docs(&db);
    let a = owner.attempt();
    let h = a.declare(def("items", true)?)?;
    a.activate(&Context::background()).await?;
    assert!(h.get("x", &Context::background()).await.is_err());
    let error = owner
        .transaction(owner.token(), &Context::background(), |_| async { Ok(()) })
        .await;
    assert!(matches!(error,Err(Error::Chord(e)) if e.code=="broken"));
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT fingerprint FROM chord_documents", [], |r| r
                .get::<_, String>(0))?
        ))
        .await?,
        "different"
    );
    db.close().await?;
    Ok(())
}
