use pi_rs_chord::{
    Result,
    context::Context,
    delta::{Op, Revision, Segment},
    json::JsonValue,
    state::{Batch, BatchError, NativeSource, Source},
    watch::WatchCall,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn j(s: &str) -> JsonValue {
    // INFALLIBLE: callers supply authored literal JSON fixtures.
    serde_json::from_str(s).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn rev(s: &str) -> Result<Revision> {
    Ok(Revision::from_json(j(s))?)
}
fn quiet() -> Batch {
    Batch::new(|_| {})
}
fn hold<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}
#[tokio::test]
async fn batch_coalesces_frames_after_adoption_then_flushes_and_defers_flush_changes() -> Result<()>
{
    let batch = quiet();
    let source = Source::new(batch.clone(), j(r#"{"n":0}"#))?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let frames = seen.clone();
    let _sub = source.subscribe(
        move |call, _| {
            hold(&frames).push(call);
            Ok(())
        },
        |_| {},
    );
    let ctx = Context::background();
    let log = Arc::new(Mutex::new(Vec::new()));
    let out = log.clone();
    let source2 = source.clone();
    let batch2 = batch.clone();
    batch.commit(&ctx, || {
        source.set(j(r#"{"n":1}"#), &ctx)?;
        source.set(j(r#"{"n":2}"#), &ctx)?;
        batch.flushing(move |ctx| {
            hold(&out).push("flush");
            let source = source2.clone();
            batch2.within(ctx, move || {
                source.set(j(r#"{"n":3}"#), &Context::background())
            })?;
            Ok(())
        });
        assert!(hold(&seen).is_empty());
        Ok(())
    })?;
    let frames = hold(&seen);
    assert_eq!(frames.len(), 2);
    assert!(
        matches!(&frames[0],WatchCall::Value{value,ops,..}if *value==j(r#"{"n":2}"#)&&*ops==vec![Op::Set(vec![Segment::Key("n".into())],j("2"))])
    );
    assert_eq!(*hold(&log), vec!["flush"]);
    Ok(())
}
#[tokio::test]
async fn derives_are_lazy_diamond_consistent_and_diff_once_from_prebatch_value() -> Result<()> {
    let batch = quiet();
    let source = Source::new(batch.clone(), j("0"))?;
    let count = Arc::new(AtomicUsize::new(0));
    let counted = count.clone();
    let left = Source::derive(
        batch.clone(),
        vec![source.clone()],
        move |v| {
            counted.fetch_add(1, Ordering::SeqCst);
            let JsonValue::Number(n) = v[0].to_json() else {
                return rev("null");
            };
            Ok(Revision::from_json(JsonValue::Number(n + 1.0))?)
        },
        |_| {},
    );
    let right = Source::derive(
        batch.clone(),
        vec![source.clone()],
        |v| {
            let JsonValue::Number(n) = v[0].to_json() else {
                return rev("null");
            };
            Ok(Revision::from_json(JsonValue::Number(n * 2.0))?)
        },
        |_| {},
    );
    let sum = Source::derive(
        batch.clone(),
        vec![left, right],
        |v| {
            let (JsonValue::Number(a), JsonValue::Number(b)) = (v[0].to_json(), v[1].to_json())
            else {
                return rev("null");
            };
            Ok(Revision::from_json(JsonValue::Number(a + b))?)
        },
        |_| {},
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(sum.value()?.to_json(), j("1"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let out = calls.clone();
    let _sub = sum.subscribe(
        move |c, _| {
            hold(&out).push(c);
            Ok(())
        },
        |_| {},
    );
    let ctx = Context::background();
    batch.commit(&ctx, || {
        source.set(j("1"), &ctx)?;
        assert_eq!(sum.value()?.to_json(), j("4"));
        source.set(j("2"), &ctx)?;
        Ok(())
    })?;
    assert_eq!(sum.value()?.to_json(), j("7"));
    assert_eq!(hold(&calls).len(), 1);
    assert_eq!(count.load(Ordering::SeqCst), 3);
    Ok(())
}
#[tokio::test]
async fn failed_derive_reports_once_heals_with_replace_and_retains_retirement() -> Result<()> {
    let batch = quiet();
    let source = Source::new(batch.clone(), j("0"))?;
    let failures = Arc::new(Mutex::new(Vec::new()));
    let errors = failures.clone();
    let derive = Source::derive(
        batch.clone(),
        vec![source.clone()],
        |v| {
            if v[0].to_json() == j("1") {
                Err(pi_rs_chord::Error::Invalid("broken".into()))
            } else {
                Ok(v[0].clone())
            }
        },
        move |e| hold(&errors).push(e),
    );
    let ctx = Context::background();
    let watch = derive.open_watch(&ctx)?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = seen.clone();
    watch.start(move |c, _| {
        let calls = calls.clone();
        async move {
            hold(&calls).push(c);
            Ok(())
        }
    })?;
    source.set(j("1"), &ctx)?;
    source.retire(&ctx)?;
    tokio::task::yield_now().await;
    assert!(derive.value().is_err());
    assert_eq!(hold(&failures).len(), 1);
    let late = derive.open_watch(&ctx)?;
    assert_eq!(late.value(), j("0"));
    source.set(j("2"), &ctx)?;
    tokio::task::yield_now().await;
    assert!(hold(&seen).iter().any(|c|matches!(c,WatchCall::Value{value,ops,retired}if *value==j("2")&&*ops==vec![Op::Replace(j("2"))]&&*retired)));
    derive.detach("released");
    assert!(derive.value().is_err());
    assert_eq!(watch.closed().await, pi_rs_chord::watch::WatchEnd::Ended);
    Ok(())
}
#[tokio::test]
async fn pure_compute_rejects_writes_and_restores_flags_after_error() -> Result<()> {
    let batch = quiet();
    let source = Source::new(batch.clone(), j("0"))?;
    let ctx = Context::background();
    let writable = source.clone();
    let attempted = Source::derive(
        batch.clone(),
        vec![source.clone()],
        move |_| {
            writable.set(j("9"), &Context::background())?;
            rev("1")
        },
        |_| {},
    );
    assert!(attempted.value().is_err());
    assert!(!batch.running());
    source.set(j("2"), &ctx)?;
    assert_eq!(source.value()?.to_json(), j("2"));
    Ok(())
}
#[tokio::test]
async fn native_identity_observation_cancellation_and_detach_preserve_last_readable_value()
-> Result<()> {
    let batch = quiet();
    let initial = Arc::new(vec![1]);
    let source = NativeSource::new(batch, initial.clone(), Arc::ptr_eq);
    let (ctx, cancel) = Context::background().with_cancel();
    let frames = Arc::new(AtomicUsize::new(0));
    let out = frames.clone();
    let ends = Arc::new(Mutex::new(Vec::new()));
    let end = ends.clone();
    let observation = source.observe(
        &ctx,
        move |_, _| {
            out.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        move |e| hold(&end).push(e),
    )?;
    source.set(initial.clone(), &ctx)?;
    assert_eq!(frames.load(Ordering::SeqCst), 0);
    source.set(Arc::new(vec![1]), &ctx)?;
    assert_eq!(frames.load(Ordering::SeqCst), 1);
    cancel.cancel("done");
    assert_eq!(*hold(&ends), vec![pi_rs_chord::watch::WatchEnd::Aborted]);
    source.set(Arc::new(vec![2]), &Context::background())?;
    assert_eq!(frames.load(Ordering::SeqCst), 1);
    observation.detach();
    source.detach("released");
    assert_eq!(*source.value(), vec![2]);
    assert!(source.set(initial, &ctx).is_err());
    Ok(())
}
#[tokio::test]
async fn batch_reports_failed_flush_and_continues_remaining_delivery() -> Result<()> {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let out = errors.clone();
    let batch = Batch::new(move |e| hold(&out).push(e));
    let completed = Arc::new(AtomicUsize::new(0));
    let done = completed.clone();
    batch.commit(&Context::background(), || {
        batch.flushing(|_| Err(pi_rs_chord::Error::Invalid("flush".into())));
        batch.flushing(move |_| {
            done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    })?;
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(matches!(
        hold(&errors).first(),
        Some(BatchError::DeliveryFailed(_))
    ));
    Ok(())
}

#[tokio::test]
async fn native_projection_is_lazy_and_soon_coalesces_one_frame() -> Result<()> {
    let batch = quiet();
    let source = NativeSource::new(batch.clone(), 0, |a, b| a == b);
    let calls = Arc::new(AtomicUsize::new(0));
    let out = calls.clone();
    let projected = source.project(
        move |n| {
            out.fetch_add(1, Ordering::SeqCst);
            Ok(Revision::from_json(JsonValue::Number(f64::from(n)))?)
        },
        |_| {},
    )?;
    source.set(1, &Context::background())?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(projected.value()?.to_json(), j("1"));
    let frames = Arc::new(AtomicUsize::new(0));
    let count = frames.clone();
    let _sub = projected.subscribe(
        move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        |_| {},
    );
    let (done, wait) = tokio::sync::oneshot::channel();
    for n in [2, 3] {
        let source = source.clone();
        batch.soon(Context::background(), move || {
            source.set(n, &Context::background())
        })?;
    }
    batch.soon(Context::background(), move || {
        let _ = done.send(());
        Ok(())
    })?;
    wait.await.map_err(|_| pi_rs_chord::Error::Closed)?;
    assert_eq!(projected.value()?.to_json(), j("3"));
    assert_eq!(frames.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn map_shared_reuses_container_identity_but_never_caches_primitives() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let map = pi_rs_chord::state::map_shared(move |_| {
        let n = counted.fetch_add(1, Ordering::SeqCst);
        Ok(Revision::from_json(JsonValue::Array(vec![
            JsonValue::Number(n as f64),
        ]))?)
    });
    let a = rev(r#"{"n":1}"#)?;
    let b = rev(r#"{"n":1}"#)?;
    let primitive = rev("1")?;
    let first = map.map(&[a.clone(), b.clone(), primitive.clone()])?;
    let second = map.map(&[b, a, primitive])?;
    assert!(first[0].ptr_eq(&second[1]));
    assert!(first[1].ptr_eq(&second[0]));
    assert!(!first[2].ptr_eq(&second[2]));
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    Ok(())
}
