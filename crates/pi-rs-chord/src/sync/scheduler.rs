//! One-at-a-time flush scheduling with urgency retention and bounded exponential retries.
use super::SyncFuture;
use crate::{Result, context::lock};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};
#[derive(Clone, Debug)]
pub struct FlushOptions {
    pub interval: Duration,
    pub urgent: Duration,
    pub retry: Duration,
    pub max_retry: Duration,
}
impl Default for FlushOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            urgent: Duration::from_millis(200),
            retry: Duration::from_secs(1),
            max_retry: Duration::from_secs(60),
        }
    }
}
struct State {
    due: Option<Instant>,
    pending: Option<Instant>,
    running: bool,
    failures: u32,
    closed: bool,
}
struct Inner {
    options: FlushOptions,
    flush: Arc<dyn Fn() -> SyncFuture<()> + Send + Sync>,
    report: Arc<dyn Fn(crate::Error) + Send + Sync>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    finished: Notify,
}
#[derive(Clone)]
pub struct FlushScheduler(Arc<Inner>);
impl FlushScheduler {
    pub fn new(
        options: FlushOptions,
        flush: impl Fn() -> SyncFuture<()> + Send + Sync + 'static,
        report: impl Fn(crate::Error) + Send + Sync + 'static,
    ) -> Self {
        let (changed, mut signal) = watch::channel(0);
        let scheduler = Self(Arc::new(Inner {
            options,
            flush: Arc::new(flush),
            report: Arc::new(report),
            state: Mutex::new(State {
                due: None,
                pending: None,
                running: false,
                failures: 0,
                closed: false,
            }),
            changed,
            finished: Notify::new(),
        }));
        let weak = Arc::downgrade(&scheduler.0);
        tokio::spawn(async move {
            loop {
                let due = {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let state = lock(&inner.state);
                    if state.closed {
                        return;
                    }
                    state.due
                };
                match due {
                    None => {
                        if signal.changed().await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Some(due) => {
                        tokio::select! {biased;changed=signal.changed()=>{if changed.is_err(){return;}continue;},_=tokio::time::sleep_until(due)=>{}}
                    }
                }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                {
                    let mut state = lock(&inner.state);
                    if state.closed {
                        return;
                    }
                    if state.due.is_none_or(|due| due > Instant::now()) {
                        continue;
                    }
                    state.due = None;
                    state.running = true;
                }
                let result = (inner.flush)().await;
                let retry = match result {
                    Ok(()) => {
                        lock(&inner.state).failures = 0;
                        None
                    }
                    Err(error) => {
                        (inner.report)(error);
                        let mut state = lock(&inner.state);
                        let delay = inner
                            .options
                            .retry
                            .saturating_mul(1u32.checked_shl(state.failures).unwrap_or(u32::MAX))
                            .min(inner.options.max_retry);
                        state.failures = state.failures.saturating_add(1);
                        Some(Instant::now() + delay)
                    }
                };
                {
                    let mut state = lock(&inner.state);
                    state.running = false;
                    let pending = state.pending.take();
                    state.due = if state.closed {
                        None
                    } else {
                        retry.or(pending)
                    };
                }
                inner.finished.notify_waiters();
            }
        });
        scheduler
    }
    pub fn notify(&self, urgent: bool) {
        let mut state = lock(&self.0.state);
        if state.closed {
            return;
        }
        let due = Instant::now()
            + if urgent {
                self.0.options.urgent
            } else {
                self.0.options.interval
            };
        let slot = if state.running {
            &mut state.pending
        } else {
            &mut state.due
        };
        *slot = Some(slot.map_or(due, |old| old.min(due)));
        drop(state);
        self.0.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    /// Stops timers, waits for admitted work, then makes one final fallible flush.
    pub async fn close(&self) -> Result<()> {
        {
            let mut state = lock(&self.0.state);
            state.closed = true;
            state.due = None;
            state.pending = None;
        }
        self.0.changed.send_modify(|n| *n = n.wrapping_add(1));
        loop {
            let finished = self.0.finished.notified();
            if !lock(&self.0.state).running {
                break;
            }
            finished.await;
        }
        (self.0.flush)().await
    }
}
