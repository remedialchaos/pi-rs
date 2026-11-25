use super::common::*;
use futures::executor::block_on;
use pi_rs_telemetry::*;
use serde_json::json;
use std::{cell::RefCell, future::ready, process::Command};
#[test]
fn parent_continues_in_another_process_without_shared_state() -> TestResult {
    let c = Context::new();
    let sender = saved(&c, "caller")?;
    let carrier = c.inject(&sender).ok_or("missing carrier")?;
    let output = Command::new(env!("CARGO_BIN_EXE_telemetry-propagation-worker"))
        .arg(serde_json::to_string(&carrier)?)
        .output()?;
    assert!(
        output.status.success(),
        "worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<RecordedTelemetrySpan> = serde_json::from_slice(&output.stdout)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].parent_context, Some(carrier.clone()));
    assert!(rows[0].parent_id.is_none());
    assert_ne!(rows[0].context, carrier);
    Ok(())
}
#[test]
fn crossed_remote_parents_remain_isolated_and_link_a_shared_root() -> TestResult {
    let caller = Context::new();
    let provider = Context::new();
    let first = saved(&caller, "first")?;
    let later = saved(&caller, "later")?;
    let first_parent = provider.extract(caller.inject(&first).as_ref());
    let later_parent = provider.extract(caller.inject(&later).as_ref());
    let first_handle = RefCell::new(None);
    let first_future = first_parent.start_span(SpanOptions::new("first.method"), |span| {
        first_handle.replace(Some(span));
        ready(Ok::<_, ErrorDetails>(()))
    });
    let first_method = first_handle
        .borrow()
        .clone()
        .ok_or("missing first method")?;
    let activation_handle = RefCell::new(None);
    let activation = provider.start_span(
        SpanOptions {
            name: "activation".into(),
            links: vec![SpanLink {
                context: first_method.clone(),
                attributes: SpanAttributes::new(),
            }],
            ..SpanOptions::default()
        },
        |span| {
            activation_handle.replace(Some(span));
            ready(Ok::<_, ErrorDetails>(()))
        },
    );
    let activation_span = activation_handle
        .borrow()
        .clone()
        .ok_or("missing activation")?;
    first_method.add_link(SpanLink {
        context: activation_span.clone(),
        attributes: SpanAttributes::new(),
    });
    let later_future = later_parent.start_span(SpanOptions::new("later.method"), |span| {
        span.add_link(SpanLink {
            context: activation_span,
            attributes: SpanAttributes::new(),
        });
        ready(Ok::<_, ErrorDetails>(()))
    });
    block_on(activation)?;
    block_on(later_future)?;
    block_on(first_future)?;
    let spans = provider.get_spans();
    let root = &spans[1];
    assert!(root.parent_context.is_none());
    assert_eq!(
        spans[0].parent_context,
        Some(caller.get_spans()[0].context.clone())
    );
    assert_eq!(
        spans[2].parent_context,
        Some(caller.get_spans()[1].context.clone())
    );
    assert_eq!(root.links[0].context, spans[0].context);
    assert_eq!(spans[2].links[0].context, root.context);
    Ok(())
}
#[test]
fn carrier_and_link_snapshots_detach_and_settled_target_identity_survives() -> TestResult {
    let c = Context::new();
    let target = saved(&c, "target")?;
    let original = c.inject(&target).ok_or("missing identity")?;
    let mut injected = c.inject(&target).ok_or("missing copy")?;
    injected.insert("pi-memory-span".into(), "999".into());
    assert_eq!(c.inject(&target), Some(original.clone()));
    block_on(c.start_span(
        SpanOptions {
            name: "linked".into(),
            links: vec![SpanLink {
                context: target,
                attributes: attributes(json!({"tags":["original"]}))?,
            }],
            ..SpanOptions::default()
        },
        |_| ready(Ok::<_, ErrorDetails>(())),
    ))?;
    let mut copies = c.get_spans();
    copies[0].context.clear();
    copies[1].links[0].context.clear();
    copies[1].links[0].attributes.clear();
    assert_eq!(
        c.get_spans()[1].links,
        vec![RecordedTelemetryLink {
            context: original,
            attributes: attributes(json!({"tags":["original"]}))?
        }]
    );
    Ok(())
}
#[test]
fn new_root_can_link_an_extracted_remote_target() -> TestResult {
    let sender = Context::new();
    let receiver = Context::new();
    let trigger = saved(&sender, "trigger")?;
    let carrier = sender.inject(&trigger).ok_or("missing carrier")?;
    let target = receiver.extract(Some(&carrier));
    block_on(receiver.start_span(
        SpanOptions {
            name: "background".into(),
            links: vec![SpanLink {
                context: target.clone(),
                attributes: attributes(json!({"reason":"restart"}))?,
            }],
            ..SpanOptions::default()
        },
        |span| {
            span.add_link(SpanLink {
                context: target,
                attributes: SpanAttributes::new(),
            });
            ready(Ok::<_, ErrorDetails>(()))
        },
    ))?;
    let rows = receiver.get_spans();
    assert!(rows[0].parent_context.is_none());
    assert_eq!(
        rows[0].links,
        vec![
            RecordedTelemetryLink {
                context: carrier.clone(),
                attributes: attributes(json!({"reason":"restart"}))?
            },
            RecordedTelemetryLink {
                context: carrier,
                attributes: SpanAttributes::new()
            }
        ]
    );
    Ok(())
}
#[test]
fn absent_and_unknown_carriers_use_root_and_noop_discards_inputs() -> TestResult {
    let c = Context::new();
    for carrier in [
        None,
        Some(TelemetryCarrier::new()),
        Some([("unknown".into(), "format".into())].into_iter().collect()),
    ] {
        let parent = c.extract(carrier.as_ref());
        assert!(parent.metrics().same_instance(&c.metrics()));
        saved(&parent, "root")?;
    }
    assert!(c.get_spans().iter().all(|s| s.parent_context.is_none()));
    assert!(
        NOOP_TELEMETRY_PROPAGATION
            .inject(&NOOP_TELEMETRY_CONTEXT)
            .is_none()
    );
    let extracted = NOOP_TELEMETRY_PROPAGATION.extract(Some(&TelemetryCarrier::new()));
    assert!(extracted.metrics().same_instance(&NOOP_TELEMETRY_METRICS));
    block_on(
        NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("noop"), |span| {
            span.add_link(SpanLink {
                context: span.clone(),
                attributes: SpanAttributes::new(),
            });
            ready(Ok::<_, ErrorDetails>(()))
        }),
    )?;
    assert!(NOOP_TELEMETRY_CONTEXT.get_spans().is_empty());
    Ok(())
}
#[test]
fn recognized_carrier_identifiers_are_opaque_strings() -> TestResult {
    let c = Context::new();
    let carrier: TelemetryCarrier = [
        ("pi-memory-scope".into(), "external-recorder".into()),
        ("pi-memory-span".into(), "external-span".into()),
        ("pi-memory-trace".into(), "external-trace".into()),
    ]
    .into_iter()
    .collect();
    saved(&c.extract(Some(&carrier)), "receiver")?;
    assert_eq!(c.get_spans()[0].parent_context, Some(carrier));
    Ok(())
}
