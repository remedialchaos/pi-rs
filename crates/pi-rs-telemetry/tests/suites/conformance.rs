use futures::executor::block_on;
use pi_rs_telemetry::{testing::*, *};
use std::future::ready;

fn run_case(index: usize) -> Result<(), ConformanceError> {
    block_on(
        create_telemetry_adapter_conformance()[index]
            .run(|| ready(InMemoryTelemetryContext::new())),
    )
}
macro_rules! case {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() -> Result<(), ConformanceError> {
            run_case($index)
        }
    };
}
case!(capabilities_survive_child_extraction_and_inert_contexts, 0);
case!(measurements_are_ordered_with_histogram_exemplars, 1);
case!(instrument_metadata_and_measurement_attributes_are_copied, 2);
case!(initial_and_late_links_do_not_change_parents, 3);
case!(settled_source_ignores_late_links, 4);
case!(json_roundtrip_preserves_parent_without_retaining_carrier, 5);
case!(unknown_carriers_record_roots, 6);
case!(fresh_adapters_propagate_across_multiple_hops, 7);
case!(
    callback_is_admitted_once_synchronously_and_preserves_result,
    8
);
case!(sync_async_and_nonerror_rejections_preserve_identity, 9);
case!(last_explicit_status_is_not_overwritten_automatically, 10);
case!(attributes_merge_and_events_keep_order, 11);
case!(settled_mutations_and_children_are_inert, 12);
case!(concurrent_children_preserve_parent_and_settlement_order, 13);

#[test]
fn normalized_snapshots_are_detached() -> Result<(), Box<dyn std::error::Error>> {
    let context = InMemoryTelemetryContext::new();
    block_on(
        context.start_span(
            SpanOptions {
                name: "snapshot".into(),
                attributes: [(
                    "tags".into(),
                    AttributeValue::Strings(vec!["original".into()]),
                )]
                .into_iter()
                .collect(),
                ..SpanOptions::default()
            },
            |span| {
                span.add_event(
                    "event",
                    [(
                        "tags".into(),
                        AttributeValue::Strings(vec!["original".into()]),
                    )]
                    .into_iter()
                    .collect(),
                );
                span.set_status(SpanStatus::Error {
                    error: Some(ErrorDetails {
                        name: "Expected".into(),
                        message: "original".into(),
                    }),
                });
                ready(Ok::<_, ErrorDetails>(()))
            },
        ),
    )?;
    let before = context.get_spans();
    let mut copy = context.get_spans();
    copy[0].context.clear();
    copy[0].attributes.clear();
    copy[0].events[0].attributes.clear();
    copy[0].status = SpanStatus::Ok;
    assert_eq!(context.get_spans(), before);
    Ok(())
}
