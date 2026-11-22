//! Explicit immutable invocation context, typed values and cancellation.
use pi_rs_telemetry::{SpanOptions, TelemetryContext};
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => error.into_inner(),
    }
}
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
pub struct ContextKey<T> {
    id: u64,
    description: String,
    marker: PhantomData<fn(T) -> T>,
}
impl<T> Clone for ContextKey<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            description: self.description.clone(),
            marker: PhantomData,
        }
    }
}
impl<T> ContextKey<T> {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            description: description.into(),
            marker: PhantomData,
        }
    }
}

type AbortListener = Box<dyn FnOnce(String) + Send + 'static>;
struct AbortState {
    reason: Option<String>,
    listeners: HashMap<u64, AbortListener>,
    next: u64,
}
struct AbortInner {
    state: Mutex<AbortState>,
    changed: tokio::sync::watch::Sender<Option<String>>,
    parents: Mutex<Vec<AbortSubscription>>,
}
#[derive(Clone)]
pub struct AbortSignal(Arc<AbortInner>);
#[derive(Clone)]
pub struct Cancel(AbortSignal);
pub struct AbortSubscription {
    inner: Arc<AbortInner>,
    id: u64,
}
impl Drop for AbortSubscription {
    fn drop(&mut self) {
        lock(&self.inner.state).listeners.remove(&self.id);
    }
}
impl AbortSignal {
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn new() -> (Self, Cancel) {
        let (changed, _) = tokio::sync::watch::channel(None);
        let signal = Self(Arc::new(AbortInner {
            state: Mutex::new(AbortState {
                reason: None,
                listeners: HashMap::new(),
                next: 0,
            }),
            changed,
            parents: Mutex::new(Vec::new()),
        }));
        (signal.clone(), Cancel(signal))
    }
    pub fn aborted(&self) -> bool {
        lock(&self.0.state).reason.is_some()
    }
    pub fn reason(&self) -> Option<String> {
        lock(&self.0.state).reason.clone()
    }
    pub fn subscribe(&self, listener: impl FnOnce(String) + Send + 'static) -> AbortSubscription {
        let mut state = lock(&self.0.state);
        let id = state.next;
        state.next += 1;
        if let Some(reason) = state.reason.clone() {
            drop(state);
            listener(reason);
        } else {
            state.listeners.insert(id, Box::new(listener));
        }
        AbortSubscription {
            inner: self.0.clone(),
            id,
        }
    }
    pub async fn cancelled(&self) -> String {
        let mut changes = self.0.changed.subscribe();
        loop {
            if let Some(reason) = changes.borrow_and_update().clone() {
                return reason;
            }
            if changes.changed().await.is_err() {
                return "The operation was aborted".into();
            }
        }
    }
    pub fn any(signals: impl IntoIterator<Item = AbortSignal>) -> Self {
        let (combined, _) = Self::new();
        for signal in signals {
            let weak = Arc::downgrade(&combined.0);
            let subscription = signal.subscribe(move |reason| {
                if let Some(inner) = weak.upgrade() {
                    Cancel(AbortSignal(inner)).cancel(reason);
                }
            });
            // A subscription keeps its parent signal alive until this combined signal is dropped.
            lock(&combined.0.parents).push(subscription);
        }
        combined
    }
}
impl Cancel {
    pub fn cancel(&self, reason: impl Into<String>) {
        let mut state = lock(&self.0.0.state);
        if state.reason.is_some() {
            return;
        }
        let reason = reason.into();
        state.reason = Some(reason.clone());
        let listeners = std::mem::take(&mut state.listeners);
        self.0.0.changed.send_replace(Some(reason.clone()));
        drop(state);
        for listener in listeners.into_values() {
            listener(reason.clone());
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AbortError(pub String);
#[derive(Clone)]
pub struct Context {
    values: Arc<HashMap<(u64, TypeId), Arc<dyn Any + Send + Sync>>>,
    name: Arc<str>,
    signal: Option<AbortSignal>,
    telemetry: pi_rs_telemetry::Context,
}
impl Default for Context {
    fn default() -> Self {
        Self::background()
    }
}
impl Context {
    pub fn background() -> Self {
        Self::root("[Context BACKGROUND_CONTEXT]")
    }
    pub fn todo() -> Self {
        Self::root("[Context TODO_CONTEXT]")
    }
    fn root(name: &str) -> Self {
        Self {
            values: Arc::new(HashMap::new()),
            name: Arc::from(name),
            signal: None,
            telemetry: pi_rs_telemetry::NOOP_TELEMETRY_CONTEXT,
        }
    }
    pub fn with_value<T: Any + Send + Sync>(&self, key: &ContextKey<T>, value: T) -> Self {
        let mut next = self.clone();
        let mut values = (*self.values).clone();
        values.insert((key.id, TypeId::of::<T>()), Arc::new(value));
        next.values = Arc::new(values);
        next.name = Arc::from(format!("{}.WithValue({})", self.name, key.description));
        next
    }
    pub fn value<T: Any + Clone + Send + Sync>(&self, key: &ContextKey<T>) -> Option<T> {
        self.values
            .get(&(key.id, TypeId::of::<T>()))
            .and_then(|value| value.downcast_ref::<T>())
            .cloned()
    }
    pub fn with_telemetry(&self, telemetry: pi_rs_telemetry::Context) -> Self {
        let mut next = self.clone();
        next.telemetry = telemetry;
        next
    }
    pub fn telemetry(&self) -> &pi_rs_telemetry::Context {
        &self.telemetry
    }
    pub fn with_abort_signal(&self, signal: AbortSignal) -> Self {
        let mut next = self.clone();
        next.signal = Some(match &self.signal {
            Some(parent) => AbortSignal::any([parent.clone(), signal]),
            None => signal,
        });
        next
    }
    pub fn without_abort_signal(&self) -> Self {
        let mut next = self.clone();
        next.signal = None;
        next
    }
    pub fn with_cancel(&self) -> (Self, Cancel) {
        let (signal, cancel) = AbortSignal::new();
        (self.with_abort_signal(signal), cancel)
    }
    pub fn abort_signal(&self) -> Option<AbortSignal> {
        self.signal.clone()
    }
    pub fn is_aborted(&self) -> bool {
        self.signal.as_ref().is_some_and(AbortSignal::aborted)
    }
    pub fn check(&self) -> std::result::Result<(), AbortError> {
        match self.signal.as_ref().and_then(AbortSignal::reason) {
            Some(reason) => Err(AbortError(reason)),
            None => Ok(()),
        }
    }
    /// Only this waiter is cancelled. Spawn underlying work separately when it must outlive the waiter.
    pub async fn await_with<F: Future>(
        &self,
        work: F,
    ) -> std::result::Result<F::Output, AbortError> {
        match &self.signal {
            None => Ok(work.await),
            Some(signal) => {
                self.check()?;
                tokio::select! {biased;reason=signal.cancelled()=>Err(AbortError(reason)),value=work=>Ok(value)}
            }
        }
    }
    pub fn trace_in<T, E: Any, F, Fut>(
        &self,
        options: SpanOptions,
        callback: F,
    ) -> impl Future<Output = std::result::Result<T, E>>
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = std::result::Result<T, E>>,
    {
        let parent = self.clone();
        self.telemetry
            .start_span(options, move |span| callback(parent.with_telemetry(span)))
    }
}
impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}
