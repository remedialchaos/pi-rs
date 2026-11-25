use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use pi_rs_chord::context::Context;
use tokio::runtime::Handle;

use crate::{ids::DocumentId, timers::TimerHandle};

use super::{
    Error, Inner, Key, Result, Session, document_cache::PreparedBatch, finite_millis, lock,
    writer::Persistence,
};

#[derive(Default)]
pub(super) struct Maintenance {
    runtime: Option<Handle>,
    queued: bool,
    next: u64,
    sweep: Option<Alarm>,
    hold: Option<Alarm>,
    sweep_declined: bool,
    pub sweeps: usize,
}

struct Alarm {
    deadline: u64,
    token: u64,
    handle: Option<TimerHandle>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DocumentCacheSizes {
    pub cached: usize,
    pub held: usize,
    pub sweeps: usize,
    pub task_documents: usize,
    pub tails: usize,
}

impl Session {
    pub fn document_cache_sizes(&self) -> DocumentCacheSizes {
        let counts = {
            let state = lock(&self.0.state);
            DocumentCacheSizes {
                cached: state.documents.len(),
                held: state.documents.held_len(),
                sweeps: 0,
                task_documents: state.documents.task_count(),
                tails: state.tails.len(),
            }
        };
        DocumentCacheSizes {
            sweeps: lock(&self.0.maintenance).sweeps,
            ..counts
        }
    }

    /// Persist deferred changes, then drop all current trackers. Future acquisitions cold-load.
    pub async fn unload_documents(&self) -> Result<()> {
        self.0.usable()?;
        let _line = self.0.line.lock().await;
        self.0.line_open()?;
        self.0.flush_holds(None, &Context::background()).await?;
        lock(&self.0.state).documents.clear();
        self.0.schedule_maintenance();
        Ok(())
    }
}

impl Inner {
    pub(super) async fn flush(&self, batch: PreparedBatch, context: &Context) -> Result<()> {
        if batch.writes.is_empty() {
            return Ok(());
        }
        self.healthy()?;
        let owner = self.weak.upgrade().ok_or(Error::Closed)?;
        let keys = batch.keys();
        self.writer
            .admit(
                owner,
                batch,
                keys,
                Persistence::Stored,
                context.without_abort_signal(),
            )
            .await
            .map_err(|_| Error::invalid("Session writer dropped a commit outcome"))?
    }

    pub(super) async fn flush_holds(
        &self,
        id: Option<DocumentId>,
        context: &Context,
    ) -> Result<()> {
        self.drained().await;
        self.healthy()?;
        let batch = lock(&self.state).documents.take_held(id);
        self.flush(batch, context).await
    }

    pub(super) fn schedule_maintenance(&self) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let mut state = lock(&self.maintenance);
        if state.queued {
            return;
        }
        if state.runtime.is_none() {
            state.runtime = Handle::try_current().ok();
        }
        let Some(runtime) = state.runtime.clone() else {
            return;
        };
        let Some(session) = self.weak.upgrade() else {
            return;
        };
        state.queued = true;
        runtime.spawn(async move {
            session.maintain().await;
        });
    }

    async fn maintain(self: Arc<Self>) {
        let _line = self.line.lock().await;
        lock(&self.maintenance).queued = false;
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let now = self.now();
        let due = lock(&self.state)
            .documents
            .next_hold()
            .is_some_and(|deadline| deadline <= now);
        if due
            && self
                .flush_holds(None, &Context::background())
                .await
                .is_err()
        {
            return;
        }
        let pins = lock(&self.observers).pins();
        let (sweep, held) = {
            let mut state = lock(&self.state);
            let next = state.documents.sweep(
                now,
                finite_millis(self.options.document_retention),
                &pins,
                |key| self.writer.gate(&Key::Document(key.to_owned())).is_some(),
            );
            (next, state.documents.next_hold())
        };
        self.arm(sweep, true);
        self.arm(held, false);
    }

    fn arm(&self, deadline: Option<u64>, background: bool) {
        let (old, token, runtime) = {
            let mut state = lock(&self.maintenance);
            if self.closing.load(Ordering::Acquire) || (background && state.sweep_declined) {
                return;
            }
            let slot = if background {
                &mut state.sweep
            } else {
                &mut state.hold
            };
            if slot.as_ref().map(|alarm| alarm.deadline) == deadline {
                return;
            }
            let old = slot.take().and_then(|alarm| alarm.handle);
            state.next += 1;
            let token = state.next;
            let slot = if background {
                &mut state.sweep
            } else {
                &mut state.hold
            };
            *slot = deadline.map(|deadline| Alarm {
                deadline,
                token,
                handle: None,
            });
            (old, token, state.runtime.clone())
        };
        if let Some(old) = old {
            self.disarm(old);
        }
        let (Some(deadline), Some(runtime)) = (deadline, runtime) else {
            return;
        };
        let delay = Duration::from_millis(deadline.saturating_sub(self.now()));
        let weak = self.weak.clone();
        let fire = Box::new(move || {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let mut state = lock(&session.maintenance);
            let slot = if background {
                &mut state.sweep
            } else {
                &mut state.hold
            };
            if slot.as_ref().is_none_or(|alarm| alarm.token != token) {
                return;
            }
            *slot = None;
            if background {
                state.sweeps += 1;
            }
            drop(state);
            let _entered = runtime.enter();
            session.schedule_maintenance();
        });
        let armed = if background {
            self.timers.background(fire, delay)
        } else {
            self.timers.foreground(fire, delay).map(Some)
        };
        let mut state = lock(&self.maintenance);
        let slot = if background {
            &mut state.sweep
        } else {
            &mut state.hold
        };
        match armed {
            Ok(Some(handle)) if slot.as_ref().is_some_and(|alarm| alarm.token == token) => {
                if let Some(alarm) = slot {
                    alarm.handle = Some(handle);
                }
            }
            Ok(Some(handle)) => {
                drop(state);
                self.disarm(handle);
            }
            Ok(None) => {
                *slot = None;
                state.sweep_declined = true;
            }
            Err(error) => {
                *slot = None;
                drop(state);
                self.report(super::ReportKind::Timer, &error.to_string());
            }
        }
    }

    pub(super) fn cancel_maintenance(&self) {
        let (sweep, hold) = {
            let mut state = lock(&self.maintenance);
            (state.sweep.take(), state.hold.take())
        };
        for handle in [sweep, hold]
            .into_iter()
            .flatten()
            .filter_map(|alarm| alarm.handle)
        {
            self.disarm(handle);
        }
    }

    fn disarm(&self, handle: TimerHandle) {
        if let Err(error) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.disarm()))
        {
            self.report(super::ReportKind::Timer, &super::panic_message(&*error));
        }
    }
}
