//! Abort helpers: the Rust port of `packages/coding-agent/src/utils/abort.ts`.

use std::sync::Arc;

use tokio::sync::watch;

/// A cloneable cancellation signal, like a Web `AbortSignal`'s useful part.
#[derive(Clone, Debug)]
pub struct AbortSignal {
    receiver: watch::Receiver<bool>,
    reason: Arc<std::sync::Mutex<Option<String>>>,
}

impl AbortSignal {
    /// Whether the signal has fired.
    pub fn is_aborted(&self) -> bool {
        *self.receiver.borrow()
    }

    /// The reason passed to [`AbortController::abort`], or `None` before it fires.
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|guard| guard.clone())
    }

    /// Resolves when the signal fires, with its reason.
    pub async fn wait_aborted(&self) -> String {
        let mut receiver = self.receiver.clone();
        if *receiver.borrow() {
            return self.abort_reason();
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow() {
                return self.abort_reason();
            }
        }
        self.abort_reason()
    }

    fn abort_reason(&self) -> String {
        self.reason()
            .unwrap_or_else(|| "The operation was aborted".to_string())
    }
}

/// Fires an [`AbortSignal`], like the Web `AbortController`.
#[derive(Debug)]
pub struct AbortController {
    sender: watch::Sender<bool>,
    signal: AbortSignal,
}

impl Default for AbortController {
    fn default() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            sender,
            signal: AbortSignal {
                receiver,
                reason: Arc::new(std::sync::Mutex::new(None)),
            },
        }
    }
}

impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Fires the signal, optionally with a reason.
    pub fn abort(&self, reason: Option<&str>) {
        if let Ok(mut stored) = self.signal.reason.lock() {
            *stored = reason.map(str::to_string);
        }
        let _ = self.sender.send(true);
    }
}

/// Returns `signal`, or a never-firing signal when none was supplied.
pub fn operation_signal(signal: Option<AbortSignal>) -> AbortSignal {
    signal.unwrap_or_else(|| AbortController::new().signal())
}

/// Races a future against an abort signal, returning `"Aborted"` when the signal wins.
pub async fn race_with_abort<T>(
    operation: impl std::future::Future<Output = T>,
    signal: Option<AbortSignal>,
) -> Result<T, String> {
    match signal {
        None => Ok(operation.await),
        Some(signal) if signal.is_aborted() => Err(signal.abort_reason()),
        Some(signal) => {
            tokio::select! {
                result = operation => Ok(result),
                reason = signal.wait_aborted() => Err(reason),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fires_once_with_its_reason() {
        let controller = AbortController::new();
        let signal = controller.signal();
        controller.abort(Some("stop"));
        assert!(signal.is_aborted());
        assert_eq!(signal.wait_aborted().await, "stop");
    }

    #[tokio::test]
    async fn races_an_operation_against_abort() {
        let controller = AbortController::new();
        let signal = controller.signal();
        controller.abort(None);
        let result = race_with_abort(async { 42 }, Some(signal)).await;
        assert_eq!(result.unwrap_err(), "The operation was aborted");
    }
}
