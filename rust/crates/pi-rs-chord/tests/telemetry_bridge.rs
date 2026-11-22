use pi_rs_chord::{
    sqlite::{Options, SqliteDatabase},
    telemetry::SqliteTelemetry,
};
use pi_rs_telemetry::{
    AttributeValue, FallibleTelemetryStore, InstrumentOptions, MetricQuery, SpanAttributes,
    SpanOptions, TelemetryContext, TelemetryCounter, TelemetryMetrics, TelemetrySpan, TraceQuery,
};

#[tokio::test]
async fn shared_telemetry_context_flushes_to_sqlite_and_propagates_storage_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = SqliteDatabase::open(dir.path().join("telemetry.db"), Options::default())
        .await
        .expect("open sqlite");
    let store = SqliteTelemetry::open(db.clone(), 86_400_000.0, 128)
        .await
        .expect("open sqlite telemetry");

    let ctx = store.telemetry_context();
    ctx.metrics()
        .counter("chord.bridge.calls", InstrumentOptions::default())
        .add(
            2.0,
            SpanAttributes::from([("kind".into(), AttributeValue::from("sync"))]),
        );

    let out: Result<usize, std::io::Error> = ctx
        .start_span(SpanOptions::new("chord.bridge.span"), |span| async move {
            span.set_attributes(SpanAttributes::from([(
                "ok".into(),
                AttributeValue::Boolean(true),
            )]));
            Ok(42)
        })
        .await;
    assert_eq!(out.expect("span ok"), 42);

    store
        .close()
        .await
        .expect("flush partial metrics and spans");

    let page = store
        .find_traces_fallible(TraceQuery::default())
        .await
        .expect("query traces through FallibleTelemetryStore");
    assert_eq!(page.traces.len(), 1);
    assert_eq!(page.traces[0].spans[0].name, "chord.bridge.span");

    let trace = store
        .get_trace_fallible(&page.traces[0].trace_id)
        .await
        .expect("get_trace through FallibleTelemetryStore")
        .expect("stored trace present");
    assert_eq!(trace.spans.len(), 1);

    let metrics = store
        .find_metrics_fallible(MetricQuery {
            name: "chord.bridge.calls".into(),
            ..MetricQuery::default()
        })
        .await
        .expect("find_metrics through FallibleTelemetryStore");
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].sum, 2.0);

    // Closing the underlying SQLite connection must surface an explicit Err
    // through FallibleTelemetryStore rather than swallowing it as None/empty.
    db.close().await.expect("close sqlite");
    assert!(
        store
            .get_trace_fallible(&page.traces[0].trace_id)
            .await
            .is_err()
    );
    assert!(
        store
            .find_metrics_fallible(MetricQuery {
                name: "chord.bridge.calls".into(),
                ..MetricQuery::default()
            })
            .await
            .is_err()
    );
}
