//! Sleep helpers, port of `packages/coding-agent/src/utils/sleep.ts`.
//!
//! Upstream rejects with `Error("Aborted")` when the signal is already aborted,
//! and aborts a pending timeout through an abort listener. Here a
//! `tokio::sync::watch::Receiver<bool>` plays the role of the signal: a value of
//! `true` means aborted.

use std::time::Duration;
use tokio::sync::watch;

/// Sleep for `ms` milliseconds.
pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Wait until the watch channel reports `true`.
async fn wait_for_abort(mut cancel: watch::Receiver<bool>) {
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            break;
        }
    }
}

/// Sleep for `ms` milliseconds, or return `Err("Aborted")` if `cancel` reports an
/// abort before the timer fires. A missing receiver behaves like an absent signal.
///
/// Upstream treats negative and `NaN` values as zero; Rust's `u64` cannot express
/// those, so callers should clamp before converting.
pub async fn sleep_cancellable(
    ms: u64,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<(), String> {
    if let Some(receiver) = &cancel
        && *receiver.borrow()
    {
        return Err("Aborted".to_string());
    }

    let timer = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(timer);

    match cancel {
        None => {
            timer.await;
            Ok(())
        }
        Some(receiver) => {
            tokio::select! {
                _ = &mut timer => Ok(()),
                _ = wait_for_abort(receiver) => Err("Aborted".to_string()),
            }
        }
    }
}

/// Backwards-compatible name matching the upstream `sleep(ms, signal)` export.
pub async fn sleep(ms: u64, cancel: Option<watch::Receiver<bool>>) -> Result<(), String> {
    sleep_cancellable(ms, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completes_when_not_cancelled() {
        sleep_ms(1).await;
        assert_eq!(sleep_cancellable(1, None).await, Ok(()));
    }

    #[tokio::test]
    async fn returns_aborted_for_already_cancelled_signal() {
        let (_tx, rx) = watch::channel(true);
        assert_eq!(
            sleep_cancellable(1000, Some(rx)).await,
            Err("Aborted".to_string())
        );
    }

    #[tokio::test]
    async fn aborts_pending_sleep() {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move { sleep_cancellable(60_000, Some(rx)).await });
        tx.send(true).expect("receiver must be alive");
        assert_eq!(
            handle.await.expect("task must join"),
            Err("Aborted".to_string())
        );
    }
}
