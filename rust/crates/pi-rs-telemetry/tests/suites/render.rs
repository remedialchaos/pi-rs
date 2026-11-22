use super::common::TestResult;
use pi_rs_telemetry::*;
use serde_json::{Value, json};
fn span(id: &str, overrides: Value) -> Result<StoredSpan, serde_json::Error> {
    let mut value = json!({"id":id,"traceId":"trace","name":id,"startedMs":0,"endedMs":10,"attributes":{},"events":[],"links":[],"status":{"status":"ok"}});
    if let (Some(base), Some(overrides)) = (value.as_object_mut(), overrides.as_object()) {
        base.extend(overrides.clone());
    }
    serde_json::from_value(value)
}
#[test]
fn renders_unordered_trace_with_attributes_durations_errors_events_links() -> TestResult {
    let root = span(
        "root",
        json!({"startedMs":1000,"endedMs":1020,"attributes":{"request":"a b","attempt":2,"cached":false,"tags":["a","b"],"sizes":[1,2],"flags":[true,false]},"events":[{"name":"checkpoint","timeMs":2,"attributes":{"phase":"tools"}},{"name":"tick","timeMs":3.5,"attributes":{}}],"links":[{"traceId":"remote-trace","spanId":"remote","attributes":{"reason":"steer"}},{"traceId":"other-trace","spanId":"other"}]}),
    )?;
    let first = span(
        "first",
        json!({"parentId":"root","startedMs":1005,"endedMs":1009.5,"status":{"status":"error","error":{"name":"Error","message":"failed call"}}}),
    )?;
    let nested = span(
        "nested",
        json!({"parentId":"first","startedMs":1006,"endedMs":1007}),
    )?;
    let second = span(
        "second",
        json!({"parentId":"root","startedMs":1010,"endedMs":1012,"status":{"status":"error"}}),
    )?;
    let spans = vec![second, nested, first, root];
    let before = spans.clone();
    assert_eq!(
        render_trace(&spans)?,
        "root request=\"a b\" attempt=2 cached=false tags=[\"a\",\"b\"] sizes=[1,2] flags=[true,false] (20ms)\n  · checkpoint phase=\"tools\" (+2ms)\n  · tick (+3.5ms)\n  → link trace remote-trace span remote reason=\"steer\"\n  → link trace other-trace span other\n  first (4.5ms) error Error: failed call\n    nested (1ms)\n  second (2ms) error"
    );
    assert_eq!(spans, before);
    Ok(())
}
#[test]
fn missing_parents_are_roots_in_start_order() -> TestResult {
    assert_eq!(
        render_trace(&[
            span(
                "later",
                json!({"parentId":"external","startedMs":20,"endedMs":30})
            )?,
            span(
                "child",
                json!({"parentId":"later","startedMs":25,"endedMs":30})
            )?,
            span(
                "earlier",
                json!({"startedMs":10,"endedMs":15,"links":[{"traceId":"trace","spanId":"later"}]})
            )?
        ])?,
        "earlier (5ms)\n  → link trace trace span later\nlater (10ms)\n  child (5ms)"
    );
    Ok(())
}
#[test]
fn cyclic_cross_trace_links_are_printed_not_followed() -> TestResult {
    assert_eq!(
        render_trace(&[
            span(
                "server:1",
                json!({"name":"server","traceId":"server-trace","links":[{"traceId":"worker-trace","spanId":"worker:1"}]})
            )?,
            span(
                "worker:1",
                json!({"name":"worker","traceId":"worker-trace","links":[{"traceId":"server-trace","spanId":"server:1"}]})
            )?
        ])?,
        "server (10ms)\n  → link trace worker-trace span worker:1\nworker (10ms)\n  → link trace server-trace span server:1"
    );
    Ok(())
}
#[test]
fn deduplicates_spans_from_stores_by_global_id() -> TestResult {
    let root = span("server:1", json!({"name":"root"}))?;
    let child = span(
        "worker:1",
        json!({"name":"child","parentId":"server:1","events":[{"name":"done","timeMs":3,"attributes":{}}]}),
    )?;
    let grandchild = span(
        "worker:2",
        json!({"name":"grandchild","parentId":"worker:1"}),
    )?;
    assert_eq!(
        render_trace(&[child.clone(), root.clone(), grandchild, root, child])?,
        "root (10ms)\n  child (10ms)\n    · done (+3ms)\n    grandchild (10ms)"
    );
    Ok(())
}
#[test]
fn equal_start_times_preserve_input_order() -> TestResult {
    let spans = vec![
        span("root", json!({}))?,
        span("z", json!({"parentId":"root"}))?,
        span("a", json!({"parentId":"root"}))?,
        span("other", json!({}))?,
    ];
    let before = spans.clone();
    assert_eq!(
        render_trace(&spans)?,
        "root (10ms)\n  z (10ms)\n  a (10ms)\nother (10ms)"
    );
    assert_eq!(spans, before);
    Ok(())
}
#[test]
fn string_attributes_are_json_escaped() -> TestResult {
    assert_eq!(
        render_trace(&[span(
            "root",
            json!({"attributes":{"text":"line one\n\"line two\""}})
        )?])?,
        "root text=\"line one\\n\\\"line two\\\"\" (10ms)"
    );
    Ok(())
}
#[test]
fn javascript_attribute_order_undefined_and_infinite_times_survive_adapter() -> TestResult {
    let mut root = span("root", json!({}))?;
    root.ended_ms = f64::INFINITY;
    root.attributes = [
        ("10".into(), 10.0.into()),
        ("2".into(), 2.0.into()),
        ("z".into(), AttributeValue::Undefined),
        ("01".into(), true.into()),
    ]
    .into_iter()
    .collect();
    let undefined = [("z".into(), AttributeValue::Undefined)]
        .into_iter()
        .collect::<SpanAttributes>();
    root.events.push(StoredEvent {
        name: "event".into(),
        time_ms: f64::INFINITY,
        attributes: undefined.clone(),
    });
    root.links.push(StoredLink {
        trace_id: "remote".into(),
        span_id: "span".into(),
        attributes: undefined,
    });
    assert_eq!(
        render_trace(&[root])?,
        "root 2=2 10=10 z=undefined 01=true (Infinityms)\n  · event z=undefined (+Infinityms)\n  → link trace remote span span z=undefined"
    );
    Ok(())
}
#[test]
fn deep_trees_do_not_recurse() -> TestResult {
    let spans = (0..2000)
        .map(|i| {
            span(
                &i.to_string(),
                if i == 0 {
                    json!({})
                } else {
                    json!({"parentId":(i-1).to_string()})
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let output = render_trace(&spans)?;
    assert_eq!(output.lines().count(), 2000);
    assert!(output.ends_with(&format!("{}1999 (10ms)", "  ".repeat(1999))));
    Ok(())
}
#[test]
fn durations_and_events_are_rounded_to_microseconds() -> TestResult {
    assert_eq!(
        render_trace(&[span(
            "root",
            json!({"startedMs":1793789452816.123,"endedMs":1793789452816.431,"events":[{"name":"half","timeMs":0.15400000002,"attributes":{}}]})
        )?])?,
        "root (0.308ms)\n  · half (+0.154ms)"
    );
    Ok(())
}
#[test]
fn empty_trace_renders_empty_string() -> TestResult {
    assert_eq!(render_trace(&[])?, "");
    Ok(())
}
#[test]
fn control_characters_in_all_terminal_fields_are_escaped() -> TestResult {
    let hostile = "\u{1b}]0;PWNED\u{7}\u{1b}[2J";
    let output = render_trace(&[span(
        "root",
        json!({"name":format!("name{hostile}"),"attributes":{format!("key{hostile}"):hostile},"events":[{"name":format!("event{hostile}"),"timeMs":1,"attributes":{}}],"links":[{"traceId":hostile,"spanId":format!("span\u{9b}{hostile}")}],"status":{"status":"error","error":{"name":format!("error{hostile}"),"message":format!("message\u{7f}{hostile}")}}}),
    )?])?;
    for line in output.lines() {
        assert!(
            !line
                .chars()
                .any(|c| matches!(c,'\u{0}'..='\u{1f}'|'\u{7f}'..='\u{9f}'))
        );
    }
    for expected in [
        "name\\u001b]0;PWNED\\u0007\\u001b[2J",
        "→ link trace \\u001b]0;PWNED",
        "span\\u009b",
        "message\\u007f",
        "key\\u001b]0;PWNED\\u0007\\u001b[2J=\"\\u001b]0;PWNED\\u0007\\u001b[2J\"",
    ] {
        assert!(output.contains(expected), "missing {expected}");
    }
    Ok(())
}
