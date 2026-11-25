use pi_rs_chord::{context::*, delta::Op, json::JsonValue, watch::*};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn j(n: f64) -> JsonValue {
    JsonValue::Number(n)
}

#[tokio::test]
async fn contexts_layer_identity_cancel_first_reason_and_mask_cleanup()
-> Result<(), Box<dyn std::error::Error>> {
    let a = ContextKey::<String>::new("a");
    let other = ContextKey::<String>::new("a");
    let root = Context::background();
    let base = root.with_value(&a, "parent".to_owned());
    let (parent, cancel_parent) = base.with_cancel();
    let (child, cancel_child) = parent.with_cancel();
    let (sibling, _) = parent.with_cancel();
    assert_eq!(base.value(&a).as_deref(), Some("parent"));
    assert!(base.value(&other).is_none());
    assert!(root.value(&a).is_none());
    assert_eq!(
        base.with_value(&a, "child".to_owned()).value(&a).as_deref(),
        Some("child")
    );
    cancel_child.cancel("child");
    cancel_child.cancel("ignored");
    assert_eq!(
        child.abort_signal().and_then(|s| s.reason()),
        Some("child".into())
    );
    assert!(!parent.is_aborted());
    assert!(!sibling.is_aborted());
    let cleanup = child.without_abort_signal();
    assert!(cleanup.abort_signal().is_none());
    assert_eq!(cleanup.value(&a).as_deref(), Some("parent"));
    cancel_parent.cancel("parent");
    assert!(sibling.is_aborted());
    assert_eq!(
        child.abort_signal().and_then(|s| s.reason()),
        Some("child".into())
    );
    assert!(
        child
            .await_with(std::future::pending::<()>())
            .await
            .is_err()
    );
    assert_eq!(root.to_string(), "[Context BACKGROUND_CONTEXT]");
    assert_ne!(root.to_string(), Context::todo().to_string());
    Ok(())
}

#[tokio::test]
async fn trace_context_preserves_values_and_records_nested_parentage()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_telemetry::{InMemoryTelemetryContext, SpanOptions, TelemetryContext};
    let telemetry = InMemoryTelemetryContext::new();
    let parent = telemetry.open_span(SpanOptions::new("parent"));
    let key = ContextKey::<u32>::new("value");
    let ctx = Context::background()
        .with_value(&key, 42)
        .with_telemetry(parent);
    let result: Result<u32, String> = ctx
        .trace_in(SpanOptions::new("child"), |child| async move {
            assert_eq!(child.value(&key), Some(42));
            child
                .trace_in(SpanOptions::new("nested"), |_| async { Ok(7) })
                .await
        })
        .await;
    assert_eq!(result, Ok(7));
    let spans = telemetry.get_spans();
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[2].parent_id, Some(spans[1].id as f64));
    Ok(())
}

#[tokio::test]
async fn json_watch_folds_before_start_keeps_acquisition_value_and_masks_context()
-> Result<(), Box<dyn std::error::Error>> {
    let count = Arc::new(AtomicUsize::new(0));
    let detached = count.clone();
    let watch = Watch::new(
        j(0.0),
        move || {
            detached.fetch_add(1, Ordering::SeqCst);
        },
        |_| {},
    );
    let (ctx, _) = Context::background().with_cancel();
    watch.advance(
        WatchCall::Value {
            value: j(1.0),
            ops: vec![Op::Replace(j(1.0))],
            retired: true,
        },
        ctx.clone(),
    );
    watch.advance(
        WatchCall::Value {
            value: j(2.0),
            ops: vec![Op::Replace(j(2.0))],
            retired: false,
        },
        ctx,
    );
    assert_eq!(watch.value(), j(0.0));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    watch.start(move |call, context| {
        let tx = tx.clone();
        async move {
            assert!(context.abort_signal().is_none());
            tx.send(call).map_err(|e| e.to_string())
        }
    })?;
    assert!(rx.try_recv().is_err());
    assert_eq!(
        rx.recv().await,
        Some(WatchCall::Value {
            value: j(2.0),
            ops: vec![Op::Replace(j(2.0))],
            retired: true
        })
    );
    assert_eq!(watch.value(), j(2.0));
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(watch.start(|_, _| async { Ok(()) }).is_err());
    Ok(())
}

#[tokio::test]
async fn watch_serializes_busy_listener_and_stop_does_not_wait_for_it()
-> Result<(), Box<dyn std::error::Error>> {
    let watch = Watch::new(j(0.0), || {}, |_| {});
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(AtomicUsize::new(0));
    let (g, e, d) = (gate.clone(), entered.clone(), done.clone());
    watch.start(move |_, _| {
        let (g, e, d) = (g.clone(), e.clone(), d.clone());
        async move {
            e.notify_one();
            g.notified().await;
            d.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    })?;
    watch.advance(
        WatchCall::Value {
            value: j(1.0),
            ops: vec![Op::Replace(j(1.0))],
            retired: false,
        },
        Context::background(),
    );
    entered.notified().await;
    watch.advance(
        WatchCall::Value {
            value: j(2.0),
            ops: vec![Op::Replace(j(2.0))],
            retired: false,
        },
        Context::background(),
    );
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(watch.closed().await, WatchEnd::Stopped);
    assert_eq!(done.load(Ordering::SeqCst), 0);
    gate.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(done.load(Ordering::SeqCst), 1);
    assert_eq!(watch.value(), j(1.0));
    Ok(())
}

#[tokio::test]
async fn failure_drops_pending_and_heal_replaces_root_with_retirement()
-> Result<(), Box<dyn std::error::Error>> {
    let watch = Watch::new(j(0.0), || {}, |_| {});
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    watch.start(move |call, _| {
        let tx = tx.clone();
        async move { tx.send(call).map_err(|e| e.to_string()) }
    })?;
    watch.advance(
        WatchCall::Value {
            value: j(1.0),
            ops: vec![],
            retired: true,
        },
        Context::background(),
    );
    watch.fail("temporarily broken", Context::background());
    assert_eq!(
        rx.recv().await,
        Some(WatchCall::Failed {
            error: "temporarily broken".into()
        })
    );
    assert_eq!(watch.value(), j(0.0));
    watch.advance(
        WatchCall::Value {
            value: j(2.0),
            ops: vec![],
            retired: false,
        },
        Context::background(),
    );
    watch.end_after_delivery();
    assert_eq!(
        rx.recv().await,
        Some(WatchCall::Value {
            value: j(2.0),
            ops: vec![Op::Replace(j(2.0))],
            retired: true
        })
    );
    assert_eq!(watch.closed().await, WatchEnd::Ended);
    Ok(())
}

#[tokio::test]
async fn native_watch_preserves_order_collapses_101_and_reports_listener_error()
-> Result<(), Box<dyn std::error::Error>> {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let r = reports.clone();
    let watch = NativeWatch::new(
        0,
        || {},
        move |error| {
            if let Ok(mut r) = r.lock() {
                r.push(error)
            }
        },
    );
    for n in 1..=101 {
        watch.advance(n, Context::background());
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    watch.start(move |call, _| {
        let tx = tx.clone();
        async move {
            tx.send(call).map_err(|e| e.to_string())?;
            Err("listener broke".into())
        }
    })?;
    assert_eq!(rx.recv().await, Some(NativeWatchCall::Value { value: 101 }));
    assert_eq!(
        watch.closed().await,
        WatchEnd::ListenerError("listener broke".into())
    );
    assert_eq!(
        *reports.lock().map_err(|e| e.to_string())?,
        vec!["listener broke"]
    );
    Ok(())
}

#[tokio::test]
async fn acquired_abort_terminates_and_releases_source_callbacks()
-> Result<(), Box<dyn std::error::Error>> {
    let (ctx, cancel) = Context::background().with_cancel();
    let watch = Watch::new(j(0.0), || {}, |_| {});
    watch.observe_abort(ctx.abort_signal().ok_or("no signal")?)?;
    cancel.cancel("closed");
    assert_eq!(watch.closed().await, WatchEnd::Aborted);
    Ok(())
}
