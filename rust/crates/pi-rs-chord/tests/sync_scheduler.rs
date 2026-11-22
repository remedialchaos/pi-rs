//! Deterministic async retry lifecycle, matching sync/client.test.ts scheduler contracts.
use pi_rs_chord::{
    Error, Result,
    sync::{FlushOptions, FlushScheduler},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
async fn settle() {
    for _ in 0..12 {
        tokio::task::yield_now().await;
    }
}
#[tokio::test(start_paused = true)]
async fn urgency_keeps_earliest_deadline_and_failed_flush_retries_without_new_commits() -> Result<()>
{
    let calls = Arc::new(AtomicUsize::new(0));
    let flushed = calls.clone();
    let failures = Arc::new(AtomicUsize::new(0));
    let errors = failures.clone();
    let scheduler = FlushScheduler::new(
        FlushOptions::default(),
        move || {
            let n = flushed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { if n < 2 { Err(Error::Closed) } else { Ok(()) } })
        },
        move |_| {
            errors.fetch_add(1, Ordering::SeqCst);
        },
    );
    scheduler.notify(false);
    settle().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    scheduler.notify(true);
    settle().await;
    tokio::time::advance(Duration::from_millis(199)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(999)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(1999)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(failures.load(Ordering::SeqCst), 2);
    scheduler.close().await?;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    scheduler.notify(true);
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn pending_urgency_survives_running_flush_and_close_cancels_pending_timer() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let flushed = calls.clone();
    let (release, held) = tokio::sync::oneshot::channel();
    let held = Arc::new(Mutex::new(Some(held)));
    let scheduler = FlushScheduler::new(
        FlushOptions::default(),
        move || {
            let n = flushed.fetch_add(1, Ordering::SeqCst);
            let held = held.lock().unwrap_or_else(|e| e.into_inner()).take();
            Box::pin(async move {
                if n == 0
                    && let Some(held) = held
                {
                    held.await.map_err(|_| Error::Closed)?;
                }
                Ok(())
            })
        },
        |_| {},
    );
    scheduler.notify(true);
    settle().await;
    tokio::time::advance(Duration::from_millis(200)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    scheduler.notify(false);
    scheduler.notify(true);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).map_err(|_| Error::Closed)?;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    scheduler.notify(false);
    scheduler.close().await?;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    Ok(())
}
