//! `AbortSignal`/`AbortController`: the ambient Web API the upstream TypeScript runs against,
//! replaced here by a runtime-agnostic shared flag every holder can poll.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

/// The error message a bare `abort()` carries as its reason, like `String(new DOMException(...))`.
const DEFAULT_ABORT_REASON: &str = "AbortError: This operation was aborted";

#[derive(Default)]
struct SignalState {
    aborted: AtomicBool,
    reason: Mutex<Option<String>>,
    notify: Notify,
}

/// A signal that is set once, by its [`AbortController`] or by whoever holds a clone.
#[derive(Clone, Default)]
pub struct AbortSignal(Arc<SignalState>);

impl std::fmt::Debug for AbortSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AbortSignal")
            .field("aborted", &self.aborted())
            .field("reason", &self.reason())
            .finish()
    }
}

impl AbortSignal {
    /// Whether the signal fired.
    pub fn aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    /// The reason the signal was aborted with; `None` until it fires. A bare `abort()` carries
    /// the message of the DOMException it would throw upstream.
    pub fn reason(&self) -> Option<String> {
        if !self.aborted() {
            return None;
        }
        let stored = self.0.reason.lock().ok().and_then(|reason| reason.clone());
        stored.or_else(|| Some(DEFAULT_ABORT_REASON.to_string()))
    }

    /// Resolves once the signal fires, with its reason. Returns at once when it already has.
    /// Consumes a clone of the signal so the returned future owns everything it borrows and is
    /// `'static`, like the promises the Web `AbortSignal` listeners upstream create.
    pub async fn wait_aborted(self) -> String {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking, so an abort between the check and the wait is not missed.
            notified.as_mut().enable();
            if let Some(reason) = self.reason() {
                return reason;
            }
            notified.await;
        }
    }

    /// Fire the signal with an explicit reason, or with the default one upstream's bare
    /// `controller.abort()` uses.
    pub fn abort_with(&self, reason: Option<&str>) {
        if self.0.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        *self
            .0
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(reason.unwrap_or(DEFAULT_ABORT_REASON).to_string());
        self.0.notify.notify_waiters();
    }
}

/// Creates and fires [`AbortSignal`]s, like the Web `AbortController`.
#[derive(Debug, Default)]
pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal this controller fires.
    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Fire the signal. `reason` reaches the cancellation notification, like the Web overload
    /// taking one.
    pub fn abort(&self, reason: Option<&str>) {
        self.signal.abort_with(reason);
    }
}

/// A signal that fires when the first of `signals` does, like `AbortSignal.any`.
pub fn abort_any(signals: Vec<AbortSignal>) -> AbortSignal {
    let out = AbortSignal::default();
    for signal in signals {
        let out = out.clone();
        tokio::spawn(async move {
            let reason = signal.wait_aborted().await;
            out.abort_with(Some(&reason));
        });
    }
    out
}

/// A signal that fires after `ms`, like `AbortSignal.timeout`.
pub fn abort_timeout(ms: u64) -> AbortSignal {
    let out = AbortSignal::default();
    let timer = out.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        timer.abort_with(None);
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fires_once_with_its_reason() {
        let controller = AbortController::new();
        let signal = controller.signal();
        assert!(!signal.aborted());
        assert_eq!(signal.reason(), None);
        controller.abort(Some("stop"));
        controller.abort(Some("later"));
        assert!(signal.aborted());
        assert_eq!(signal.reason(), Some("stop".to_string()));
        assert_eq!(signal.wait_aborted().await, "stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn any_fires_with_the_first_reason() {
        let first = AbortController::new();
        let second = AbortController::new();
        let merged = abort_any(vec![first.signal(), second.signal()]);
        first.abort(Some("first"));
        assert_eq!(merged.wait_aborted().await, "first");
    }
}
