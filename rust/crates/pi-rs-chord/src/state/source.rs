use super::Batch;
use crate::{
    Error, Result,
    context::{Context, lock},
    delta::{Op, Revision, diff_shared},
    json::JsonValue,
    watch::{Watch, WatchCall, WatchEnd},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};
type Compute = Arc<dyn Fn(&[Revision]) -> Result<Revision> + Send + Sync>;
type Report = Arc<dyn Fn(String) + Send + Sync>;
type Advance = Arc<dyn Fn(WatchCall, &Context) -> Result<()> + Send + Sync>;
struct Subscriber {
    advance: Advance,
    end: Arc<dyn Fn(WatchEnd) + Send + Sync>,
}
struct State {
    value: Option<Revision>,
    error: Option<String>,
    detached: Option<String>,
    dirty: bool,
    computing: bool,
    inputs: Vec<Source>,
    compute: Option<Compute>,
    report: Option<Report>,
    failing: bool,
    framed: Option<Option<Revision>>,
    retired: bool,
    explicit_ops: Option<Vec<Op>>,
    dependents: Vec<Weak<Inner>>,
    watches: HashMap<u64, Watch>,
    subscribers: HashMap<u64, Arc<Subscriber>>,
    next: u64,
}
struct Inner {
    batch: Mutex<Option<Batch>>,
    label: String,
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct Source(Arc<Inner>);
pub struct Subscription {
    detach: Option<Box<dyn FnOnce() + Send>>,
}
impl Subscription {
    pub fn detach(&mut self) {
        if let Some(detach) = self.detach.take() {
            detach()
        }
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.detach()
    }
}
impl Source {
    fn make(
        batch: Batch,
        label: String,
        value: Option<Revision>,
        compute: Option<Compute>,
        inputs: Vec<Source>,
        report: Report,
    ) -> Self {
        let dirty = compute.is_some();
        let source = Self(Arc::new(Inner {
            batch: Mutex::new(Some(batch)),
            label,
            state: Mutex::new(State {
                value,
                error: None,
                detached: None,
                dirty,
                computing: false,
                inputs: inputs.clone(),
                compute,
                report: Some(report),
                failing: false,
                framed: None,
                retired: false,
                explicit_ops: None,
                dependents: Vec::new(),
                watches: HashMap::new(),
                subscribers: HashMap::new(),
                next: 0,
            }),
        }));
        for input in inputs {
            lock(&input.0.state)
                .dependents
                .push(Arc::downgrade(&source.0))
        }
        source
    }
    pub fn new(batch: Batch, value: JsonValue) -> Result<Self> {
        Ok(Self::make(
            batch,
            "source".into(),
            Some(Revision::from_json(value)?),
            None,
            Vec::new(),
            Arc::new(|_| {}),
        ))
    }
    pub fn framed(
        batch: Batch,
        label: impl Into<String>,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self::make(
            batch,
            label.into(),
            None,
            None,
            Vec::new(),
            Arc::new(report),
        )
    }
    pub fn derive(
        batch: Batch,
        inputs: Vec<Source>,
        compute: impl Fn(&[Revision]) -> Result<Revision> + Send + Sync + 'static,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self::make(
            batch,
            "derive".into(),
            None,
            Some(Arc::new(compute)),
            inputs,
            Arc::new(report),
        )
    }
    fn batch(&self) -> Result<Batch> {
        lock(&self.0.batch)
            .clone()
            .ok_or_else(|| Error::Invalid("released".into()))
    }
    pub fn value(&self) -> Result<Revision> {
        let state = lock(&self.0.state);
        if let Some(error) = &state.detached {
            return Err(Error::Invalid(error.clone()));
        }
        if !state.dirty {
            if let Some(error) = &state.error {
                return Err(Error::Invalid(error.clone()));
            }
            return state.value.clone().ok_or_else(|| {
                Error::Invalid(format!(
                    "not_ready: {}: read before its first value",
                    self.0.label
                ))
            });
        }
        if state.computing {
            return Err(Error::Invalid("cyclic derive".into()));
        }
        let inputs = state.inputs.clone();
        let compute = state
            .compute
            .clone()
            .ok_or_else(|| Error::Invalid("missing compute".into()))?;
        drop(state);
        lock(&self.0.state).computing = true;
        let result = self.batch()?.compute(|| {
            let values = inputs
                .iter()
                .map(Source::value)
                .collect::<Result<Vec<_>>>()?;
            match compute(&values) {
                Ok(value) => Ok(value),
                Err(error) => {
                    let report = {
                        let mut state = lock(&self.0.state);
                        let report = if !state.failing {
                            state.report.clone()
                        } else {
                            None
                        };
                        state.failing = true;
                        report
                    };
                    if let Some(report) = report {
                        report(error.to_string())
                    }
                    Err(error)
                }
            }
        });
        let mut state = lock(&self.0.state);
        state.dirty = false;
        state.computing = false;
        match &result {
            Ok(value) => {
                state.value = Some(value.clone());
                state.error = None;
                state.failing = false
            }
            Err(error) => state.error = Some(error.to_string()),
        }
        result
    }
    pub fn ready(&self) -> bool {
        let state = lock(&self.0.state);
        state.value.is_some() && state.error.is_none() && state.detached.is_none()
    }
    pub fn failure(&self) -> Option<String> {
        lock(&self.0.state).error.clone()
    }
    pub fn watched(&self) -> bool {
        let s = lock(&self.0.state);
        !s.watches.is_empty() || !s.subscribers.is_empty()
    }
    fn list(&self, state: &mut State, batch: &Batch) {
        if state.framed.is_none() && (!state.watches.is_empty() || !state.subscribers.is_empty()) {
            state.framed = Some(if state.error.is_some() {
                None
            } else {
                state.value.clone()
            });
            let source = self.clone();
            batch.changed(move |context| source.deliver(context));
        }
    }
    fn notify(&self) {
        let dependents = {
            let mut state = lock(&self.0.state);
            state.dependents.retain(|v| v.strong_count() > 0);
            state
                .dependents
                .iter()
                .filter_map(Weak::upgrade)
                .map(Source)
                .collect::<Vec<_>>()
        };
        for dependent in dependents {
            dependent.invalidate()
        }
    }
    fn invalidate(&self) {
        let Ok(batch) = self.batch() else { return };
        let mut state = lock(&self.0.state);
        if state.dirty || state.detached.is_some() {
            return;
        }
        self.list(&mut state, &batch);
        state.dirty = true;
        drop(state);
        self.notify();
    }
    pub(crate) fn invalidator(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        let weak = Arc::downgrade(&self.0);
        Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                Source(inner).invalidate();
                true
            } else {
                false
            }
        })
    }
    pub fn set(&self, value: JsonValue, context: &Context) -> Result<()> {
        self.set_revision(Revision::from_json(value)?, context)
    }
    pub fn set_revision(&self, value: Revision, context: &Context) -> Result<()> {
        let batch = self.batch()?;
        batch.assert_not_computing("source.set")?;
        if lock(&self.0.state).compute.is_some() {
            return Err(Error::Invalid("cannot set a derive".into()));
        }
        let source = self.clone();
        let commit_batch = batch.clone();
        batch.within(context, move || {
            let mut state = lock(&source.0.state);
            if state.detached.is_some() {
                return Ok(());
            }
            source.list(&mut state, &commit_batch);
            state.value = Some(value);
            state.error = None;
            state.failing = false;
            drop(state);
            source.notify();
            Ok(())
        })
    }
    pub fn fail(&self, error: impl Into<String>, context: &Context) -> Result<()> {
        let batch = self.batch()?;
        batch.assert_not_computing("source.fail")?;
        let source = self.clone();
        let commit_batch = batch.clone();
        let error = error.into();
        batch.within(context, move || {
            let mut state = lock(&source.0.state);
            if state.detached.is_some() {
                return Ok(());
            }
            source.list(&mut state, &commit_batch);
            let report = if state.failing {
                None
            } else {
                state.report.clone()
            };
            state.failing = true;
            state.error = Some(error.clone());
            drop(state);
            if let Some(report) = report {
                report(error)
            }
            source.notify();
            Ok(())
        })
    }
    /// Publish an address revision. Multiple writes in one batch fall back to its final root.
    pub(crate) fn address_revision(
        &self,
        value: Revision,
        ops: Vec<Op>,
        retired: bool,
        context: &Context,
    ) -> Result<()> {
        let batch = self.batch()?;
        let source = self.clone();
        let commit_batch = batch.clone();
        batch.within(context, move || {
            let mut state = lock(&source.0.state);
            source.list(&mut state, &commit_batch);
            if state.framed.is_some() {
                state.explicit_ops = Some(
                    if state.explicit_ops.is_some() || state.retired || retired {
                        vec![Op::Replace(value.to_json())]
                    } else {
                        ops
                    },
                );
            }
            state.value = Some(value);
            state.error = None;
            state.retired |= retired;
            drop(state);
            source.notify();
            if retired {
                source.source_retired(&mut HashSet::new())
            }
            Ok(())
        })
    }
    pub fn quiet(&self) {
        lock(&self.0.state).failing = true
    }
    pub fn retire(&self, context: &Context) -> Result<()> {
        let batch = self.batch()?;
        batch.assert_not_computing("source.retire")?;
        let source = self.clone();
        let commit_batch = batch.clone();
        batch.within(context, move || {
            let mut s = lock(&source.0.state);
            source.list(&mut s, &commit_batch);
            s.retired = true;
            drop(s);
            source.notify();
            source.source_retired(&mut HashSet::new());
            Ok(())
        })
    }
    fn source_retired(&self, visited: &mut HashSet<usize>) {
        if !visited.insert(Arc::as_ptr(&self.0) as usize) {
            return;
        }
        let dependents = {
            let mut state = lock(&self.0.state);
            if !state.watches.is_empty() || !state.subscribers.is_empty() {
                state.retired = true
            }
            state
                .dependents
                .iter()
                .filter_map(Weak::upgrade)
                .map(Source)
                .collect::<Vec<_>>()
        };
        for dependent in dependents {
            dependent.source_retired(visited)
        }
    }
    pub fn open_watch(&self, context: &Context) -> Result<Watch> {
        let batch = self.batch()?;
        batch.assert_outside("watch")?;
        let result = self.value();
        let mut state = lock(&self.0.state);
        if state.detached.is_some() {
            return Err(Error::Invalid("released".into()));
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => state.value.clone().ok_or(error)?,
        };
        let error = state.error.clone();
        let id = state.next;
        state.next += 1;
        drop(state);
        let weak = Arc::downgrade(&self.0);
        let watch = Watch::new(
            value.to_json(),
            move || {
                if let Some(source) = weak.upgrade() {
                    lock(&source.state).watches.remove(&id);
                }
            },
            move |e| batch.report_listener(e),
        );
        lock(&self.0.state).watches.insert(id, watch.clone());
        if let Some(signal) = context.abort_signal() {
            watch.observe_abort(signal)?
        }
        if let Some(error) = error {
            watch.fail(error, context.clone())
        }
        Ok(watch)
    }
    pub fn subscribe(
        &self,
        advance: impl Fn(WatchCall, &Context) -> Result<()> + Send + Sync + 'static,
        end: impl Fn(WatchEnd) + Send + Sync + 'static,
    ) -> Subscription {
        let mut state = lock(&self.0.state);
        let id = state.next;
        state.next += 1;
        state.subscribers.insert(
            id,
            Arc::new(Subscriber {
                advance: Arc::new(advance),
                end: Arc::new(end),
            }),
        );
        drop(state);
        let weak = Arc::downgrade(&self.0);
        Subscription {
            detach: Some(Box::new(move || {
                if let Some(source) = weak.upgrade() {
                    lock(&source.state).subscribers.remove(&id);
                }
            })),
        }
    }
    fn deliver(&self, context: &Context) -> Result<()> {
        let before = lock(&self.0.state).framed.take();
        let Some(before) = before else { return Ok(()) };
        let result = self.value();
        let call = match result {
            Ok(value) => {
                let mut state = lock(&self.0.state);
                let retired = state.retired;
                state.retired = false;
                let explicit = state.explicit_ops.take();
                drop(state);
                let publish_empty = explicit.is_some();
                let ops = explicit.unwrap_or_else(|| {
                    before.as_ref().map_or_else(
                        || vec![Op::Replace(value.to_json())],
                        |before| diff_shared(before, &value),
                    )
                });
                if ops.is_empty() && !retired && !publish_empty {
                    return Ok(());
                }
                WatchCall::Value {
                    value: value.to_json(),
                    ops,
                    retired,
                }
            }
            Err(error) => {
                if lock(&self.0.state).detached.is_some() || before.is_none() {
                    return Ok(());
                }
                WatchCall::Failed {
                    error: error.to_string(),
                }
            }
        };
        let (watches, subscribers) = {
            let s = lock(&self.0.state);
            (
                s.watches.values().cloned().collect::<Vec<_>>(),
                s.subscribers.values().cloned().collect::<Vec<_>>(),
            )
        };
        for watch in watches {
            watch.advance(call.clone(), context.clone())
        }
        for subscriber in subscribers {
            if let Err(error) = (subscriber.advance)(call.clone(), context)
                && let Some(batch) = lock(&self.0.batch).clone()
            {
                batch.report_listener(error.to_string());
            }
        }
        Ok(())
    }
    pub fn end_after_delivery(&self) {
        let (watches, subscribers) = {
            let mut s = lock(&self.0.state);
            (
                s.watches.values().cloned().collect::<Vec<_>>(),
                std::mem::take(&mut s.subscribers),
            )
        };
        for w in watches {
            w.end_after_delivery()
        }
        for s in subscribers.into_values() {
            (s.end)(WatchEnd::Ended)
        }
    }
    pub fn detach(&self, error: impl Into<String>) {
        let (watches, subscribers, inputs) = {
            let mut state = lock(&self.0.state);
            if state.detached.is_some() {
                return;
            }
            state.detached = Some(error.into());
            state.framed = None;
            state.compute = None;
            state.report = None;
            state.dependents.clear();
            (
                std::mem::take(&mut state.watches),
                std::mem::take(&mut state.subscribers),
                std::mem::take(&mut state.inputs),
            )
        };
        for input in inputs {
            lock(&input.0.state)
                .dependents
                .retain(|v| !v.ptr_eq(&Arc::downgrade(&self.0)))
        }
        lock(&self.0.batch).take();
        for w in watches.into_values() {
            w.end(WatchEnd::Ended);
        }
        for s in subscribers.into_values() {
            (s.end)(WatchEnd::Ended)
        }
    }
}
#[derive(Clone)]
pub struct Constant {
    value: Revision,
}
pub fn constant(value: JsonValue) -> Result<Constant> {
    Ok(Constant {
        value: Revision::from_json(value)?,
    })
}
impl Constant {
    pub fn value(&self) -> Revision {
        self.value.clone()
    }
    pub fn open_watch(&self, context: &Context) -> Result<Watch> {
        let watch = Watch::new(self.value.to_json(), || {}, |_| {});
        if let Some(signal) = context.abort_signal() {
            watch.observe_abort(signal)?
        }
        Ok(watch)
    }
}
