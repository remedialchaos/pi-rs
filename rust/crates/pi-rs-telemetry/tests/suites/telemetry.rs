use super::common::*;
use futures::executor::block_on;
use pi_rs_telemetry::*;
use serde_json::json;
use std::{cell::Cell, future::ready, sync::Arc};
#[test]
fn schema_identity_preserves_serializable_definitions() -> TestResult {
    let definition = Arc::new(
        json!({"version":1,"spans":{"operation":{"description":"Test operation","parents":{"kind":"any"},"startAttributes":{"kind":{"type":"string","required":true,"values":["read","write"],"description":"Kind"}},"endAttributes":{},"events":{"result":{"description":"Result","attributes":{"outcome":{"type":"string","required":true,"values":["ok","error"],"description":"Outcome"}}}},"status":{"default":"ok","errorWhen":"The operation fails"}}}}),
    );
    let schema = define_telemetry_schema(definition.clone());
    assert!(Arc::ptr_eq(&schema, &definition));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(schema.as_ref())?)?,
        *definition
    );
    Ok(())
}
#[test]
fn combined_schema_child_starters_bind_explicit_parents_and_preserve_errors() -> TestResult {
    let c = Context::new();
    let schemas = [
        json!({"version":1,"spans":{"operation":{}}}),
        json!({"version":3,"spans":{"request":{}}}),
    ];
    let starter = create_typed_span_starter(c.clone(), &schemas);
    let result = block_on(starter.start(
        "operation",
        attributes(json!({"kind":"read"}))?,
        |_, child| async move {
            child
                .start(
                    "request",
                    [("provider".into(), "example".into())]
                        .into_iter()
                        .collect(),
                    |span, _| {
                        span.set_attributes(
                            [("response".into(), "cached".into())].into_iter().collect(),
                        );
                        ready(Ok::<_, ErrorDetails>(42))
                    },
                )
                .await
        },
    ))?;
    assert_eq!(result, 42);
    let spans = c.get_spans();
    assert_eq!(spans[0].parent_id, None);
    assert_eq!(spans[1].parent_id, Some(spans[0].id as f64));
    let error = Arc::new("sync");
    let result = block_on(starter.start("operation", SpanAttributes::new(), |_, _| {
        ready(Err::<(), _>(error.clone()))
    }));
    assert!(
        result
            .as_ref()
            .is_err_and(|actual| Arc::ptr_eq(actual, &error))
    );
    let result = block_on(
        starter.start("request", SpanAttributes::new(), |_, _| async {
            Err::<(), _>("async")
        }),
    );
    assert_eq!(result, Err("async"));
    Ok(())
}
#[test]
fn noop_admits_synchronously_and_reuses_inert_capabilities() -> TestResult {
    let admitted = Cell::new(false);
    let future = NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("first"), |span| {
        admitted.set(true);
        async move {
            let child = span
                .start_span(SpanOptions::new("child"), |child| {
                    ready(Ok::<_, ErrorDetails>(child))
                })
                .await?;
            assert!(child.metrics().same_instance(&span.metrics()));
            assert!(child.propagation().same_instance(&span.propagation()));
            Ok::<_, ErrorDetails>(42)
        }
    });
    assert!(admitted.get());
    assert_eq!(block_on(future)?, 42);
    assert!(
        NOOP_TELEMETRY_CONTEXT
            .metrics()
            .same_instance(&NOOP_TELEMETRY_METRICS)
    );
    assert!(
        NOOP_TELEMETRY_CONTEXT
            .propagation()
            .same_instance(&NOOP_TELEMETRY_PROPAGATION)
    );
    Ok(())
}
#[test]
fn noop_preserves_sync_and_async_rejection_identity() {
    let error = Arc::new("sync");
    let result = block_on(
        NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("sync"), |_| {
            ready(Err::<(), _>(error.clone()))
        }),
    );
    assert!(
        result
            .as_ref()
            .is_err_and(|actual| Arc::ptr_eq(actual, &error))
    );
    let result = block_on(
        NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("async"), |_| async {
            Err::<(), _>("async")
        }),
    );
    assert_eq!(result, Err("async"));
}
#[test]
fn noop_ignores_recording_payloads_and_links() -> TestResult {
    let result = block_on(NOOP_TELEMETRY_CONTEXT.start_span(
        SpanOptions {
            name: "operation".into(),
            attributes: attributes(json!({"kind":"test"}))?,
            ..SpanOptions::default()
        },
        |span| {
            span.add_event(
                "event",
                [("count".into(), 1.0.into())].into_iter().collect(),
            );
            span.set_attributes([("complete".into(), true.into())].into_iter().collect());
            span.set_status(SpanStatus::Error {
                error: Some(ErrorDetails {
                    name: "Expected".into(),
                    message: "test".into(),
                }),
            });
            span.add_link(SpanLink {
                context: span.clone(),
                attributes: SpanAttributes::new(),
            });
            assert!(span.get_spans().is_empty());
            assert!(span.inject(&span).is_none());
            ready(Ok::<_, ErrorDetails>(42))
        },
    ))?;
    assert_eq!(result, 42);
    assert!(NOOP_TELEMETRY_CONTEXT.get_spans().is_empty());
    Ok(())
}
