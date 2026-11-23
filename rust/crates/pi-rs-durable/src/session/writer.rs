use std::{
    collections::HashMap,
    mem,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};

use pi_rs_chord::context::Context;
use tokio::sync::{oneshot, watch};

use crate::{
    ids::Seq,
    storage::{CommitResult, StorageError},
    storage_types::StorageWrite,
};

use super::{Error, Inner, Key, Result, document_cache::PreparedBatch, lock};

#[derive(Clone)]
pub(crate) struct Gate(watch::Sender<bool>);

impl Gate {
    fn new() -> Self {
        Self(watch::channel(false).0)
    }
    fn open(&self) {
        self.0.send_replace(true);
    }
    fn same_as(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
    pub async fn wait(self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
}

pub(crate) enum Persistence {
    Stored,
    Held { deadline: Option<u64> },
}

struct Part {
    batch: PreparedBatch,
    keys: Vec<Key>,
    persistence: Persistence,
    context: Context,
    gate: Gate,
    outcome: oneshot::Sender<Result<()>>,
}

#[derive(Default)]
struct Queue {
    parts: Vec<Part>,
    running: bool,
    unpublished: HashMap<Key, Gate>,
    newest: Option<Gate>,
}

/// Receipts establish durability; caches and observers advance only as each part is published.
#[derive(Default)]
pub(crate) struct Writer(Mutex<Queue>);

impl Writer {
    pub fn gate(&self, key: &Key) -> Option<Gate> {
        let queue = lock(&self.0);
        if *key == Key::All {
            queue.newest.clone()
        } else {
            queue.unpublished.get(key).cloned()
        }
    }

    pub fn admit(
        &self,
        session: Arc<Inner>,
        batch: PreparedBatch,
        keys: Vec<Key>,
        persistence: Persistence,
        context: Context,
    ) -> oneshot::Receiver<Result<()>> {
        let (outcome, receiver) = oneshot::channel();
        let gate = Gate::new();
        let mut queue = lock(&self.0);
        for key in &keys {
            queue.unpublished.insert(key.clone(), gate.clone());
        }
        queue.newest = Some(gate.clone());
        queue.parts.push(Part {
            batch,
            keys,
            persistence,
            context,
            gate,
            outcome,
        });
        if !queue.running {
            queue.running = true;
            // Storage adapters are synchronous. Offload the call and keep the line available
            // to prepare unrelated jobs; admission itself never awaits the writer.
            tokio::spawn(async move {
                session.writer.run(&session).await;
            });
        }
        receiver
    }

    async fn run(&self, session: &Arc<Inner>) {
        loop {
            let mut parts = {
                let mut queue = lock(&self.0);
                if queue.parts.is_empty() {
                    queue.running = false;
                    return;
                }
                mem::take(&mut queue.parts)
            };
            if let Some(cause) = session.failure() {
                for part in parts {
                    self.finish(part, Err(session.failed(&cause)));
                }
                continue;
            }
            let writes: Vec<_> = parts
                .iter_mut()
                .filter_map(|part| match part.persistence {
                    Persistence::Stored => Some(mem::take(&mut part.batch.writes)),
                    Persistence::Held { .. } => None,
                })
                .collect();
            let stored = if writes.is_empty() {
                None
            } else {
                let storage = Arc::clone(&session.storage);
                Some(tokio::task::spawn_blocking(move || store(&*storage, writes)).await)
            };
            let mut backend_error = None;
            let mut refusal = None;
            let mut slots = Vec::new().into_iter().zip(Vec::new());
            match stored {
                Some(Ok(Stored {
                    batches,
                    receipts,
                    result,
                })) => {
                    slots = batches.into_iter().zip(receipts);
                    match result {
                        Ok(CommitResult::Stored { .. }) => {}
                        Ok(CommitResult::Refused { refused, .. }) => {
                            refusal = Some(Arc::new(refused))
                        }
                        Err(error) => backend_error = Some(Arc::new(error)),
                    }
                }
                Some(Err(error)) => {
                    backend_error = Some(Arc::new(StorageError::Backend(error.to_string())))
                }
                None => {}
            }
            let mut stopped = session.failure();
            let mut external = stopped.is_some();
            let mut delayed = Vec::new();
            for (index, mut part) in parts.into_iter().enumerate() {
                let stored = matches!(part.persistence, Persistence::Stored);
                let seq = if stored {
                    slots.next().and_then(|(batch, seq)| {
                        part.batch.writes = batch;
                        seq
                    })
                } else {
                    None
                };
                if let Some(cause) = &stopped {
                    let result = if stored
                        && !external
                        && let Some(error) = &backend_error
                    {
                        Err(Error::Storage(error.clone()))
                    } else {
                        Err(session.failed(cause))
                    };
                    delayed.push((part, result));
                    continue;
                }
                if stored && seq.is_none() {
                    let error = backend_error
                        .clone()
                        .or_else(|| refusal.take())
                        .unwrap_or_else(|| {
                            Arc::new(StorageError::Backend(
                                "Storage committed without receipt".into(),
                            ))
                        });
                    stopped = Some(error.to_string());
                    delayed.push((part, Err(Error::Storage(error))));
                    continue;
                }
                // An earlier part's observer and waiter can run before the next publication.
                if index > 0 {
                    tokio::task::yield_now().await;
                }
                if let Some(cause) = session.failure() {
                    delayed.push((part, Err(session.failed(&cause))));
                    stopped = Some(cause);
                    external = true;
                    continue;
                }
                match session.publish_in_storage(
                    &mut part.batch,
                    &part.persistence,
                    seq,
                    &part.context,
                ) {
                    Err(error) => {
                        stopped = Some(error.to_string());
                        delayed.push((part, Err(error)));
                    }
                    Ok(listener_failure) => {
                        if let Some(error) = &backend_error {
                            if let Some(listener_failure) = listener_failure {
                                session.report(
                                    super::ReportKind::Listener,
                                    &listener_failure.to_string(),
                                );
                            }
                            delayed.push((part, Err(Error::Storage(error.clone()))));
                        } else {
                            if let Some(listener_failure) = listener_failure {
                                let cause = listener_failure.to_string();
                                session.fail(cause.clone());
                                stopped = Some(cause);
                            }
                            self.finish(part, Ok(()));
                            session.schedule_maintenance();
                        }
                    }
                }
            }
            if let Some(cause) = backend_error
                .as_ref()
                .map(|error| error.to_string())
                .or(stopped)
                .or_else(|| refusal.map(|error| error.to_string()))
            {
                session.fail(cause);
            }
            for (part, outcome) in delayed {
                self.finish(part, outcome);
            }
        }
    }

    fn finish(&self, part: Part, outcome: Result<()>) {
        let mut queue = lock(&self.0);
        for key in &part.keys {
            if queue
                .unpublished
                .get(key)
                .is_some_and(|gate| gate.same_as(&part.gate))
            {
                queue.unpublished.remove(key);
            }
        }
        if queue
            .newest
            .as_ref()
            .is_some_and(|gate| gate.same_as(&part.gate))
        {
            queue.newest = None;
        }
        let _ = part.outcome.send(outcome);
        part.gate.open();
    }
}

struct Stored {
    batches: Vec<Vec<StorageWrite>>,
    receipts: Vec<Option<Seq>>,
    result: crate::storage::StorageResult<CommitResult>,
}

fn store(storage: &dyn crate::storage::Storage, batches: Vec<Vec<StorageWrite>>) -> Stored {
    let mut receipts = vec![None; batches.len()];
    let mut next = 0;
    let mut malformed = false;
    let mut previous = None;
    let result = catch_unwind(AssertUnwindSafe(|| {
        storage.commit(&batches, &mut |index, seq| {
            if !malformed
                && index == next
                && index < receipts.len()
                && previous.is_none_or(|previous| seq > previous)
            {
                receipts[index] = Some(seq);
                previous = Some(seq);
                next += 1;
            } else {
                malformed = true;
            }
        })
    }))
    .unwrap_or_else(|_| Err(StorageError::Backend("Storage commit panicked".into())));
    let counts_match = match &result {
        Ok(CommitResult::Stored { stored }) => *stored == next && next == batches.len(),
        Ok(CommitResult::Refused { stored, .. }) => *stored == next && next < batches.len(),
        Err(_) => true,
    };
    let result = if malformed || !counts_match {
        Err(StorageError::Backend(
            "Storage returned inconsistent commit receipts".into(),
        ))
    } else {
        result
    };
    Stored {
        batches,
        receipts,
        result,
    }
}
