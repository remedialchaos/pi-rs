use pi_rs_chord::{
    Error, Result,
    sqlite::{Options, SqliteDatabase},
    sync::SyncFuture,
    telemetry::{
        IngestFacet, IngestOptions, IngestRegistry, Recorder, SendKind, SqliteTelemetry,
        TelemetrySender, TelemetrySink,
    },
    telemetry_ingest::TelemetryBatch,
    telemetry_sqlite,
};
use pi_rs_telemetry::{
    AttributeValue, MetricKind, MetricQuery, SpanAttributes, SpanStatus, StoredSpan,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn span(id: &str) -> StoredSpan {
    StoredSpan {
        id: id.into(),
        trace_id: id.into(),
        parent_id: None,
        name: "event".into(),
        started_ms: 1.0,
        ended_ms: 2.0,
        attributes: SpanAttributes::new(),
        events: vec![],
        links: vec![],
        status: SpanStatus::Ok,
    }
}
#[tokio::test]
async fn recording_preserves_parent_carriers_links_explicit_status_and_settlement() -> Result<()> {
    let scopes = Arc::new(AtomicUsize::new(0));
    let calls = scopes.clone();
    let recorder = Recorder::with_clock(
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            "scope".into()
        },
        10,
        || 1000.123,
    );
    assert_eq!(scopes.load(Ordering::SeqCst), 0);
    let root = recorder.context().open(
        "root",
        SpanAttributes::from([("gone".into(), AttributeValue::Undefined)]),
    )?;
    let carrier = root
        .context()
        .inject()
        .ok_or_else(|| Error::Invalid("missing carrier".into()))?;
    let extracted = recorder.extract(Some(&carrier));
    let child = extracted.open("child", SpanAttributes::new())?;
    child.add_link(root.context(), SpanAttributes::new());
    child.add_event("event", SpanAttributes::new());
    child.finish(None)?;
    child.add_event("late", SpanAttributes::new());
    child.finish(None)?;
    root.status(SpanStatus::Ok);
    root.finish(Some(pi_rs_telemetry::ErrorDetails {
        name: "Error".into(),
        message: "ignored".into(),
    }))?;
    let late = root.context().open("late-child", SpanAttributes::new())?;
    assert!(late.context().inject().is_none());
    late.finish(None)?;
    let failed: Result<()> = recorder
        .context()
        .trace("failed", SpanAttributes::new(), |_| async {
            Err(Error::Invalid("bad".into()))
        })
        .await;
    assert!(failed.is_err());
    let batch = recorder.take(true, 100);
    assert_eq!(scopes.load(Ordering::SeqCst), 1);
    assert_eq!(batch.spans.len(), 3);
    assert_eq!(batch.spans[0].parent_id.as_deref(), Some("scope:1"));
    assert_eq!(batch.spans[0].trace_id, "scope:1");
    assert_eq!(batch.spans[0].links[0].span_id, "scope:1");
    assert_eq!(batch.spans[0].events.len(), 1);
    assert_eq!(batch.spans[1].status, SpanStatus::Ok);
    assert!(batch.spans[1].attributes.is_empty());
    assert!(matches!(batch.spans[2].status, SpanStatus::Error { .. }));
    recorder.release();
    recorder.stop(false);
    recorder
        .context()
        .open("stopped", SpanAttributes::new())?
        .finish(None)?;
    assert_eq!(recorder.queued_spans(), 0);
    Ok(())
}
#[derive(Default)]
struct Registry(Vec<(String, String, Value)>);
impl IngestRegistry for Registry {
    fn register(
        &mut self,
        name: &str,
        variant: &str,
        schema: Value,
        _facet: IngestFacet,
    ) -> Result<()> {
        self.0.push((name.into(), variant.into(), schema));
        Ok(())
    }
}
#[test]
fn facet_admission_checks_closed_batches_optional_links_and_stamps_each_principal() -> Result<()> {
    let recorder = Recorder::new(|| "server".into(), 100);
    let target = recorder.clone();
    let facet = IngestFacet::new(IngestOptions {
        target: Arc::new(move |b| target.ingest(b)),
        admit: Arc::new(|p| p["kind"] == "user"),
        worker_host: Arc::new(|p| p["host"].as_str().map(str::to_owned)),
        principal_attributes: Arc::new(|p| {
            SpanAttributes::from([(
                "chord.principal".into(),
                p["id"].as_str().unwrap_or("").into(),
            )])
        }),
    })?;
    let mut registry = Registry::default();
    facet.register(&mut registry)?;
    assert_eq!(registry.0.len(), 2);
    assert_eq!(registry.0[0].0, "chord.telemetry.page");
    assert_eq!(registry.0[1].1, "worker");
    assert!(facet.page(json!({"kind":"other"})).is_err());
    let page = facet.page(json!({"kind":"user","id":"actual"}))?;
    let mut own = serde_json::to_value(span("browser:p:1"))?;
    own["attributes"] = json!({"chord.principal":"forged","own":true});
    own["links"] = json!([{"traceId":"external","spanId":"external"}]);
    assert_eq!(
        page.ingest(
            "p",
            0.0,
            json!({"spans":[own.clone(),serde_json::to_value(span("server:1"))?]})
        )?
        .written,
        1
    );
    assert!(
        page.ingest("p", 0.0, json!({"spans":[own.clone()],"metrics":[]}))
            .is_err()
    );
    let mut invalid = own.clone();
    invalid["name"] = json!("bad\nname");
    assert!(page.ingest("p", 0.0, json!({"spans":[invalid]})).is_err());
    assert!(
        page.ingest("p", 0.0, json!({"spans":vec![own;101]}))
            .is_err()
    );
    assert!(facet.worker(&json!({"kind":"other"})).is_none());
    let worker = facet
        .worker(&json!({"host":"worker:actual"}))
        .ok_or_else(|| Error::Invalid("missing worker".into()))?;
    worker.ingest(json!({"spans":[serde_json::to_value(span("worker:1"))?],"metrics":[]}))?;
    let batch = recorder.take(true, 100);
    assert_eq!(batch.spans.len(), 2);
    assert_eq!(
        batch.spans[0].attributes["chord.principal"],
        AttributeValue::String("actual".into())
    );
    assert_eq!(
        batch.spans[0].attributes["chord.host"],
        AttributeValue::String("browser".into())
    );
    assert_eq!(
        batch.spans[1].attributes["chord.host"],
        AttributeValue::String("worker:actual".into())
    );
    Ok(())
}
#[derive(Default)]
struct Sink {
    bound: AtomicBool,
    fail: AtomicBool,
    hold: AtomicBool,
    calls: Mutex<Vec<TelemetryBatch>>,
    releases: AtomicUsize,
    gate: Arc<Notify>,
}
impl TelemetrySink for Sink {
    fn send(&self, b: &TelemetryBatch) -> Option<SyncFuture<()>> {
        if !self.bound.load(Ordering::Acquire) {
            return None;
        }
        lock(&self.calls).push(b.clone());
        let fail = self.fail.load(Ordering::Acquire);
        let hold = self.hold.load(Ordering::Acquire);
        let gate = self.gate.clone();
        Some(Box::pin(async move {
            if hold {
                gate.notified().await;
            }
            if fail {
                Err(Error::Invalid("refused".into()))
            } else {
                Ok(())
            }
        }))
    }
    fn release(&self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn sender_requeues_unbound_but_drops_refused_batches_and_releases_once() -> Result<()> {
    let recorder = Recorder::new(|| "sender".into(), 101);
    recorder.ingest(TelemetryBatch {
        spans: (0..101).map(|i| span(&format!("s:{i}"))).collect(),
        metrics: vec![],
    })?;
    let sink = Arc::new(Sink::default());
    let sender = TelemetrySender::new(recorder.clone(), sink.clone(), SendKind::Page);
    sender.send(true).await?;
    assert_eq!(recorder.queued_spans(), 101);
    sink.bound.store(true, Ordering::Release);
    sink.fail.store(true, Ordering::Release);
    sender.send(true).await?;
    assert_eq!(lock(&sink.calls)[0].spans.len(), 100);
    assert_eq!(recorder.queued_spans(), 1);
    sender.send(true).await?;
    assert_eq!(recorder.queued_spans(), 0);
    sender.stop();
    sender.stop();
    sender.send(true).await?;
    drop(sender);
    assert_eq!(sink.releases.load(Ordering::SeqCst), 1);
    assert_eq!(lock(&sink.calls).len(), 2);
    Ok(())
}
#[tokio::test]
async fn cancelling_sender_future_releases_reserved_capacity() -> Result<()> {
    let recorder = Recorder::new(|| "sender".into(), 1);
    recorder.ingest(TelemetryBatch {
        spans: vec![span("first")],
        metrics: vec![],
    })?;
    let sink = Arc::new(Sink::default());
    sink.bound.store(true, Ordering::Release);
    sink.hold.store(true, Ordering::Release);
    let sender = TelemetrySender::new(recorder.clone(), sink.clone(), SendKind::Worker);
    {
        let sending = sender.send(true);
        tokio::pin!(sending);
        tokio::select! {result=&mut sending=>{result?;panic!("send did not wait");},_=tokio::task::yield_now()=>{}}
    }
    assert_eq!(lock(&sink.calls).len(), 1);
    assert_eq!(
        recorder.ingest(TelemetryBatch {
            spans: vec![span("second")],
            metrics: vec![]
        })?,
        1
    );
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn sender_timer_retries_unbound_and_stop_cancels_future_ticks() -> Result<()> {
    let recorder = Recorder::new(|| "timer".into(), 10);
    recorder.ingest(TelemetryBatch {
        spans: vec![span("first")],
        metrics: vec![],
    })?;
    let sink = Arc::new(Sink::default());
    let sender = TelemetrySender::new(recorder.clone(), sink.clone(), SendKind::Worker);
    sender.start_timer(Duration::from_secs(1))?;
    assert!(sender.start_timer(Duration::from_secs(1)).is_err());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(recorder.queued_spans(), 1);
    sink.bound.store(true, Ordering::Release);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(lock(&sink.calls).len(), 1);
    sender.stop();
    assert!(sender.start_timer(Duration::from_secs(1)).is_err());
    recorder.ingest(TelemetryBatch {
        spans: vec![span("second")],
        metrics: vec![],
    })?;
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(lock(&sink.calls).len(), 1);
    Ok(())
}
#[tokio::test]
async fn sqlite_close_flushes_partial_metrics_keeps_database_open_and_caches_write_failure()
-> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = SqliteTelemetry::open(db.clone(), 60_000.0, 10).await?;
    store.recorder().measure(
        "pending".into(),
        MetricKind::Counter,
        None,
        3.0,
        SpanAttributes::new(),
    )?;
    store.close().await?;
    store.close().await?;
    let metrics = telemetry_sqlite::find_metrics(
        &db,
        MetricQuery {
            name: "pending".into(),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].sum, 3.0);
    assert_eq!(
        store.ingest(TelemetryBatch {
            spans: vec![span("ignored")],
            metrics: vec![]
        })?,
        0
    );
    db.close().await?;
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = SqliteTelemetry::open(db.clone(), 60_000.0, 10).await?;
    store.ingest(TelemetryBatch {
        spans: vec![span("queued")],
        metrics: vec![],
    })?;
    db.call(|c| {
        c.execute_batch("DROP TABLE telemetry_spans")?;
        Ok(())
    })
    .await?;
    assert!(store.flush().await.is_err());
    assert!(store.flush().await.is_err());
    assert!(store.close().await.is_err());
    assert!(store.recorder().stopped());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_flush_owns_capacity_and_completion_after_its_waiter_is_cancelled() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = SqliteTelemetry::open(db.clone(), 60_000.0, 1).await?;
    let recent_span = |id| StoredSpan {
        started_ms: store.recorder().now(),
        ended_ms: store.recorder().now(),
        ..span(id)
    };
    store.ingest(TelemetryBatch {
        spans: vec![recent_span("first")],
        metrics: vec![],
    })?;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let blocker = db.call(move |_| {
        let _ = entered.send(());
        blocked.recv().map_err(|_| pi_rs_chord::Error::Closed)?;
        Ok(())
    });
    ready.await.map_err(|_| pi_rs_chord::Error::Closed)?;
    let owned = store.clone();
    let waiter = tokio::spawn(async move { owned.flush().await });
    for _ in 0..100 {
        if store.recorder().queued_spans() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.recorder().queued_spans(), 0);
    waiter.abort();
    let _ = waiter.await;
    release.send(()).map_err(|_| pi_rs_chord::Error::Closed)?;
    blocker.await?;
    // Join admitted SQLite work without another flush resetting its reservation.
    db.call(|_| Ok(())).await?;
    for _ in 0..12 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.ingest(TelemetryBatch {
            spans: vec![recent_span("second")],
            metrics: vec![]
        })?,
        1
    );
    store.close().await?;
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT count(*) FROM telemetry_spans", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        2
    );
    db.close().await
}
