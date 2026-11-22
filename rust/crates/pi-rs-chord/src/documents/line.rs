use crate::{
    Error, Result,
    context::{AbortSignal, Cancel, lock},
};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;
#[derive(Default)]
struct State {
    busy: bool,
    waiting: VecDeque<oneshot::Sender<LineLease>>,
}
#[derive(Clone, Default)]
pub struct Line(Arc<Mutex<State>>);
struct Lease {
    owner: Weak<Mutex<State>>,
    released: Mutex<bool>,
}
#[derive(Clone)]
pub struct LineLease(Arc<Lease>);
impl Line {
    /// Reservation happens on this call, not when the returned future is first polled.
    pub fn acquire(&self) -> impl Future<Output = Result<LineLease>> + Send + 'static {
        let (send, receive) = oneshot::channel();
        let mut state = lock(&self.0);
        if state.busy {
            state.waiting.push_back(send)
        } else {
            state.busy = true;
            let _ = send.send(LineLease(Arc::new(Lease {
                owner: Arc::downgrade(&self.0),
                released: Mutex::new(false),
            })));
        }
        drop(state);
        async move { receive.await.map_err(|_| Error::Closed) }
    }
}
impl LineLease {
    pub fn release(&self) {
        self.0.release()
    }
}
impl Lease {
    fn release(&self) {
        {
            let mut released = lock(&self.released);
            if *released {
                return;
            }
            *released = true;
        }
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        loop {
            let next = {
                let mut state = lock(&owner);
                match state.waiting.pop_front() {
                    Some(next) => Some(next),
                    None => {
                        state.busy = false;
                        None
                    }
                }
            };
            let Some(next) = next else { return };
            let lease = LineLease(Arc::new(Lease {
                owner: Arc::downgrade(&owner),
                released: Mutex::new(false),
            }));
            if let Err(lease) = next.send(lease) {
                *lock(&lease.0.released) = true;
            } else {
                return;
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.release()
    }
}
#[derive(Clone)]
pub struct SealToken {
    pub(super) signal: AbortSignal,
    cancel: Cancel,
}
impl Default for SealToken {
    fn default() -> Self {
        let (signal, cancel) = AbortSignal::new();
        Self { signal, cancel }
    }
}
impl SealToken {
    pub fn sealed(&self) -> bool {
        self.signal.aborted()
    }
    pub fn seal(&self) {
        self.cancel.cancel("released: sealed")
    }
    pub(super) fn same(&self, other: &Self) -> bool {
        self.signal.same(&other.signal)
    }
}
