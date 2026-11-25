use super::types::*;
use crate::*;
use std::future::{Future, ready};

pub fn create_telemetry_adapter_conformance() -> Vec<TelemetryAdapterConformanceCase> {
    [
        (
            "capabilities",
            "carries adapter capabilities through contexts and spans",
        ),
        (
            "metrics",
            "records ordered measurements and optional histogram exemplars",
        ),
        (
            "metrics",
            "copies instrument metadata and measurement attributes",
        ),
        (
            "links",
            "records initial and late links without changing parents",
        ),
        ("links", "ignores links added after source settlement"),
        (
            "propagation",
            "roundtrips parent identity through JSON without retaining the carrier",
        ),
        (
            "propagation",
            "absent and unknown carriers produce recorded roots",
        ),
        (
            "propagation",
            "propagates across fresh adapters and multiple hops",
        ),
        (
            "callback lifecycle",
            "admits once synchronously and preserves the result",
        ),
        (
            "callback lifecycle",
            "preserves synchronous and asynchronous rejection values",
        ),
        (
            "status",
            "uses last explicit status without automatic overwrite",
        ),
        (
            "attributes and events",
            "merges attributes and preserves ordered event snapshots",
        ),
        (
            "callback lifecycle",
            "makes post-settlement mutations and child spans inert",
        ),
        (
            "callback lifecycle",
            "isolates concurrent children and records settlement order",
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(kind, (group, name))| TelemetryAdapterConformanceCase { group, name, kind })
    .collect()
}
fn check(condition: bool, message: &str) -> Result<(), ConformanceError> {
    if condition {
        Ok(())
    } else {
        Err(ConformanceError(message.into()))
    }
}
fn attr(key: &str, value: impl Into<AttributeValue>) -> SpanAttributes {
    [(key.into(), value.into())].into_iter().collect()
}
async fn saved(context: &Context, name: &str) -> Result<Context, ConformanceError> {
    context
        .start_span(SpanOptions::new(name), |span| {
            ready(Ok::<_, ConformanceError>(span))
        })
        .await
}
impl TelemetryAdapterConformanceCase {
    pub async fn run<F, Fut, Fixture>(&self, factory: F) -> Result<(), ConformanceError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Fixture>,
        Fixture: TelemetryAdapterFixture,
    {
        let fixture = factory().await;
        let result = run(self.kind, &fixture, &factory).await;
        fixture.dispose().await;
        result
    }
}
async fn run<F, Fut, Fixture>(
    kind: usize,
    fixture: &Fixture,
    factory: &F,
) -> Result<(), ConformanceError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture>,
    Fixture: TelemetryAdapterFixture,
{
    let c = fixture.context();
    match kind {
        0 => {
            let span = saved(&c, "capabilities").await?;
            let parent = c.extract(c.inject(&span).as_ref());
            let inert = saved(&span, "inert").await?;
            for context in [&c, &span, &parent, &inert] {
                check(
                    context.metrics().same_instance(&c.metrics()),
                    "metrics capability changed",
                )?;
                check(
                    context.propagation().same_instance(&c.propagation()),
                    "propagation capability changed",
                )?;
            }
        }
        1 => {
            c.counter(
                "bytes",
                InstrumentOptions {
                    description: Some("Encoded bytes".into()),
                    unit: Some("By".into()),
                },
            )
            .add(12.0, attr("direction", "out"));
            let active = c.up_down_counter("active", InstrumentOptions::default());
            active.add(1.0, SpanAttributes::new());
            active.add(-1.0, SpanAttributes::new());
            let histogram = c.histogram(
                "duration",
                HistogramOptions {
                    instrument: InstrumentOptions {
                        description: None,
                        unit: Some("ms".into()),
                    },
                    boundaries: Some(vec![1.0, 10.0]),
                },
            );
            let span = saved(&c, "measured").await?;
            histogram.record(5.0, attr("method", "read"), Some(&span));
            let rows = fixture.get_metrics().await;
            check(
                rows.iter().map(|m| (m.kind, m.value)).collect::<Vec<_>>()
                    == vec![
                        (MetricKind::Counter, 12.0),
                        (MetricKind::UpDownCounter, 1.0),
                        (MetricKind::UpDownCounter, -1.0),
                        (MetricKind::Histogram, 5.0),
                    ],
                "measurement order changed",
            )?;
            check(
                rows[0].description.as_deref() == Some("Encoded bytes")
                    && rows[0].unit.as_deref() == Some("By")
                    && rows[0].attributes == attr("direction", "out"),
                "counter metadata changed",
            )?;
            check(
                rows[3].boundaries == Some(vec![1.0, 10.0]) && rows[3].context == c.inject(&span),
                "histogram exemplar changed",
            )?;
        }
        2 => {
            let mut options = HistogramOptions {
                instrument: InstrumentOptions {
                    description: Some("Original".into()),
                    unit: None,
                },
                boundaries: Some(vec![1.0, 10.0]),
            };
            let instrument = c.histogram("snapshot", options.clone());
            options.instrument.description = Some("Changed".into());
            options.boundaries = Some(vec![100.0]);
            let mut attributes = attr("tags", AttributeValue::Strings(vec!["original".into()]));
            instrument.record(-1.0, attributes.clone(), None);
            attributes.insert("tags".into(), "changed".into());
            let rows = fixture.get_metrics().await;
            check(
                rows.len() == 1
                    && rows[0].description.as_deref() == Some("Original")
                    && rows[0].boundaries == Some(vec![1.0, 10.0])
                    && rows[0].attributes["tags"]
                        == AttributeValue::Strings(vec!["original".into()]),
                "metadata was not detached",
            )?;
        }
        3 => {
            let target = saved(&c, "target").await?;
            c.start_span(
                SpanOptions {
                    name: "linked".into(),
                    links: vec![SpanLink {
                        context: target.clone(),
                        attributes: attr("reason", AttributeValue::Strings(vec!["initial".into()])),
                    }],
                    ..SpanOptions::default()
                },
                |span| {
                    span.add_link(SpanLink {
                        context: target.clone(),
                        attributes: attr("reason", "late"),
                    });
                    span.add_link(SpanLink {
                        context: Context::noop(),
                        attributes: SpanAttributes::new(),
                    });
                    ready(Ok::<_, ConformanceError>(()))
                },
            )
            .await?;
            let rows = fixture.get_spans().await;
            check(
                rows[1].parent_context.is_none()
                    && rows[1].links.len() == 2
                    && rows[1].links[0].context == rows[0].context
                    && rows[1].links[1].attributes == attr("reason", "late"),
                "links changed parent or retained unknown context",
            )?;
        }
        4 => {
            let source = saved(&c, "source").await?;
            source.add_link(SpanLink {
                context: source.clone(),
                attributes: SpanAttributes::new(),
            });
            check(
                fixture.get_spans().await[0].links.is_empty(),
                "settled link retained",
            )?;
        }
        5 => {
            let sender = saved(&c, "sender").await?;
            let carrier = c
                .inject(&sender)
                .ok_or_else(|| ConformanceError("missing sender carrier".into()))?;
            let mut wire: TelemetryCarrier = serde_json::from_str(
                &serde_json::to_string(&carrier).map_err(|e| ConformanceError(e.to_string()))?,
            )
            .map_err(|e| ConformanceError(e.to_string()))?;
            let parent = c.extract(Some(&wire));
            wire.values_mut()
                .for_each(|value| *value = "mutated".into());
            saved(&parent, "receiver").await?;
            check(
                fixture.get_spans().await[1].parent_context == Some(carrier.clone())
                    && c.inject(&sender) == Some(carrier),
                "carrier mutation leaked",
            )?;
        }
        6 => {
            check(
                c.inject(&Context::noop()).is_none(),
                "unknown identity injected",
            )?;
            for carrier in [
                None,
                Some(TelemetryCarrier::new()),
                Some([("unknown".into(), "format".into())].into_iter().collect()),
            ] {
                saved(&c.extract(carrier.as_ref()), "fallback").await?;
            }
            check(
                fixture
                    .get_spans()
                    .await
                    .iter()
                    .all(|s| s.parent_context.is_none()),
                "fallback was not root",
            )?;
        }
        7 => {
            let receiver = factory().await;
            let worker = factory().await;
            let sender = saved(&c, "sender").await?;
            let carrier = c.inject(&sender);
            let received = saved(&receiver.context().extract(carrier.as_ref()), "receiver").await?;
            let next = receiver.context().inject(&received);
            saved(&worker.context().extract(next.as_ref()), "worker").await?;
            let valid = receiver.get_spans().await[0].parent_context == carrier
                && worker.get_spans().await[0].parent_context == next;
            receiver.dispose().await;
            worker.dispose().await;
            check(valid, "hop parent identity changed")?;
        }
        8 => {
            let calls = std::cell::Cell::new(0);
            let expected = std::sync::Arc::new(42);
            let future = c.start_span(SpanOptions::new("success"), |_| {
                calls.set(calls.get() + 1);
                ready(Ok::<_, ConformanceError>(expected.clone()))
            });
            check(calls.get() == 1, "callback not admitted exactly once")?;
            check(
                std::sync::Arc::ptr_eq(&future.await?, &expected),
                "result identity changed",
            )?;
            check(fixture.get_spans().await[0].settled, "span not settled")?;
        }
        9 => {
            let error = std::sync::Arc::new("sync");
            let result = c
                .start_span(SpanOptions::new("sync-error"), |_| {
                    ready(Err::<(), _>(error.clone()))
                })
                .await;
            check(
                result
                    .as_ref()
                    .is_err_and(|actual| std::sync::Arc::ptr_eq(actual, &error)),
                "rejection identity changed",
            )?;
            let result = c
                .start_span(SpanOptions::new("async-error"), |_| async {
                    Err::<(), _>(42)
                })
                .await;
            check(result == Err(42), "async rejection changed")?;
            let result = c
                .start_span(SpanOptions::new("undefined-error"), |_| {
                    ready(Err::<(), _>(()))
                })
                .await;
            check(
                result == Err(())
                    && fixture
                        .get_spans()
                        .await
                        .iter()
                        .all(|s| matches!(s.status, SpanStatus::Error { .. })),
                "non-error rejection not recorded",
            )?;
        }
        10 => {
            c.start_span(SpanOptions::new("last-status"), |span| {
                span.set_status(SpanStatus::Error { error: None });
                span.set_status(SpanStatus::Ok);
                ready(Ok::<_, ConformanceError>(()))
            })
            .await?;
            let result = c
                .start_span(SpanOptions::new("explicit-status"), |span| {
                    span.set_status(SpanStatus::Ok);
                    ready(Err::<(), _>("thrown"))
                })
                .await;
            check(
                result == Err("thrown")
                    && fixture
                        .get_spans()
                        .await
                        .iter()
                        .all(|s| s.status == SpanStatus::Ok),
                "automatic status overwrote explicit status",
            )?;
        }
        11 => {
            c.start_span(
                SpanOptions {
                    name: "attributes".into(),
                    attributes: attr("keep", "original"),
                    ..SpanOptions::default()
                },
                |span| {
                    span.set_attributes(
                        [
                            ("keep".into(), AttributeValue::Undefined),
                            ("last".into(), true.into()),
                        ]
                        .into_iter()
                        .collect(),
                    );
                    span.add_event(
                        "first",
                        attr("tags", AttributeValue::Strings(vec!["original".into()])),
                    );
                    span.add_event("second", SpanAttributes::new());
                    ready(Ok::<_, ConformanceError>(()))
                },
            )
            .await?;
            let rows = fixture.get_spans().await;
            check(
                rows[0].attributes["keep"] == AttributeValue::from("original")
                    && rows[0]
                        .attributes
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        == vec!["keep", "last"]
                    && rows[0]
                        .events
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        == vec!["first", "second"],
                "attributes or ordered events changed",
            )?;
        }
        12 => {
            let span = saved(&c, "settled").await?;
            let before = fixture.get_spans().await;
            span.set_attributes(attr("late", true));
            span.add_event("late", SpanAttributes::new());
            span.set_status(SpanStatus::Error { error: None });
            let result = span
                .start_span(SpanOptions::new("late-child"), |child| {
                    child.add_event("ignored", SpanAttributes::new());
                    ready(Ok::<_, ConformanceError>(42))
                })
                .await?;
            check(
                result == 42 && fixture.get_spans().await == before,
                "settled context mutated or recorded child",
            )?;
        }
        13 => {
            c.start_span(SpanOptions::new("parent"), |parent| async move {
                let first = parent.start_span(SpanOptions::new("first"), |_| {
                    ready(Ok::<_, ConformanceError>(()))
                });
                let second = parent.start_span(SpanOptions::new("second"), |_| {
                    ready(Ok::<_, ConformanceError>(()))
                });
                second.await?;
                first.await
            })
            .await?;
            let rows = fixture.get_spans().await;
            check(
                rows.iter().map(|s| s.parent_id).collect::<Vec<_>>()
                    == vec![None, Some(1.0), Some(1.0)]
                    && rows.iter().map(|s| s.end_sequence).collect::<Vec<_>>()
                        == vec![Some(3), Some(2), Some(1)],
                "concurrent parent or settlement order changed",
            )?;
        }
        _ => return Err(ConformanceError("unknown conformance case".into())),
    }
    Ok(())
}
