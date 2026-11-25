use pi_rs_chord::{
    Result,
    sqlite::{Options, SqliteDatabase, apply_migrations_in},
    telemetry_ingest::{PageBudgets, TelemetryBatch, TelemetryRecorder, series_key},
    telemetry_sqlite as sql,
};
use pi_rs_telemetry::{
    AttributeValue, MetricKind, MetricQuery, SpanAttributes, SpanStatus, StoredEvent, StoredLink,
    StoredMetric, StoredSpan, TraceQuery,
};
fn span(id: &str, trace: &str, time: f64) -> StoredSpan {
    StoredSpan {
        id: id.into(),
        trace_id: trace.into(),
        parent_id: None,
        name: "root".into(),
        started_ms: time,
        ended_ms: time + 1.0,
        attributes: SpanAttributes::new(),
        events: vec![StoredEvent {
            name: "event".into(),
            time_ms: 0.123,
            attributes: SpanAttributes::new(),
        }],
        links: vec![StoredLink {
            trace_id: "other".into(),
            span_id: "linked".into(),
            attributes: SpanAttributes::new(),
        }],
        status: SpanStatus::Ok,
    }
}
fn metric() -> StoredMetric {
    StoredMetric {
        interval_ms: 10_000.0,
        name: "calls".into(),
        kind: MetricKind::Counter,
        unit: None,
        attributes: SpanAttributes::new(),
        count: 1,
        sum: 2.0,
        min: 2.0,
        max: 2.0,
    }
}
#[tokio::test]
async fn telemetry_v1_migrates_real_file_and_v2_records_preserve_microseconds_and_typed_filters()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("telemetry.db");
    let db = SqliteDatabase::open(&path, Options::default()).await?;
    assert!(sql::assert_store(&db).await.is_err());
    db.transaction(|db|{apply_migrations_in(db,"telemetry_schema",&sql::migrations()[..1])?;db.execute_batch("INSERT INTO telemetry_spans VALUES('legacy','old',NULL,'old',1000.123,1001.456,'{}','[]','[]','{\"status\":\"ok\"}')")?;Ok(())}).await?;
    assert!(sql::assert_store(&db).await.is_err());
    sql::migrate(&db).await?;
    sql::assert_store(&db).await?;
    let old = sql::get_trace(&db, "old".into())
        .await?
        .ok_or_else(|| pi_rs_chord::Error::Invalid("lost legacy trace".into()))?;
    assert_eq!(old.spans[0].started_ms, 1000.123);
    let mut boolean = span("scope:9", "boolean", 2000.0006);
    boolean
        .attributes
        .insert("a.\"b".into(), AttributeValue::Boolean(true));
    let mut numeric = span("scope:10", "numeric", 2000.0006);
    numeric
        .attributes
        .insert("a.\"b".into(), AttributeValue::Number(1.0));
    let mut child = span("scope:11", "boolean", 2000.0006);
    child.parent_id = Some("scope:9".into());
    child.name = "child".into();
    child.status = SpanStatus::Error { error: None };
    sql::write_batch(
        &db,
        TelemetryBatch {
            spans: vec![boolean.clone(), numeric, child],
            metrics: vec![metric(), metric()],
        },
        0.0,
    )
    .await?;
    let mut duplicate = boolean;
    duplicate.name = "must not replace".into();
    sql::write_batch(
        &db,
        TelemetryBatch {
            spans: vec![duplicate],
            metrics: vec![],
        },
        0.0,
    )
    .await?;
    let mut attrs = SpanAttributes::new();
    attrs.insert("a.\"b".into(), AttributeValue::Boolean(true));
    let found = sql::find_traces(
        &db,
        TraceQuery {
            attributes: attrs,
            errors: true,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(found.traces.len(), 1);
    assert_eq!(found.traces[0].trace_id, "boolean");
    assert_eq!(found.traces[0].spans[0].id, "scope:9");
    assert_eq!(found.traces[0].spans[0].name, "root");
    assert_eq!(found.traces[0].spans[0].started_ms, 2000.001);
    assert_eq!(found.traces[0].spans[0].events[0].time_ms, 0.123);
    assert_eq!(found.traces[0].spans[0].links[0].span_id, "linked");
    let first = sql::find_traces(
        &db,
        TraceQuery {
            limit: Some(1),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(first.traces[0].trace_id, "boolean");
    assert!(first.cursor.is_some());
    let next = sql::find_traces(
        &db,
        TraceQuery {
            limit: Some(1),
            cursor: first.cursor,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(next.traces[0].trace_id, "numeric");
    let metrics = sql::find_metrics(
        &db,
        MetricQuery {
            name: "calls".into(),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(metrics[0].count, 2);
    assert_eq!(metrics[0].sum, 4.0);
    db.close().await?;
    let raw = rusqlite::Connection::open(&path)?;
    assert_eq!(
        raw.query_row(
            "SELECT started_us FROM telemetry_spans WHERE id='scope:9'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2_000_001
    );
    assert_eq!(
        raw.query_row(
            "SELECT strict FROM pragma_table_list WHERE name='telemetry_spans'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}
#[test]
fn ingest_capacity_counts_inflight_spans_and_metric_intervals_until_release() -> Result<()> {
    let mut recorder = TelemetryRecorder::new(1);
    assert_eq!(
        recorder.ingest(TelemetryBatch {
            spans: vec![span("a", "a", 0.0)],
            metrics: vec![metric(), metric()]
        })?,
        1
    );
    let batch = recorder.take(false, 1, 20_000.0);
    assert_eq!(batch.metrics[0].count, 2);
    assert_eq!(
        series_key(&batch.metrics[0])?,
        "[\"calls\",\"counter\",null,\"{}\"]"
    );
    assert_eq!(
        recorder.ingest(TelemetryBatch {
            spans: vec![span("b", "b", 0.0)],
            metrics: vec![metric()]
        })?,
        0
    );
    assert_eq!(recorder.dropped_spans, 1);
    assert_eq!(recorder.dropped_measurements, 1);
    recorder.release();
    assert_eq!(
        recorder.ingest(TelemetryBatch {
            spans: vec![span("c", "c", 0.0)],
            metrics: vec![metric()]
        })?,
        1
    );
    recorder.stop(false);
    assert_eq!(recorder.ingest(TelemetryBatch::default())?, 0);
    assert_eq!(recorder.take(true, 100, 0.0).spans.len(), 1);
    Ok(())
}
#[test]
fn page_ingest_refuses_foreign_ids_stamps_principal_and_enforces_per_minute_budget() {
    let mut budgets = PageBudgets::default();
    let mut browser = span("browser:p:1", "browser:p:1", 1.0);
    browser.attributes.insert(
        "chord.principal".into(),
        AttributeValue::String("forged".into()),
    );
    browser
        .attributes
        .insert("own".into(), AttributeValue::Number(1.0));
    let foreign = span("server:1", "server:1", 1.0);
    let accepted = budgets.filter("p", 0.0, vec![foreign, browser.clone()], || {
        let mut attrs = SpanAttributes::new();
        attrs.insert(
            "chord.principal".into(),
            AttributeValue::String("actual".into()),
        );
        attrs
    });
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        accepted[0].attributes["chord.principal"],
        AttributeValue::String("actual".into())
    );
    assert_eq!(
        accepted[0].attributes["chord.host"],
        AttributeValue::String("browser".into())
    );
    assert_eq!(
        budgets
            .filter("p", 1.0, vec![browser.clone(); 1201], SpanAttributes::new)
            .len(),
        1199
    );
    assert!(
        budgets
            .filter("p", 2.0, vec![browser.clone()], SpanAttributes::new)
            .is_empty()
    );
    assert_eq!(
        budgets
            .filter("p", 60_000.0, vec![browser], SpanAttributes::new)
            .len(),
        1
    );
}
