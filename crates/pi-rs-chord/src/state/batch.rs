use crate::{
    Error, Result,
    context::{Context, lock},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, ThreadId},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchError {
    ListenerError(String),
    DeliveryFailed(String),
}
type Work = Box<dyn FnOnce() -> Result<()> + Send>;
type Delivery = Box<dyn FnOnce(&Context) -> Result<()> + Send>;
#[derive(Default)]
struct Scope {
    running: usize,
    computing: usize,
}
#[derive(Default)]
struct State {
    scopes: HashMap<ThreadId, Scope>,
    owner: Option<ThreadId>,
    sending: bool,
    changed: VecDeque<Delivery>,
    flushes: VecDeque<Delivery>,
    later: Vec<Work>,
    soon: Option<Vec<Work>>,
}
struct Inner {
    state: Mutex<State>,
    available: Condvar,
    report: Arc<dyn Fn(BatchError) + Send + Sync>,
}
#[derive(Clone)]
pub struct Batch(Arc<Inner>);
struct Flags {
    batch: Batch,
    thread: ThreadId,
    compute: bool,
}
impl Drop for Flags {
    fn drop(&mut self) {
        let mut state = lock(&self.batch.0.state);
        if let Some(scope) = state.scopes.get_mut(&self.thread) {
            scope.running -= 1;
            scope.computing -= usize::from(self.compute);
            if scope.running == 0 {
                state.scopes.remove(&self.thread);
            }
        }
    }
}
struct Commit<'a> {
    batch: &'a Batch,
    outer: bool,
}
impl Commit<'_> {
    fn release(&mut self, state: &mut State) {
        if self.outer {
            self.outer = false;
            state.owner = None;
            self.batch.0.available.notify_all();
        }
    }
}
impl Drop for Commit<'_> {
    fn drop(&mut self) {
        if self.outer {
            let mut state = lock(&self.batch.0.state);
            state.sending = false;
            self.release(&mut state);
        }
    }
}
impl Batch {
    pub fn new(report: impl Fn(BatchError) + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            available: Condvar::new(),
            report: Arc::new(report),
        }))
    }
    pub fn running(&self) -> bool {
        lock(&self.0.state)
            .scopes
            .contains_key(&thread::current().id())
    }
    pub fn assert_outside(&self, operation: &str) -> Result<()> {
        if self.running() {
            Err(Error::Invalid(format!(
                "invalid/reentrant: {operation} inside a commit or a derive compute"
            )))
        } else {
            Ok(())
        }
    }
    pub fn assert_not_computing(&self, operation: &str) -> Result<()> {
        if lock(&self.0.state)
            .scopes
            .get(&thread::current().id())
            .is_some_and(|scope| scope.computing > 0)
        {
            Err(Error::Invalid(format!(
                "invalid/reentrant: {operation} inside a derive compute or a compose"
            )))
        } else {
            Ok(())
        }
    }
    fn flags(&self, compute: bool) -> Flags {
        let thread = thread::current().id();
        let mut state = lock(&self.0.state);
        let scope = state.scopes.entry(thread).or_default();
        scope.running += 1;
        scope.computing += usize::from(compute);
        Flags {
            batch: self.clone(),
            thread,
            compute,
        }
    }
    pub fn compute<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let _flags = self.flags(true);
        work()
    }
    pub fn run<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let _flags = self.flags(false);
        work()
    }
    pub fn report_listener(&self, error: String) {
        (self.0.report)(BatchError::ListenerError(error))
    }
    pub fn report_delivery(&self, error: impl ToString) {
        (self.0.report)(BatchError::DeliveryFailed(error.to_string()))
    }
    pub fn commit(&self, context: &Context, adopt: impl FnOnce() -> Result<()>) -> Result<()> {
        let thread = thread::current().id();
        let mut state = self
            .0
            .available
            .wait_while(lock(&self.0.state), |state| {
                state.owner.is_some_and(|owner| owner != thread)
            })
            .unwrap_or_else(|error| error.into_inner());
        let outer = state.owner.replace(thread).is_none();
        drop(state);
        self.commit_boxed(context, Box::new(adopt), Commit { batch: self, outer })
    }
    fn commit_boxed(
        &self,
        context: &Context,
        mut adopt: Box<dyn FnOnce() -> Result<()> + '_>,
        mut commit: Commit<'_>,
    ) -> Result<()> {
        let mut result = Ok(());
        loop {
            let flags = self.flags(false);
            let adopted = adopt();
            if adopted.is_ok() {
                loop {
                    let work = lock(&self.0.state).changed.pop_front();
                    let Some(work) = work else { break };
                    if let Err(error) = work(context) {
                        self.report_delivery(error)
                    }
                }
                lock(&self.0.state).sending = true;
                loop {
                    let flush = lock(&self.0.state).flushes.pop_front();
                    let Some(flush) = flush else { break };
                    if let Err(error) = flush(context) {
                        self.report_delivery(error)
                    }
                }
            }
            result = result.and(adopted);
            drop(flags);
            let later = {
                let mut state = lock(&self.0.state);
                state.sending = false;
                state.changed.clear();
                state.flushes.clear();
                let later = std::mem::take(&mut state.later);
                if later.is_empty() {
                    commit.release(&mut state);
                }
                later
            };
            if later.is_empty() {
                return result;
            }
            adopt = Box::new(move || {
                for work in later {
                    work()?;
                }
                Ok(())
            });
        }
    }
    pub fn within(
        &self,
        context: &Context,
        adopt: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let thread = thread::current().id();
        let mut state = lock(&self.0.state);
        if state.sending || state.owner.is_some_and(|owner| owner != thread) {
            // An independent thread may still enqueue work as the owner drains deliveries.
            state.later.push(Box::new(adopt));
            Ok(())
        } else if state.scopes.contains_key(&thread) {
            drop(state);
            adopt()
        } else {
            state.owner = Some(thread);
            drop(state);
            self.commit_boxed(
                context,
                Box::new(adopt),
                Commit {
                    batch: self,
                    outer: true,
                },
            )
        }
    }
    pub fn soon(
        &self,
        context: Context,
        adopt: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let mut state = lock(&self.0.state);
        if let Some(soon) = &mut state.soon {
            soon.push(Box::new(adopt));
            return Ok(());
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|e| Error::Invalid(e.to_string()))?;
        state.soon = Some(vec![Box::new(adopt)]);
        drop(state);
        let batch = self.clone();
        runtime.spawn(async move {
            tokio::task::yield_now().await;
            let work = lock(&batch.0.state).soon.take().unwrap_or_default();
            if let Err(error) = batch.commit(&context, move || {
                for each in work {
                    each()?
                }
                Ok(())
            }) {
                batch.report_delivery(error)
            }
        });
        Ok(())
    }
    pub(crate) fn changed(&self, deliver: impl FnOnce(&Context) -> Result<()> + Send + 'static) {
        lock(&self.0.state).changed.push_back(Box::new(deliver))
    }
    pub fn flushing(&self, flush: impl FnOnce(&Context) -> Result<()> + Send + 'static) {
        lock(&self.0.state).flushes.push_back(Box::new(flush))
    }
}
