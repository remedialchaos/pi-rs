use super::common::*;
use futures::executor::block_on;
use pi_rs_telemetry::*;
use serde_json::json;
use std::{future::ready, sync::Arc};
#[test]
fn only_settled_traces_are_visible_and_snapshots_detach() -> TestResult {
    let (c, _) = timed(20000.0);
    let active = c.start_span(
        SpanOptions {
            name: "root".into(),
            attributes: attributes(json!({"tags":["original"]}))?,
            ..SpanOptions::default()
        },
        |root| {
            root.add_event(
                "event",
                [(
                    "tags".into(),
                    AttributeValue::Strings(vec!["original".into()]),
                )]
                .into_iter()
                .collect(),
            );
            async move {
                root.start_span(SpanOptions::new("child"), |_| {
                    ready(Ok::<_, ErrorDetails>(()))
                })
                .await
            }
        },
    );
    assert!(
        block_on(c.find_traces(TraceQuery::default()))?
            .traces
            .is_empty()
    );
    assert!(block_on(c.get_trace("missing")).is_none());
    block_on(active)?;
    let mut trace = block_on(c.find_traces(TraceQuery::default()))?
        .traces
        .remove(0);
    let root_id = trace.spans[0].id.clone();
    let trace_id = trace.trace_id.clone();
    assert_eq!(root_id, trace_id);
    assert_eq!(trace.spans[1].parent_id, Some(root_id.clone()));
    assert_eq!(trace.spans[0].started_ms, 20000.0);
    assert!(trace.spans[0].ended_ms >= trace.spans[0].started_ms);
    assert!(trace.spans[0].events[0].time_ms >= 0.0);
    let (scope, id) = root_id
        .rsplit_once(':')
        .ok_or("missing global ID separator")?;
    assert!(uuid::Uuid::parse_str(scope).is_ok());
    assert_eq!(id, "1");
    trace.spans[0].attributes.clear();
    trace.spans[0].events[0].attributes.clear();
    let fresh = block_on(c.get_trace(&trace_id)).ok_or("missing trace")?;
    assert_eq!(
        fresh.spans[0].attributes,
        attributes(json!({"tags":["original"]}))?
    );
    assert_eq!(
        fresh.spans[0].events[0].attributes,
        fresh.spans[0].attributes
    );
    Ok(())
}
#[test]
fn filters_first_span_name_time_errors_and_attributes_on_one_span() -> TestResult {
    let (c, clock) = timed(10000.0);
    let clock = clock.as_ref();
    block_on(c.start_span(SpanOptions::new("old"), |root| async move {
        clock.set(30000.0, 100.0);
        root.start_span(
            SpanOptions {
                name: "recent-child".into(),
                attributes: attributes(json!({"left":1}))?,
                ..SpanOptions::default()
            },
            |_| ready(Ok::<_, Box<dyn std::error::Error>>(())),
        )
        .await?;
        root.start_span(
            SpanOptions {
                name: "split".into(),
                attributes: attributes(json!({"right":2}))?,
                ..SpanOptions::default()
            },
            |_| ready(Ok::<_, Box<dyn std::error::Error>>(())),
        )
        .await
    }))?;
    clock.set(20000.0, 100.0);
    block_on(c.start_span(SpanOptions::new("wanted"), |root| async move {
        clock.set(20001.0, 100.0);
        root.start_span(
            SpanOptions {
                name: "failure".into(),
                attributes: attributes(
                    json!({"left":1,"right":2,"valid":true,"odd.\"key":"value"}),
                )?,
                ..SpanOptions::default()
            },
            |span| {
                span.set_status(SpanStatus::Error { error: None });
                ready(Ok::<_, Box<dyn std::error::Error>>(()))
            },
        )
        .await
    }))?;
    clock.set(40000.0, 100.0);
    block_on(c.start_span(
        SpanOptions {
            name: "new".into(),
            attributes: attributes(json!({"valid":1}))?,
            ..SpanOptions::default()
        },
        |_| ready(Ok::<_, ErrorDetails>(())),
    ))?;
    let names = |query| -> Result<Vec<String>, TelemetryError> {
        Ok(block_on(c.find_traces(query))?
            .traces
            .into_iter()
            .map(|t| t.spans[0].name.clone())
            .collect())
    };
    assert_eq!(
        names(TraceQuery {
            name: Some("wanted".into()),
            attributes: attributes(json!({"left":1,"right":2}))?,
            errors: true,
            start_min: Some(20000.0),
            start_max: Some(20000.0),
            ..TraceQuery::default()
        })?,
        vec!["wanted"]
    );
    assert!(
        names(TraceQuery {
            name: Some("failure".into()),
            ..TraceQuery::default()
        })?
        .is_empty()
    );
    assert_eq!(
        names(TraceQuery {
            attributes: attributes(json!({"left":1,"right":2}))?,
            ..TraceQuery::default()
        })?,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            attributes: attributes(json!({"valid":true,"odd.\"key":"value"}))?,
            ..TraceQuery::default()
        })?,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            attributes: attributes(json!({"valid":1}))?,
            ..TraceQuery::default()
        })?,
        vec!["new"]
    );
    assert_eq!(
        names(TraceQuery {
            errors: true,
            ..TraceQuery::default()
        })?,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            start_min: Some(20000.0),
            ..TraceQuery::default()
        })?,
        vec!["new", "wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            start_max: Some(10000.0),
            ..TraceQuery::default()
        })?,
        vec!["old"]
    );
    Ok(())
}
#[test]
fn tied_trace_starts_page_complete_traces_without_duplicates() -> TestResult {
    let (c, clock) = timed(20000.0);
    let clock = clock.as_ref();
    for _ in 0..25 {
        clock.set(20000.0, 100.0);
        block_on(c.start_span(SpanOptions::new("root"), |root| async move {
            clock.set(20001.0, 101.0);
            root.start_span(SpanOptions::new("child"), |_| {
                ready(Ok::<_, ErrorDetails>(()))
            })
            .await
        }))?;
    }
    let first = block_on(c.find_traces(TraceQuery::default()))?;
    assert_eq!(first.traces.len(), 20);
    assert!(first.cursor.is_some());
    let rest = block_on(c.find_traces(TraceQuery {
        cursor: first.cursor.clone(),
        ..TraceQuery::default()
    }))?;
    assert_eq!(rest.traces.len(), 5);
    assert!(rest.cursor.is_none());
    let all = block_on(c.find_traces(TraceQuery {
        limit: Some(100),
        ..TraceQuery::default()
    }))?
    .traces;
    assert_eq!(
        first
            .traces
            .into_iter()
            .chain(rest.traces)
            .collect::<Vec<_>>(),
        all
    );
    assert!(all.iter().all(|t| t.spans.len() == 2));
    let mut cursor = None;
    let mut ids = vec![];
    loop {
        let page = block_on(c.find_traces(TraceQuery {
            name: Some("root".into()),
            limit: Some(3),
            cursor,
            ..TraceQuery::default()
        }))?;
        ids.extend(page.traces.into_iter().map(|t| t.trace_id));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, all.into_iter().map(|t| t.trace_id).collect::<Vec<_>>());
    Ok(())
}
#[test]
fn remote_trace_ids_parents_links_and_incomplete_fallbacks_are_preserved() -> TestResult {
    let sender = Context::new();
    let receiver = Context::new();
    let sent = saved(&sender, "sender")?;
    let carrier = sender.inject(&sent).ok_or("missing carrier")?;
    let remote = receiver.extract(Some(&carrier));
    saved(&remote, "receiver")?;
    block_on(receiver.start_span(
        SpanOptions {
            name: "linked".into(),
            links: vec![SpanLink {
                context: remote,
                attributes: attributes(json!({"reason":"background"}))?,
            }],
            ..SpanOptions::default()
        },
        |_| ready(Ok::<_, ErrorDetails>(())),
    ))?;
    let sent_trace = block_on(sender.find_traces(TraceQuery::default()))?
        .traces
        .remove(0);
    let continued =
        block_on(receiver.get_trace(&sent_trace.trace_id)).ok_or("missing remote trace")?;
    assert_eq!(continued.spans.len(), 1);
    assert_eq!(
        continued.spans[0].parent_id,
        Some(sent_trace.spans[0].id.clone())
    );
    assert_ne!(continued.spans[0].id, sent_trace.spans[0].id);
    let linked = block_on(receiver.find_traces(TraceQuery {
        name: Some("linked".into()),
        ..TraceQuery::default()
    }))?
    .traces
    .remove(0);
    assert_ne!(linked.trace_id, sent_trace.trace_id);
    assert_eq!(
        linked.spans[0].links,
        vec![StoredLink {
            trace_id: sent_trace.trace_id,
            span_id: sent_trace.spans[0].id.clone(),
            attributes: attributes(json!({"reason":"background"}))?
        }]
    );
    for key in carrier.keys() {
        let mut incomplete = carrier.clone();
        incomplete.shift_remove(key);
        saved(&receiver.extract(Some(&incomplete)), "fallback")?;
    }
    let fallbacks = block_on(receiver.find_traces(TraceQuery {
        name: Some("fallback".into()),
        ..TraceQuery::default()
    }))?
    .traces;
    assert_eq!(fallbacks.len(), 3);
    assert!(
        fallbacks
            .iter()
            .all(|t| t.spans[0].parent_id.is_none() && t.trace_id == t.spans[0].id)
    );
    Ok(())
}
#[test]
fn external_identifiers_have_no_enforced_id_format() -> TestResult {
    let c = Context::new();
    let carrier: TelemetryCarrier = [
        ("pi-memory-scope".into(), "external-scope".into()),
        ("pi-memory-span".into(), "external-span".into()),
        ("pi-memory-trace".into(), "external-trace".into()),
    ]
    .into_iter()
    .collect();
    let parent = c.extract(Some(&carrier));
    assert_eq!(c.inject(&parent), Some(carrier));
    saved(&parent, "continued")?;
    let trace = block_on(c.find_traces(TraceQuery::default()))?
        .traces
        .remove(0);
    assert_eq!(trace.trace_id, "external-trace");
    assert_eq!(
        trace.spans[0].parent_id.as_deref(),
        Some("external-scope:external-span")
    );
    Ok(())
}
#[test]
fn metric_queries_use_inclusive_intervals_typed_equality_and_detached_values() -> TestResult {
    let (c, clock) = timed(20001.0);
    let counter = c.counter(
        "hits",
        InstrumentOptions {
            unit: Some("By".into()),
            ..InstrumentOptions::default()
        },
    );
    counter.add(2.0, attributes(json!({"method":"read","valid":true}))?);
    counter.add(3.0, attributes(json!({"valid":true,"method":"read"}))?);
    counter.add(11.0, attributes(json!({"method":"write","valid":1}))?);
    c.counter("other", InstrumentOptions::default())
        .add(100.0, attributes(json!({"method":"read","valid":true}))?);
    clock.set(30001.0, 100.0);
    counter.add(7.0, attributes(json!({"method":"read","valid":true}))?);
    let query = MetricQuery {
        name: "hits".into(),
        start_min: Some(20000.0),
        start_max: Some(20000.0),
        attributes: attributes(json!({"method":"read","valid":true}))?,
    };
    let mut rows = block_on(c.find_metrics(query.clone()));
    assert_eq!(rows.iter().map(|r| r.count).sum::<u64>(), 2);
    assert_eq!(rows.iter().map(|r| r.sum).sum::<f64>(), 5.0);
    assert_eq!(
        rows.iter().map(|r| r.min).fold(f64::INFINITY, f64::min),
        2.0
    );
    assert_eq!(
        rows.iter().map(|r| r.max).fold(f64::NEG_INFINITY, f64::max),
        3.0
    );
    assert!(rows.iter().all(|r| r.kind == MetricKind::Counter
        && r.unit.as_deref() == Some("By")
        && r.interval_ms == 20000.0));
    rows[0].attributes.clear();
    assert_eq!(
        block_on(c.find_metrics(query))[0].attributes["method"],
        AttributeValue::from("read")
    );
    assert!(
        block_on(c.find_metrics(MetricQuery {
            name: "missing".into(),
            ..MetricQuery::default()
        }))
        .is_empty()
    );
    assert_eq!(
        block_on(c.find_metrics(MetricQuery {
            name: "hits".into(),
            attributes: attributes(json!({"valid":1}))?,
            ..MetricQuery::default()
        }))
        .iter()
        .map(|r| r.sum)
        .sum::<f64>(),
        11.0
    );
    assert_eq!(
        block_on(c.find_metrics(MetricQuery {
            name: "hits".into(),
            start_min: Some(30000.0),
            ..MetricQuery::default()
        }))
        .iter()
        .map(|r| r.sum)
        .sum::<f64>(),
        7.0
    );
    Ok(())
}
#[test]
fn wall_boundaries_and_relative_event_times_do_not_leak_into_raw_events() -> TestResult {
    let (c, clock) = timed(1000.0);
    let span = block_on(c.start_span(SpanOptions::new("timed"), |span| {
        clock.set(1000.0, 110.0);
        span.add_event(
            "event",
            [("count".into(), 1.0.into())].into_iter().collect(),
        );
        clock.set(1000.0, 125.0);
        ready(Ok::<_, ErrorDetails>(span))
    }))?;
    let id = c.inject(&span).ok_or("missing identity")?["pi-memory-trace"].clone();
    let trace = block_on(c.get_trace(&id)).ok_or("missing trace")?;
    assert_eq!(
        (
            trace.spans[0].started_ms,
            trace.spans[0].ended_ms,
            trace.spans[0].events[0].time_ms
        ),
        (1000.0, 1025.0, 10.0)
    );
    let raw = serde_json::to_value(c.get_spans())?;
    assert!(raw[0].get("startedMs").is_none());
    assert!(raw[0]["events"][0].get("timeMs").is_none());
    assert_eq!(
        raw[0]["events"][0],
        json!({"name":"event","attributes":{"count":1.0}})
    );
    Ok(())
}
#[test]
fn span_starts_keep_submillisecond_order_and_spacing() -> TestResult {
    let (c, clock) = timed(1000.0);
    block_on(c.start_span(SpanOptions::new("first"), |_| {
        clock.set(1000.0, 100.25);
        ready(Ok::<_, ErrorDetails>(()))
    }))?;
    block_on(c.start_span(SpanOptions::new("second"), |_| {
        clock.set(1000.0, 100.5);
        ready(Ok::<_, ErrorDetails>(()))
    }))?;
    let spans = block_on(c.find_traces(TraceQuery::default()))?
        .traces
        .into_iter()
        .flat_map(|t| t.spans)
        .map(|s| (s.name, s.started_ms, s.ended_ms))
        .collect::<Vec<_>>();
    assert_eq!(
        spans,
        vec![
            ("second".into(), 1000.25, 1000.5),
            ("first".into(), 1000.0, 1000.25)
        ]
    );
    Ok(())
}
#[test]
fn raw_metric_rows_are_count_one_without_private_timing() -> TestResult {
    let (c, _) = timed(20001.0);
    let counter = c.counter("calls", InstrumentOptions::default());
    counter.add(2.0, SpanAttributes::new());
    counter.add(3.0, SpanAttributes::new());
    let rows = block_on(c.find_metrics(MetricQuery {
        name: "calls".into(),
        ..MetricQuery::default()
    }));
    assert_eq!(
        rows.iter()
            .map(|r| (r.interval_ms, r.count, r.sum))
            .collect::<Vec<_>>(),
        vec![(20000.0, 1, 2.0), (20000.0, 1, 3.0)]
    );
    let raw = serde_json::to_value(c.get_metrics())?;
    assert!(raw[0].get("intervalMs").is_none());
    assert_eq!(
        raw[0],
        json!({"kind":"counter","name":"calls","value":2.0,"attributes":{},"context":null})
    );
    Ok(())
}
#[test]
fn stored_links_and_error_details_are_detached() -> TestResult {
    let c = Context::new();
    let target = saved(&c, "target")?;
    block_on(c.start_span(
        SpanOptions {
            name: "source".into(),
            links: vec![SpanLink {
                context: target,
                attributes: attributes(json!({"tags":["original"]}))?,
            }],
            ..SpanOptions::default()
        },
        |span| {
            span.set_status(SpanStatus::Error {
                error: Some(ErrorDetails {
                    name: "Expected".into(),
                    message: "original".into(),
                }),
            });
            ready(Ok::<_, ErrorDetails>(()))
        },
    ))?;
    let trace = block_on(c.find_traces(TraceQuery {
        name: Some("source".into()),
        ..TraceQuery::default()
    }))?
    .traces
    .remove(0);
    let mut changed = trace.clone();
    changed.spans[0].links[0].attributes.clear();
    changed.spans[0].status = SpanStatus::Ok;
    assert_eq!(block_on(c.get_trace(&trace.trace_id)), Some(trace));
    Ok(())
}
#[test]
fn anchored_clock_counts_microseconds_with_stationary_wall_milliseconds() {
    let source = Arc::new(FakeClock::at(50000.0, 1000.25));
    let clock = TelemetryClock::new(source.clone());
    let first = clock.now();
    source.set(50000.0, 1000.2504);
    assert_eq!(clock.now() - first, 0.0);
    source.set(50000.0, 1000.7512);
    assert!((clock.now() - first - 0.501).abs() < 0.000001);
    assert_eq!((clock.now() * 1000.0).fract(), 0.0);
}
#[test]
fn anchored_clock_follows_wall_steps_only_past_ten_milliseconds() {
    let source = Arc::new(FakeClock::at(80000.0, 1000.0));
    let clock = TelemetryClock::new(source.clone());
    assert_eq!(clock.now(), 80000.0);
    source.set(80005.0, 1003.5);
    assert_eq!(clock.now(), 80003.5);
    source.set(3650004.0, 1004.0);
    assert_eq!(clock.now(), 3650004.0);
    source.set(3650004.0, 1004.125);
    assert_eq!(clock.now(), 3650004.125);
}
