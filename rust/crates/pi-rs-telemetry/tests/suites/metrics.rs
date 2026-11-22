use super::common::*;
use futures::executor::block_on;
use pi_rs_telemetry::*;
use serde_json::json;
use std::future::ready;
fn schemas() -> Vec<TelemetrySchemaDefinition> {
    vec![
        json!({"version":1,"spans":{},"metrics":{"network.bytes":{"kind":"counter","description":"Encoded bytes","unit":"By","attributes":{"direction":{"type":"string","values":["in","out"],"required":true,"description":"Direction","cardinality":"low"}}},"network.active":{"kind":"upDownCounter","description":"Open links","attributes":{}}}}),
        json!({"version":1,"spans":{},"metrics":{"call.duration":{"kind":"histogram","description":"Call duration","unit":"ms","boundaries":[1,10,100],"attributes":{"method":{"type":"string","required":false,"description":"Method"}}}}}),
    ]
}
#[test]
fn schema_metrics_bind_names_metadata_and_exemplars() -> TestResult {
    let adapter = InMemoryTelemetryContext::new();
    let definitions = schemas();
    let metrics = create_typed_metrics(adapter.metrics(), &definitions);
    metrics
        .counter("network.bytes")
        .add(128.0, attributes(json!({"direction":"out"}))?);
    let active = metrics.up_down_counter("network.active");
    active.add(1.0, SpanAttributes::new());
    active.add(-1.0, SpanAttributes::new());
    block_on(adapter.start_span(SpanOptions::new("request"), |span| {
        metrics.histogram("call.duration").record(
            12.0,
            [("method".into(), "read".into())].into_iter().collect(),
            Some(&span),
        );
        metrics
            .histogram("call.duration")
            .record(13.0, SpanAttributes::new(), None);
        metrics
            .histogram("call.duration")
            .record(14.0, SpanAttributes::new(), Some(&span));
        ready(Ok::<_, ErrorDetails>(()))
    }))?;
    let rows = adapter.get_metrics();
    assert_eq!(rows.len(), 6);
    assert_eq!(
        (
            rows[0].kind,
            rows[0].name.as_str(),
            rows[0].description.as_deref(),
            rows[0].unit.as_deref(),
            rows[0].value
        ),
        (
            MetricKind::Counter,
            "network.bytes",
            Some("Encoded bytes"),
            Some("By"),
            128.0
        )
    );
    assert_eq!(rows[0].attributes, attributes(json!({"direction":"out"}))?);
    assert_eq!(rows[3].boundaries, Some(vec![1.0, 10.0, 100.0]));
    assert_eq!(
        rows[3].context,
        Some(adapter.get_spans()[0].context.clone())
    );
    assert!(rows[4].context.is_none());
    assert_eq!(rows[5].context, rows[3].context);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&definitions[0])?)?,
        definitions[0]
    );
    Ok(())
}
#[test]
fn histogram_records_schema_declared_required_attributes() -> TestResult {
    let c = Context::new();
    let schemas = [
        json!({"version":1,"spans":{},"metrics":{"required":{"kind":"histogram","description":"Required tag","attributes":{"cache":{"type":"boolean","required":true,"description":"Cache hit"}}}}}),
    ];
    struct RequiredHistogram;
    struct RequiredAttributes {
        cache: bool,
    }
    impl From<RequiredAttributes> for SpanAttributes {
        fn from(value: RequiredAttributes) -> Self {
            [("cache".into(), value.cache.into())].into_iter().collect()
        }
    }
    impl TelemetryMetricSchema for RequiredHistogram {
        const NAME: &'static str = "required";
        type Attributes = RequiredAttributes;
    }
    impl HistogramSchema for RequiredHistogram {}
    create_typed_metrics(c.metrics(), &schemas)
        .histogram_schema::<RequiredHistogram>()
        .record(1.0, RequiredAttributes { cache: true }, None);
    assert_eq!(
        c.get_metrics()[0].attributes,
        attributes(json!({"cache":true}))?
    );
    Ok(())
}
#[test]
fn snapshots_detach_measurements_exemplars_and_boundaries() -> TestResult {
    let c = Context::new();
    let instrument = c.histogram(
        "duration",
        HistogramOptions {
            boundaries: Some(vec![1.0, 2.0]),
            ..HistogramOptions::default()
        },
    );
    let target = saved(&c, "request")?;
    instrument.record(
        1.0,
        attributes(json!({"tags":["original"]}))?,
        Some(&target),
    );
    let before = c.get_metrics();
    let mut copy = c.get_metrics();
    copy[0]
        .boundaries
        .as_mut()
        .ok_or("missing boundaries")?
        .push(3.0);
    copy[0].attributes.insert("tags".into(), "mutated".into());
    copy[0]
        .context
        .as_mut()
        .ok_or("missing context")?
        .insert("pi-memory-span".into(), "999".into());
    assert_eq!(c.get_metrics(), before);
    Ok(())
}
#[test]
fn noop_instruments_share_identity_and_ignore_payloads() -> TestResult {
    let c = NOOP_TELEMETRY_METRICS.counter("bytes", InstrumentOptions::default());
    let active = NOOP_TELEMETRY_METRICS.up_down_counter("active", InstrumentOptions::default());
    let hist = NOOP_TELEMETRY_METRICS.histogram("duration", HistogramOptions::default());
    assert!(
        c.same_instance(&NOOP_TELEMETRY_METRICS.counter("other", InstrumentOptions::default()))
    );
    assert!(active.same_instance(
        &NOOP_TELEMETRY_METRICS.up_down_counter("other", InstrumentOptions::default())
    ));
    assert!(
        hist.same_instance(&NOOP_TELEMETRY_METRICS.histogram("other", HistogramOptions::default()))
    );
    c.add(1.0, attributes(json!({"direction":"out"}))?);
    active.add(-1.0, SpanAttributes::new());
    hist.record(1.0, SpanAttributes::new(), Some(&NOOP_TELEMETRY_CONTEXT));
    assert!(NOOP_TELEMETRY_CONTEXT.get_metrics().is_empty());
    Ok(())
}
