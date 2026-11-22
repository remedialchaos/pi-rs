use pi_rs_telemetry::{
    AttributeValue, FallibleTelemetryStore, InstrumentOptions, MetricKind, MetricQuery,
    RecordedTelemetryMeasurement, SpanAttributes, SpanOptions, StoredSpan, TelemetryContext,
    TelemetryCounter, TelemetryError, TelemetryMetrics, TelemetryRecorderSink, TelemetrySpan,
    TraceQuery,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingSink {
    spans: Mutex<Vec<StoredSpan>>,
    measurements: Mutex<Vec<(f64, RecordedTelemetryMeasurement)>>,
}

impl TelemetryRecorderSink for RecordingSink {
    fn record_span(&self, span: StoredSpan) {
        self.spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(span);
    }

    fn record_measurement(&self, interval_ms: f64, measurement: RecordedTelemetryMeasurement) {
        self.measurements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((interval_ms, measurement));
    }
}

#[test]
fn sink_receives_settled_spans_and_metric_measurements_and_fallible_queries_work() {
    futures::executor::block_on(async {
        let sink = Arc::new(RecordingSink::default());
        let ctx = pi_rs_telemetry::Context::with_sink(sink.clone());

        let counter = ctx.metrics().counter(
            "pi.requests",
            InstrumentOptions {
                description: Some("request count".into()),
                unit: Some("1".into()),
            },
        );
        counter.add(
            3.0,
            SpanAttributes::from([("route".into(), AttributeValue::from("/chat"))]),
        );

        let result: Result<u32, std::io::Error> = ctx
            .start_span(SpanOptions::new("bridge.op"), |span| async move {
                span.set_attributes(SpanAttributes::from([(
                    "ok".into(),
                    AttributeValue::Boolean(true),
                )]));
                Ok(7)
            })
            .await;
        assert_eq!(result.expect("span succeeded"), 7);

        let spans = sink
            .spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "bridge.op");
        assert_eq!(
            spans[0].attributes.get("ok"),
            Some(&AttributeValue::Boolean(true))
        );

        let measurements = sink
            .measurements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(measurements.len(), 1);
        assert_eq!(measurements[0].1.name, "pi.requests");
        assert_eq!(measurements[0].1.kind, MetricKind::Counter);
        assert_eq!(measurements[0].1.value, 3.0);

        let page = ctx
            .find_traces_fallible(TraceQuery::default())
            .await
            .expect("fallible find_traces");
        assert_eq!(page.traces.len(), 1);
        let trace = ctx
            .get_trace_fallible(&spans[0].trace_id)
            .await
            .expect("fallible get_trace");
        assert!(trace.is_some());
        let metrics = ctx
            .find_metrics_fallible(MetricQuery {
                name: "pi.requests".into(),
                ..MetricQuery::default()
            })
            .await
            .expect("fallible find_metrics");
        assert_eq!(metrics.len(), 1);

        let invalid = ctx
            .find_traces_fallible(TraceQuery {
                limit: Some(0),
                ..TraceQuery::default()
            })
            .await;
        assert!(matches!(invalid, Err(TelemetryError::InvalidLimit)));
    });
}
