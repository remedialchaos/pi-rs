//! Host-supplied timers with a native fallback for work that must resume.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::errors::BoxError;

const MAX_TIMER_DELAY: Duration = Duration::from_millis(2_147_483_647);

pub type TimerCallback = Box<dyn FnOnce() + Send + 'static>;
pub type SetTimer = Arc<
    dyn Fn(TimerCallback, Duration, TimerOptions) -> Result<Option<TimerHandle>, BoxError>
        + Send
        + Sync,
>;
pub type TimerReporter = Arc<dyn Fn(BoxError) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerOptions {
    pub background: bool,
}

#[must_use = "retain the handle if the timer may need to be disarmed"]
pub struct TimerHandle(TimerCallback);

impl TimerHandle {
    pub fn new(disarm: TimerCallback) -> Self {
        Self(disarm)
    }
    pub fn disarm(self) {
        (self.0)()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TimerError {
    #[error("no Tokio runtime available for a native timer")]
    NoRuntime(#[source] tokio::runtime::TryCurrentError),
    #[error("HarnessOptions.setTimer declined a timer that paces work; using setTimeout")]
    Declined,
    #[error("HarnessOptions.setTimer panicked: {0}")]
    HostPanicked(String),
}

pub fn native_timer(
    callback: TimerCallback,
    delay: Duration,
    _options: TimerOptions,
) -> Result<TimerHandle, TimerError> {
    let runtime = tokio::runtime::Handle::try_current().map_err(TimerError::NoRuntime)?;
    let first = tokio::time::Instant::now() + delay.min(MAX_TIMER_DELAY);
    let job = runtime.spawn(async move {
        tokio::time::sleep_until(first).await;
        let mut remaining = delay.saturating_sub(MAX_TIMER_DELAY);
        while !remaining.is_zero() {
            let step = remaining.min(MAX_TIMER_DELAY);
            tokio::time::sleep(step).await;
            remaining -= step;
        }
        callback();
    });
    Ok(TimerHandle::new(Box::new(move || job.abort())))
}

#[derive(Clone, Default)]
pub struct Timers {
    host: Option<SetTimer>,
    report: Option<TimerReporter>,
    reported_fallback: Arc<AtomicBool>,
}

pub fn host_timers(set_timer: Option<SetTimer>, report: TimerReporter) -> Timers {
    Timers {
        host: set_timer,
        report: Some(report),
        reported_fallback: Arc::default(),
    }
}

fn arm_host(
    host: &SetTimer,
    callback: TimerCallback,
    delay: Duration,
    options: TimerOptions,
) -> Result<Option<TimerHandle>, BoxError> {
    catch_unwind(AssertUnwindSafe(|| host(callback, delay, options))).map_err(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        Box::new(TimerError::HostPanicked(message.to_owned())) as BoxError
    })?
}

impl Timers {
    pub fn foreground(
        &self,
        callback: TimerCallback,
        delay: Duration,
    ) -> Result<TimerHandle, TimerError> {
        let options = TimerOptions { background: false };
        let Some(host) = &self.host else {
            return native_timer(callback, delay, options);
        };
        // The host owns the first arm; keep exactly one callable copy if it declines.
        let callback = Arc::new(Mutex::new(Some(callback)));
        let hosted = Arc::clone(&callback);
        let call = |callback: &Mutex<Option<TimerCallback>>| {
            let callback = callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(callback) = callback {
                callback();
            }
        };
        let error = match arm_host(host, Box::new(move || call(&hosted)), delay, options) {
            Ok(Some(handle)) => return Ok(handle),
            Ok(None) => Box::new(TimerError::Declined) as BoxError,
            Err(error) => error,
        };
        if !self.reported_fallback.swap(true, Ordering::Relaxed)
            && let Some(report) = &self.report
        {
            report(error);
        }
        native_timer(Box::new(move || call(&callback)), delay, options)
    }

    pub fn background(
        &self,
        callback: TimerCallback,
        delay: Duration,
    ) -> Result<Option<TimerHandle>, TimerError> {
        let options = TimerOptions { background: true };
        let Some(host) = &self.host else {
            return native_timer(callback, delay, options).map(Some);
        };
        match arm_host(host, callback, delay, options) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if let Some(report) = &self.report {
                    report(error);
                }
                Ok(None)
            }
        }
    }
}
