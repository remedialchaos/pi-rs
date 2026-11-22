use crate::{
    Error, Result,
    context::{Context, lock},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchError {
    ListenerError(String),
    DeliveryFailed(String),
}
type Work = Box<dyn FnOnce() -> Result<()> + Send>;
type Delivery = Box<dyn FnOnce(&Context) -> Result<()> + Send>;
#[derive(Default)]
struct State {
    running: bool,
    computing: bool,
    sending: bool,
    changed: VecDeque<Delivery>,
    flushes: VecDeque<Delivery>,
    later: Vec<Work>,
    soon: Option<Vec<Work>>,
}
struct Inner {
    state: Mutex<State>,
    report: Arc<dyn Fn(BatchError) + Send + Sync>,
}
#[derive(Clone)]
pub struct Batch(Arc<Inner>);
struct Flags {
    batch: Batch,
    running: bool,
    computing: bool,
}
impl Drop for Flags {
    fn drop(&mut self) {
        let mut state = lock(&self.batch.0.state);
        state.running = self.running;
        state.computing = self.computing;
    }
}
impl Batch {
    pub fn new(report: impl Fn(BatchError) + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            report: Arc::new(report),
        }))
    }
    pub fn running(&self) -> bool {
        lock(&self.0.state).running
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
        if lock(&self.0.state).computing {
            Err(Error::Invalid(format!(
                "invalid/reentrant: {operation} inside a derive compute or a compose"
            )))
        } else {
            Ok(())
        }
    }
    fn flags(&self, compute: bool) -> Flags {
        let mut state = lock(&self.0.state);
        let flags = Flags {
            batch: self.clone(),
            running: state.running,
            computing: state.computing,
        };
        state.running = true;
        state.computing |= compute;
        flags
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
        self.commit_boxed(context, Box::new(adopt))
    }
    fn commit_boxed(
        &self,
        context: &Context,
        adopt: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<()> {
        let flags = self.flags(false);
        let result = adopt();
        if result.is_ok() {
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
        let later = {
            let mut state = lock(&self.0.state);
            state.sending = false;
            state.changed.clear();
            state.flushes.clear();
            std::mem::take(&mut state.later)
        };
        drop(flags);
        result?;
        if !later.is_empty() {
            self.commit_boxed(
                context,
                Box::new(move || {
                    for work in later {
                        work()?
                    }
                    Ok(())
                }),
            )?
        }
        Ok(())
    }
    pub fn within(
        &self,
        context: &Context,
        adopt: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let state = lock(&self.0.state);
        if state.sending {
            drop(state);
            lock(&self.0.state).later.push(Box::new(adopt));
            Ok(())
        } else if state.running {
            drop(state);
            adopt()
        } else {
            drop(state);
            self.commit(context, adopt)
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
