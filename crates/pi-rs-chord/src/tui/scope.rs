//! UI-library-independent terminal resource ownership and optimistic state.
use crate::{
    Result,
    context::{Cancel, Context, lock},
    contracts::Contract,
    host::{Held, Host},
    json::JsonValue,
    state::{NativeSource, Source},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

type Release = Box<dyn FnOnce() + Send>;
type Changed = Arc<dyn Fn() + Send + Sync>;

pub struct TuiScope {
    host: Host,
    context: Context,
    cancel: Cancel,
    changed: Changed,
    disposed: Arc<AtomicBool>,
    releases: Mutex<Vec<Release>>,
}
impl TuiScope {
    pub fn new(host: Host, changed: impl Fn() + Send + Sync + 'static) -> Self {
        let (context, cancel) = Context::background().with_cancel();
        Self {
            host,
            context,
            cancel,
            changed: Arc::new(changed),
            disposed: Arc::new(AtomicBool::new(false)),
            releases: Mutex::new(Vec::new()),
        }
    }
    fn redraw(&self) -> Changed {
        let disposed = self.disposed.clone();
        let changed = self.changed.clone();
        Arc::new(move || {
            if !disposed.load(Ordering::Acquire) {
                changed();
            }
        })
    }
    pub fn hold(&self, contract: Arc<Contract>, params: JsonValue) -> Result<Held> {
        let held = self.host.hold(contract, params, &self.context)?;
        let _reader = self.watch_native(held.state())?;
        let release = held.clone();
        lock(&self.releases).push(Box::new(move || release.release()));
        Ok(held)
    }
    /// A redraw reads `None` while a source is failed or has no value.
    pub fn watch(&self, source: Source) -> impl Fn() -> Option<JsonValue> + Send + Sync + 'static {
        let changed = self.redraw();
        let advance = changed.clone();
        let subscription = source.subscribe(
            move |_, _| {
                advance();
                Ok(())
            },
            |_| {},
        );
        lock(&self.releases).push(Box::new(move || drop(subscription)));
        changed();
        move || source.value().ok().map(|r| r.to_json())
    }
    pub fn watch_native<T: Clone + Send + 'static>(
        &self,
        source: NativeSource<T>,
    ) -> Result<impl Fn() -> Option<T> + Send + Sync + 'static> {
        let changed = self.redraw();
        let advance = changed.clone();
        let observation = source.observe(
            &self.context,
            move |_, _| {
                advance();
                Ok(())
            },
            |_| {},
        )?;
        lock(&self.releases).push(Box::new(move || drop(observation)));
        changed();
        Ok(move || source.current().ok())
    }
    pub fn page_state<T: Clone + PartialEq + Send + 'static>(
        &self,
        key: &str,
        initial: T,
    ) -> Result<NativeSource<T>> {
        let source = self.host.page_state(key, initial)?;
        let _reader = self.watch_native(source.clone())?;
        Ok(source)
    }
    pub fn optimistic<T: Clone + Send + 'static, A: Clone + Send + 'static>(
        &self,
        read: impl Fn() -> T + Send + Sync + 'static,
        reduce: impl Fn(T, &A) -> T + Send + Sync + 'static,
    ) -> Optimistic<T, A> {
        Optimistic {
            read: Arc::new(read),
            reduce: Arc::new(reduce),
            state: Arc::new(Mutex::new(OptimisticState {
                actions: Vec::new(),
                pending: 0,
            })),
            changed: self.redraw(),
        }
    }
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel("terminal scope disposed");
        let releases = std::mem::take(&mut *lock(&self.releases));
        for release in releases {
            release();
        }
    }
}
impl Drop for TuiScope {
    fn drop(&mut self) {
        self.dispose();
    }
}
struct OptimisticState<A> {
    actions: Vec<A>,
    pending: usize,
}
type Reduce<T, A> = dyn Fn(T, &A) -> T + Send + Sync;
pub struct Optimistic<T, A> {
    read: Arc<dyn Fn() -> T + Send + Sync>,
    reduce: Arc<Reduce<T, A>>,
    state: Arc<Mutex<OptimisticState<A>>>,
    changed: Changed,
}
struct Settled<A> {
    state: Arc<Mutex<OptimisticState<A>>>,
    changed: Changed,
}
impl<A> Drop for Settled<A> {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.state);
            state.pending = state.pending.saturating_sub(1);
            if state.pending == 0 {
                state.actions.clear();
            }
        }
        (self.changed)();
    }
}
impl<T: Clone, A: Clone> Optimistic<T, A> {
    pub fn read(&self) -> T {
        let base = (self.read)();
        let actions = lock(&self.state).actions.clone();
        actions
            .iter()
            .fold(base, |value, action| (self.reduce)(value, action))
    }
    pub fn pending(&self) -> bool {
        lock(&self.state).pending != 0
    }
    pub async fn add<R>(&self, action: A, call: impl std::future::Future<Output = R>) -> R {
        {
            let mut state = lock(&self.state);
            state.actions.push(action);
            state.pending += 1;
        }
        let _settled = Settled {
            state: self.state.clone(),
            changed: self.changed.clone(),
        };
        (self.changed)();
        call.await
    }
}
