use super::{Batch, Source};
use crate::{
    Error, Result,
    context::{AbortSubscription, Context, lock},
    delta::Revision,
    watch::{NativeWatch, NativeWatchCall, WatchEnd},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
type Compare<T> = Arc<dyn Fn(&T, &T) -> bool + Send + Sync>;
type Advance<T> = Arc<dyn Fn(NativeWatchCall<T>, &Context) -> Result<()> + Send + Sync>;
struct Observer<T> {
    advance: Advance<T>,
    end: Arc<dyn Fn(WatchEnd) + Send + Sync>,
    abort: Option<AbortSubscription>,
}
struct State<T> {
    value: T,
    error: Option<String>,
    ended: Option<String>,
    shown: Option<std::result::Result<T, String>>,
    listed: bool,
    watches: HashMap<u64, NativeWatch<T>>,
    observers: HashMap<u64, Observer<T>>,
    next: u64,
    invalidators: Vec<Box<dyn Fn() -> bool + Send + Sync>>,
}
struct Inner<T> {
    state: Mutex<State<T>>,
    batch: Mutex<Option<Batch>>,
    same: Compare<T>,
}
#[derive(Clone)]
pub struct NativeSource<T>(Arc<Inner<T>>);
pub struct NativeObservation<T> {
    pub value: T,
    detach: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl<T> NativeObservation<T> {
    pub fn detach(&self) {
        if let Some(detach) = lock(&self.detach).take() {
            detach()
        }
    }
}
impl<T> Drop for NativeObservation<T> {
    fn drop(&mut self) {
        self.detach()
    }
}
impl<T: Clone + Send + 'static> NativeSource<T> {
    pub fn new(
        batch: Batch,
        value: T,
        same: impl Fn(&T, &T) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                value,
                error: None,
                ended: None,
                shown: None,
                listed: false,
                watches: HashMap::new(),
                observers: HashMap::new(),
                next: 0,
                invalidators: Vec::new(),
            }),
            batch: Mutex::new(Some(batch)),
            same: Arc::new(same),
        }))
    }
    pub fn value(&self) -> T {
        lock(&self.0.state).value.clone()
    }
    pub fn current(&self) -> Result<T> {
        let state = lock(&self.0.state);
        if let Some(error) = &state.error {
            return Err(Error::Invalid(error.clone()));
        }
        Ok(state.value.clone())
    }
    fn unfollow(state: &mut State<T>) {
        if state.watches.is_empty() && state.observers.is_empty() {
            state.shown = None
        }
    }
    fn remove(&self, id: u64, end: Option<WatchEnd>) {
        let observer = {
            let mut s = lock(&self.0.state);
            let o = s.observers.remove(&id);
            Self::unfollow(&mut s);
            o
        };
        if let Some(observer) = observer {
            drop(observer.abort);
            if let Some(end) = end {
                (observer.end)(end)
            }
        }
    }
    fn follow(state: &mut State<T>) {
        if state.shown.is_none() {
            state.shown = Some(
                state
                    .error
                    .as_ref()
                    .map_or_else(|| Ok(state.value.clone()), |e| Err(e.clone())),
            )
        }
    }
    pub fn open_watch(&self, context: &Context) -> Result<NativeWatch<T>> {
        let batch = lock(&self.0.batch).clone();
        if let Some(batch) = &batch {
            batch.assert_outside("watch")?
        }
        let mut state = lock(&self.0.state);
        let id = state.next;
        state.next += 1;
        let value = state.value.clone();
        let ended = state.ended.is_some();
        let error = state.error.clone();
        if !ended {
            Self::follow(&mut state)
        }
        drop(state);
        let weak = Arc::downgrade(&self.0);
        let watch = NativeWatch::new(
            value,
            move || {
                if let Some(inner) = weak.upgrade() {
                    let mut s = lock(&inner.state);
                    s.watches.remove(&id);
                    Self::unfollow(&mut s)
                }
            },
            move |e| {
                if let Some(batch) = &batch {
                    batch.report_listener(e)
                }
            },
        );
        if ended {
            watch.end(WatchEnd::Ended);
            return Ok(watch);
        }
        lock(&self.0.state).watches.insert(id, watch.clone());
        if let Some(signal) = context.abort_signal() {
            watch.observe_abort(signal)?
        }
        if let Some(error) = error {
            watch.fail(error, context.clone())
        }
        Ok(watch)
    }
    pub fn observe(
        &self,
        context: &Context,
        advance: impl Fn(NativeWatchCall<T>, &Context) -> Result<()> + Send + Sync + 'static,
        end: impl Fn(WatchEnd) + Send + Sync + 'static,
    ) -> Result<NativeObservation<T>> {
        if let Some(batch) = lock(&self.0.batch).clone() {
            batch.assert_outside("observe")?
        }
        let mut state = lock(&self.0.state);
        let value = state.value.clone();
        let id = state.next;
        state.next += 1;
        let ended = state.ended.is_some();
        let aborted = context.is_aborted();
        if ended || aborted {
            drop(state);
            end(if ended {
                WatchEnd::Ended
            } else {
                WatchEnd::Aborted
            });
            return Ok(NativeObservation {
                value,
                detach: Mutex::new(None),
            });
        }
        Self::follow(&mut state);
        state.observers.insert(
            id,
            Observer {
                advance: Arc::new(advance),
                end: Arc::new(end),
                abort: None,
            },
        );
        drop(state);
        if let Some(signal) = context.abort_signal() {
            let weak = Arc::downgrade(&self.0);
            let subscription = signal.subscribe(move |_| {
                if let Some(inner) = weak.upgrade() {
                    NativeSource(inner).remove(id, Some(WatchEnd::Aborted))
                }
            });
            if let Some(observer) = lock(&self.0.state).observers.get_mut(&id) {
                observer.abort = Some(subscription)
            }
        }
        let weak = Arc::downgrade(&self.0);
        Ok(NativeObservation {
            value,
            detach: Mutex::new(Some(Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    NativeSource(inner).remove(id, None)
                }
            }))),
        })
    }
    fn changed(&self, batch: &Batch) {
        let mut state = lock(&self.0.state);
        let invalidators = std::mem::take(&mut state.invalidators);
        if !state.listed && state.shown.is_some() {
            state.listed = true;
            let source = self.clone();
            batch.changed(move |ctx| source.deliver(ctx))
        }
        drop(state);
        let invalidators = invalidators.into_iter().filter(|f| f()).collect::<Vec<_>>();
        lock(&self.0.state).invalidators.extend(invalidators);
    }
    pub fn set(&self, value: T, context: &Context) -> Result<()> {
        let state = lock(&self.0.state);
        if let Some(error) = &state.ended {
            return Err(Error::Invalid(error.clone()));
        }
        if state.error.is_none() && (self.0.same)(&value, &state.value) {
            return Ok(());
        }
        drop(state);
        let batch = lock(&self.0.batch)
            .clone()
            .ok_or_else(|| Error::Invalid("released".into()))?;
        batch.assert_not_computing("writable.set")?;
        let source = self.clone();
        let in_batch = batch.clone();
        batch.within(context, move || {
            let mut state = lock(&source.0.state);
            state.value = value;
            state.error = None;
            drop(state);
            source.changed(&in_batch);
            Ok(())
        })
    }
    pub fn fail(&self, error: impl Into<String>, context: &Context) -> Result<()> {
        let batch = lock(&self.0.batch)
            .clone()
            .ok_or_else(|| Error::Invalid("released".into()))?;
        batch.assert_not_computing("native.fail")?;
        let source = self.clone();
        let in_batch = batch.clone();
        let error = error.into();
        batch.within(context, move || {
            lock(&source.0.state).error = Some(error);
            source.changed(&in_batch);
            Ok(())
        })
    }
    fn deliver(&self, context: &Context) -> Result<()> {
        let mut state = lock(&self.0.state);
        state.listed = false;
        let Some(shown) = &state.shown else {
            return Ok(());
        };
        let call = if let Some(error) = &state.error {
            if shown.is_err() {
                return Ok(());
            }
            NativeWatchCall::Failed {
                error: error.clone(),
            }
        } else {
            if let Ok(before) = shown
                && (self.0.same)(before, &state.value)
            {
                return Ok(());
            }
            NativeWatchCall::Value {
                value: state.value.clone(),
            }
        };
        state.shown = Some(match &call {
            NativeWatchCall::Value { value } => Ok(value.clone()),
            NativeWatchCall::Failed { error } => Err(error.clone()),
        });
        let watches = state.watches.values().cloned().collect::<Vec<_>>();
        let observers = state
            .observers
            .values()
            .map(|o| o.advance.clone())
            .collect::<Vec<_>>();
        drop(state);
        for w in watches {
            match &call {
                NativeWatchCall::Value { value } => w.advance(value.clone(), context.clone()),
                NativeWatchCall::Failed { error } => w.fail(error.clone(), context.clone()),
            }
        }
        for observer in observers {
            if let Err(error) = observer(call.clone(), context)
                && let Some(batch) = lock(&self.0.batch).clone()
            {
                batch.report_listener(error.to_string());
            }
        }
        Ok(())
    }
    pub fn detach(&self, error: impl Into<String>) {
        let (watches, observers) = {
            let mut state = lock(&self.0.state);
            if state.ended.is_some() {
                return;
            }
            state.ended = Some(error.into());
            state.shown = None;
            state.invalidators.clear();
            (
                std::mem::take(&mut state.watches),
                std::mem::take(&mut state.observers),
            )
        };
        lock(&self.0.batch).take();
        for w in watches.into_values() {
            w.end(WatchEnd::Ended);
        }
        for o in observers.into_values() {
            drop(o.abort);
            (o.end)(WatchEnd::Ended)
        }
    }
    /// Turns a native input into a lazy JSON derive without serializing native values.
    pub fn project(
        &self,
        project: impl Fn(T) -> Result<Revision> + Send + Sync + 'static,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Source> {
        let batch = lock(&self.0.batch)
            .clone()
            .ok_or_else(|| Error::Invalid("released".into()))?;
        let source = self.clone();
        let derived = Source::derive(
            batch,
            Vec::new(),
            move |_| project(source.current()?),
            report,
        );
        lock(&self.0.state).invalidators.push(derived.invalidator());
        Ok(derived)
    }
}
