//! Serialized watches with bounded pending frames and explicit terminal outcomes.
use crate::{
    context::{AbortSignal, AbortSubscription, Context, lock},
    delta::{Op, PendingOps, PendingTake},
    json::JsonValue,
};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEnd {
    Stopped,
    Aborted,
    Ended,
    ListenerError(String),
    Failed(String),
}
#[derive(Clone, Debug, PartialEq)]
pub enum WatchCall<T = JsonValue> {
    Value {
        value: T,
        ops: Vec<Op>,
        retired: bool,
    },
    Failed {
        error: String,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum NativeWatchCall<T> {
    Value { value: T },
    Failed { error: String },
}
type Callback<T> = dyn Fn(
        WatchCall<T>,
        Context,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>
    + Send
    + Sync;
type JsonOf<T> = dyn Fn(&T) -> JsonValue + Send + Sync;
struct State<T> {
    value: T,
    pending: VecDeque<(WatchCall<T>, Context)>,
    ops: PendingOps,
    folded: bool,
    replace: bool,
    retired: bool,
    listener: Option<Arc<Callback<T>>>,
    detach: Option<Box<dyn FnOnce() + Send>>,
    report: Option<Arc<dyn Fn(String) + Send + Sync>>,
    started: bool,
    running: bool,
    ending: bool,
    end: Option<WatchEnd>,
    abort: Option<AbortSubscription>,
    abort_installed: bool,
}
struct Core<T> {
    state: Mutex<State<T>>,
    closed: tokio::sync::watch::Sender<Option<WatchEnd>>,
    json: Option<Arc<JsonOf<T>>>,
    native_replace: Mutex<Option<Arc<dyn Fn() -> T + Send + Sync>>>,
}
impl<T: Clone + Send + 'static> Core<T> {
    fn new(
        value: T,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(String) + Send + Sync + 'static,
        json: Option<Arc<JsonOf<T>>>,
    ) -> Arc<Self> {
        let (closed, _) = tokio::sync::watch::channel(None);
        Arc::new(Self {
            state: Mutex::new(State {
                value,
                pending: VecDeque::new(),
                ops: PendingOps::default(),
                folded: false,
                replace: false,
                retired: false,
                listener: None,
                detach: Some(Box::new(detach)),
                report: Some(Arc::new(report)),
                started: false,
                running: false,
                ending: false,
                end: None,
                abort: None,
                abort_installed: false,
            }),
            closed,
            json,
            native_replace: Mutex::new(None),
        })
    }
    fn terminate(&self, end: WatchEnd) -> WatchEnd {
        let mut state = lock(&self.state);
        if let Some(end) = &state.end {
            return end.clone();
        }
        state.end = Some(end.clone());
        state.listener = None;
        state.report = None;
        state.pending.clear();
        state.ops.take();
        state.retired = false;
        state.replace = false;
        let detach = state.detach.take();
        let abort = state.abort.take();
        drop(state);
        lock(&self.native_replace).take();
        drop(abort);
        if let Some(detach) = detach {
            detach();
        }
        self.closed.send_replace(Some(end.clone()));
        end
    }
    fn advance(self: &Arc<Self>, call: WatchCall<T>, context: Context) {
        let mut state = lock(&self.state);
        if state.end.is_some() || state.ending {
            return;
        }
        match &call {
            WatchCall::Failed { .. } => {
                state.pending.clear();
                state.ops.take();
                state.folded = false;
                state.replace = self.json.is_some();
            }
            WatchCall::Value { ops, retired, .. } if self.json.is_some() => {
                state.retired |= *retired;
                if !state.replace {
                    state.ops.push(ops);
                    state.folded = !state.pending.is_empty();
                }
                state.pending.clear();
            }
            WatchCall::Value { .. } => {
                if matches!(state.pending.front(), Some((WatchCall::Failed { .. }, _))) {
                    state.pending.clear();
                }
                if state.pending.len() >= 100 {
                    state.pending.clear();
                    let replacement = lock(&self.native_replace).clone();
                    drop(state);
                    let call = match replacement {
                        Some(replace) => WatchCall::Value {
                            value: replace(),
                            ops: Vec::new(),
                            retired: false,
                        },
                        None => call,
                    };
                    let mut state = lock(&self.state);
                    if state.end.is_none() && !state.ending {
                        state.pending.push_back((call, context));
                    }
                    drop(state);
                    self.schedule();
                    return;
                }
            }
        }
        state.pending.push_back((call, context));
        drop(state);
        self.schedule();
    }
    fn schedule(self: &Arc<Self>) {
        let mut state = lock(&self.state);
        if state.running || !state.started || state.end.is_some() {
            return;
        }
        state.running = true;
        drop(state);
        let core = self.clone();
        tokio::spawn(async move {
            core.drain().await;
        });
    }
    async fn drain(self: Arc<Self>) {
        loop {
            let delivery = {
                let mut state = lock(&self.state);
                if state.end.is_some() {
                    state.running = false;
                    return;
                }
                let Some((mut call, context)) = state.pending.pop_front() else {
                    state.running = false;
                    let ending = state.ending;
                    drop(state);
                    if ending {
                        self.terminate(WatchEnd::Ended);
                    }
                    return;
                };
                if let WatchCall::Value {
                    value,
                    ops,
                    retired,
                } = &mut call
                {
                    if let Some(json) = &self.json {
                        let batch = state.ops.take();
                        if state.replace {
                            *ops = vec![Op::Replace(json(value))];
                            state.replace = false;
                        } else if state.folded {
                            *ops = match batch {
                                PendingTake::Ops(ops) => ops,
                                PendingTake::Replace => vec![Op::Replace(json(value))],
                            };
                        }
                        *retired |= state.retired;
                        state.retired = false;
                        state.folded = false;
                    }
                    state.value = value.clone();
                }
                state
                    .listener
                    .clone()
                    .map(|listener| (listener, call, context.without_abort_signal()))
            };
            let Some((listener, call, context)) = delivery else {
                lock(&self.state).running = false;
                return;
            };
            if let Err(error) = listener(call, context).await {
                let report = {
                    let state = lock(&self.state);
                    if state.end.is_some() {
                        None
                    } else {
                        state.report.clone()
                    }
                };
                if let Some(report) = report {
                    report(error.clone());
                    self.terminate(WatchEnd::ListenerError(error));
                }
                lock(&self.state).running = false;
                return;
            }
        }
    }
    fn start<F, Fut>(self: &Arc<Self>, listener: F) -> crate::Result<()>
    where
        F: Fn(WatchCall<T>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        let mut state = lock(&self.state);
        if state.started {
            return Err(crate::Error::Invalid("Watch is already started".into()));
        }
        if state.end.is_some() {
            return Err(crate::Error::Invalid("Watch is stopped".into()));
        }
        state.started = true;
        state.listener = Some(Arc::new(move |call, context| {
            Box::pin(listener(call, context))
        }));
        drop(state);
        self.schedule();
        Ok(())
    }
    fn observe_abort(self: &Arc<Self>, signal: AbortSignal) -> crate::Result<()> {
        let mut state = lock(&self.state);
        if state.abort_installed {
            return Err(crate::Error::Invalid(
                "Watch abort is already installed".into(),
            ));
        }
        if state.end.is_some() {
            return Ok(());
        }
        state.abort_installed = true;
        drop(state);
        let weak = Arc::downgrade(self);
        let subscription = signal.subscribe(move |_| {
            if let Some(core) = weak.upgrade() {
                core.terminate(WatchEnd::Aborted);
            }
        });
        let mut state = lock(&self.state);
        if state.end.is_none() {
            state.abort = Some(subscription);
        }
        Ok(())
    }
    fn end_after_delivery(&self) {
        let mut state = lock(&self.state);
        if state.end.is_some() || state.ending {
            return;
        }
        state.ending = true;
        let end = !state.running && state.pending.is_empty();
        drop(state);
        if end {
            self.terminate(WatchEnd::Ended);
        }
    }
    async fn closed(&self) -> WatchEnd {
        let mut closed = self.closed.subscribe();
        loop {
            if let Some(end) = closed.borrow_and_update().clone() {
                return end;
            }
            if closed.changed().await.is_err() {
                return WatchEnd::Ended;
            }
        }
    }
}
#[derive(Clone)]
pub struct Watch(Arc<Core<JsonValue>>);
impl Watch {
    pub fn new(
        value: JsonValue,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self(Core::new(
            value,
            detach,
            report,
            Some(Arc::new(Clone::clone)),
        ))
    }
    pub fn value(&self) -> JsonValue {
        lock(&self.0.state).value.clone()
    }
    pub fn advance(&self, call: WatchCall, context: Context) {
        self.0.advance(call, context);
    }
    pub fn fail(&self, error: impl Into<String>, context: Context) {
        self.advance(
            WatchCall::Failed {
                error: error.into(),
            },
            context,
        );
    }
    pub fn start<F, Fut>(&self, listener: F) -> crate::Result<()>
    where
        F: Fn(WatchCall, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        self.0.start(listener)
    }
    pub fn stop(&self) -> WatchEnd {
        self.0.terminate(WatchEnd::Stopped)
    }
    pub fn end(&self, end: WatchEnd) -> WatchEnd {
        self.0.terminate(end)
    }
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }
    pub fn observe_abort(&self, signal: AbortSignal) -> crate::Result<()> {
        self.0.observe_abort(signal)
    }
    pub async fn closed(&self) -> WatchEnd {
        self.0.closed().await
    }
}
#[derive(Clone)]
pub struct NativeWatch<T>(Arc<Core<T>>);
impl<T: Clone + Send + 'static> NativeWatch<T> {
    pub fn new(
        value: T,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self(Core::new(value, detach, report, None))
    }
    pub fn set_replace(&self, replace: impl Fn() -> T + Send + Sync + 'static) {
        *lock(&self.0.native_replace) = Some(Arc::new(replace));
    }
    pub fn value(&self) -> T {
        lock(&self.0.state).value.clone()
    }
    pub fn advance(&self, value: T, context: Context) {
        self.0.advance(
            WatchCall::Value {
                value,
                ops: Vec::new(),
                retired: false,
            },
            context,
        );
    }
    pub fn fail(&self, error: impl Into<String>, context: Context) {
        self.0.advance(
            WatchCall::Failed {
                error: error.into(),
            },
            context,
        );
    }
    pub fn start<F, Fut>(&self, listener: F) -> crate::Result<()>
    where
        F: Fn(NativeWatchCall<T>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        self.0.start(move |call, context| {
            listener(
                match call {
                    WatchCall::Value { value, .. } => NativeWatchCall::Value { value },
                    WatchCall::Failed { error } => NativeWatchCall::Failed { error },
                },
                context,
            )
        })
    }
    pub fn stop(&self) -> WatchEnd {
        self.0.terminate(WatchEnd::Stopped)
    }
    pub fn end(&self, end: WatchEnd) -> WatchEnd {
        self.0.terminate(end)
    }
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }
    pub fn observe_abort(&self, signal: AbortSignal) -> crate::Result<()> {
        self.0.observe_abort(signal)
    }
    pub async fn closed(&self) -> WatchEnd {
        self.0.closed().await
    }
}
