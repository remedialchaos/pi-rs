use pi_rs_chord::{
    Result,
    context::Context,
    contracts::schema as s,
    delta::{Op, Segment},
    documents::{Definition, DocumentStore, Documents, KeyedBy, Line, SealToken},
    json::JsonValue,
    state::Batch,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn value(text: &str) -> JsonValue {
    // INFALLIBLE: callers supply authored literal or formatted JSON fixtures.
    serde_json::from_str(text).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn definition(name: &str, keyed: KeyedBy, persist: bool) -> Result<Arc<Definition>> {
    Ok(Arc::new(
        Definition::new(name, s::obj([("n", s::required(s::int()))]), 1, || {
            Ok(value("{\"n\":0}"))
        })?
        .keyed(keyed)
        .persisted(persist, |_, _, _| false),
    ))
}
fn owner(store: Option<DocumentStore>) -> Documents {
    Documents::new("owner", Batch::new(|_| {}), store, |_| {})
}
#[tokio::test]
async fn line_reserves_fifo_before_poll_and_release_is_idempotent() -> Result<()> {
    let line = Line::default();
    let first = line.acquire();
    let second = line.acquire();
    let mut third = Box::pin(line.acquire());
    let a = first.await?;
    std::future::poll_fn(|cx| {
        assert!(third.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    a.release();
    a.release();
    std::future::poll_fn(|cx| {
        assert!(third.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let b = second.await?;
    b.release();
    let c = third.await?;
    let abandoned = line.acquire();
    let next = line.acquire();
    drop(abandoned);
    c.release();
    next.await?.release();
    Ok(())
}
#[tokio::test]
async fn singleton_activation_atomic_retirement_and_settled_drafts() -> Result<()> {
    let docs = owner(None);
    let ctx = Context::background();
    let attempt = docs.attempt();
    let handle = attempt.declare(definition("list", KeyedBy::Singleton, false)?)?;
    assert!(handle.value().is_err());
    attempt.activate(&ctx).await?;
    let initial = handle.value()?;
    let frames = Arc::new(Mutex::new(Vec::new()));
    let output = frames.clone();
    let source = handle.source()?;
    let _subscription = source.subscribe(
        move |v, _| {
            output
                .lock()
                .map_err(|_| pi_rs_chord::Error::Closed)?
                .push(v);
            Ok(())
        },
        |_| {},
    );
    let h = handle.clone();
    let retained = Arc::new(Mutex::new(None));
    let held = retained.clone();
    docs.transaction(handle.token(), &ctx, move |tx| async move {
        let draft = tx.draft(&h, None).await?;
        draft.set(&[Segment::Key("n".into())], JsonValue::Number(1.0))?;
        *held.lock().map_err(|_| pi_rs_chord::Error::Closed)? = Some(draft);
        Ok(())
    })
    .await?;
    assert_eq!(handle.value()?.to_json(), value("{\"n\":1}"));
    assert!(
        retained
            .lock()
            .map_err(|_| pi_rs_chord::Error::Closed)?
            .as_ref()
            .is_some_and(|d| d.value().is_err())
    );
    let h = handle.clone();
    docs.transaction(handle.token(), &ctx, move |tx| async move {
        tx.retire(&h, None).await?;
        let draft = tx.draft(&h, None).await?;
        draft.set(&[Segment::Key("n".into())], JsonValue::Number(2.0))?;
        Ok(())
    })
    .await?;
    assert!(
        matches!(frames.lock().map_err(|_|pi_rs_chord::Error::Closed)?.last(),Some(pi_rs_chord::watch::WatchCall::Value{retired:true,ops,..})if matches!(ops.as_slice(),[Op::Replace(_)]))
    );
    let h = handle.clone();
    docs.transaction(handle.token(), &ctx, move |tx| async move {
        tx.retire(&h, None).await
    })
    .await?;
    assert!(handle.value()?.ptr_eq(&initial));
    docs.end();
    assert!(handle.value().is_err());
    Ok(())
}
#[tokio::test]
async fn keyed_absence_noop_creation_key_order_and_migration() -> Result<()> {
    let docs = owner(None);
    let ctx = Context::background();
    let a = docs.attempt();
    let h = a.declare(definition("drafts", KeyedBy::String, false)?)?;
    a.activate(&ctx).await?;
    assert!(h.get("a", &ctx).await?.is_none());
    for (key, n) in [("b", 2.0), ("", 1.0), ("a", 0.0)] {
        let handle = h.clone();
        docs.transaction(h.token(), &ctx, move |tx| async move {
            tx.draft(&handle, Some(key.into()))
                .await?
                .set(&[Segment::Key("n".into())], JsonValue::Number(n))
        })
        .await?;
    }
    assert_eq!(h.keys(&ctx).await?, vec!["", "b"]);
    let a = docs.attempt();
    let migrated = Arc::new(
        Definition::new("drafts", s::obj([("n", s::required(s::int()))]), 2, || {
            Ok(value("{\"n\":9}"))
        })?
        .keyed(KeyedBy::String)
        .migrating(|_, from| Ok(value(&format!("{{\"n\":{from}}}")))),
    );
    let h2 = a.declare(migrated)?;
    a.activate(&ctx).await?;
    assert!(h.get("b", &ctx).await.is_err());
    assert_eq!(
        h2.get("b", &ctx).await?.map(|v| v.to_json()),
        Some(value("{\"n\":1}"))
    );
    Ok(())
}
#[tokio::test]
async fn definition_checks_initial_once_migration_always_and_schema_versions() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let def = Definition::new(
        "count",
        s::obj([("n", s::required(s::int()))]),
        1,
        move || {
            let n = count.fetch_add(1, Ordering::SeqCst);
            Ok(if n == 0 {
                value("{\"n\":0}")
            } else {
                value("{\"n\":\"unchecked\"}")
            })
        },
    )?;
    assert!(def.initial().is_ok());
    assert!(def.initial().is_ok());
    let bad = Definition::new("bad", s::int(), 1, || Ok(JsonValue::Null))?;
    assert!(bad.initial().is_err());
    assert!(bad.initial().is_err());
    let migration = Definition::new("migrate", s::int(), 2, || Ok(JsonValue::Number(1.0)))?
        .migrating(|_, _| Ok(JsonValue::String("invalid".into())));
    assert!(migration.migrate(JsonValue::Null, 1).is_err());
    assert!(migration.migrate(JsonValue::Null, 1).is_err());
    let docs = owner(None);
    let a = docs.attempt();
    a.declare(Arc::new(def))?;
    a.activate(&Context::background()).await?;
    assert!(
        docs.attempt()
            .declare(Arc::new(Definition::new(
                "count",
                s::obj([("other", s::required(s::int()))]),
                1,
                || Ok(value("{\"other\":1}"))
            )?))
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn fence_cuts_unadmitted_transaction_and_releases_line() -> Result<()> {
    let docs = owner(None);
    let ctx = Context::background();
    let a = docs.attempt();
    let h = a.declare(definition("list", KeyedBy::Singleton, false)?)?;
    a.activate(&ctx).await?;
    let (started, ready) = tokio::sync::oneshot::channel();
    let (finish, wait) = tokio::sync::oneshot::channel();
    let d = docs.clone();
    let handle = h.clone();
    let token = handle.token().clone();
    let task = tokio::spawn(async move {
        d.transaction(&token, &Context::background(), move |tx| async move {
            let draft = tx.draft(&handle, None).await?;
            draft.set(&[Segment::Key("n".into())], JsonValue::Number(5.0))?;
            let _ = started.send(());
            let _ = wait.await;
            Ok(())
        })
        .await
    });
    ready.await.map_err(|_| pi_rs_chord::Error::Closed)?;
    docs.fence(h.token());
    docs.settled().await?;
    assert_eq!(h.value()?.to_json(), value("{\"n\":0}"));
    let _ = finish.send(());
    assert!(task.await.map_err(|_| pi_rs_chord::Error::Closed)?.is_err());
    assert!(!SealToken::default().sealed());
    Ok(())
}
#[tokio::test]
async fn persisted_singleton_deltas_checkpoint_and_restart_are_v3_compatible() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let db = pi_rs_chord::sqlite::SqliteDatabase::open(file.path(), Default::default()).await?;
    pi_rs_chord::store::apply_store_schema(&db).await?;
    let store = DocumentStore::new(db.clone(), "stable.owner");
    let ctx = Context::background();
    let docs = owner(Some(store.clone()));
    let a = docs.attempt();
    let h = a.declare(definition("list", KeyedBy::Singleton, true)?)?;
    a.activate(&ctx).await?;
    for n in [1.0, 2.0] {
        let handle = h.clone();
        docs.transaction(h.token(), &ctx, move |tx| async move {
            tx.draft(&handle, None)
                .await?
                .set(&[Segment::Key("n".into())], JsonValue::Number(n))
        })
        .await?;
    }
    let rows = db
        .call(|c| {
            let mut s =
                c.prepare("SELECT kind,version,content FROM chord_revisions ORDER BY seq")?;
            Ok(s.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "base");
    assert_eq!(rows[1].0, "delta");
    docs.end();
    let docs = owner(Some(store));
    let a = docs.attempt();
    let h = a.declare(definition("list", KeyedBy::Singleton, true)?)?;
    a.activate(&ctx).await?;
    assert_eq!(h.value()?.to_json(), value("{\"n\":2}"));
    db.close().await?;
    Ok(())
}
