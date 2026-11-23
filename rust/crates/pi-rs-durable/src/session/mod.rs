//! Atomic session mutation line, fork-aware history, and publication-driven documents.

mod cached_storage;
mod conversation_tails;
mod document_cache;
mod forks;
pub mod keys;
mod maintenance;
mod observers;
mod readers;
mod transaction;
mod writer;

use std::{
    any::Any,
    ops::AsyncFnOnce,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::future::BoxFuture;
use pi_rs_chord::{
    context::{AbortError, Context},
    delta::DeltaError,
    watch::WatchEnd,
};
use pi_rs_telemetry::{AttributeValue, TelemetrySpan};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};

use crate::{
    documents::DocumentError,
    errors::{BoxError, SessionError},
    ids::*,
    storage::{Storage, StorageError, StorageResult},
    storage_types::*,
    timers::{SetTimer, Timers, host_timers},
};

use cached_storage::CachedStorage;
use conversation_tails::ConversationTails;
use document_cache::{DocumentCache, PreparedBatch};
use maintenance::Maintenance;
use observers::{Listener, Observers};
use writer::{Persistence, Writer};

pub use conversation_tails::HeadMarker;
pub use keys::Key;
pub use maintenance::DocumentCacheSizes;
pub use observers::{
    CommitObserver, DocumentObserver, DocumentWatch, Observation, Observer, Subscription,
};
pub use transaction::{TaskOptions, TaskTrace, Transaction, TransactionScope};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Storage(Arc<StorageError>),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Draft(#[from] DeltaError),
    #[error(transparent)]
    Chord(#[from] pi_rs_chord::Error),
    #[error(transparent)]
    Cancelled(#[from] AbortError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Callback(#[from] BoxError),
    #[error("Session is closed")]
    Closed,
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl From<StorageError> for Error {
    fn from(value: StorageError) -> Self {
        Self::Storage(Arc::new(value))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    Closed,
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub enum CommitPersistence {
    Immediate,
    /// With no window, use the Session default. `Duration::MAX` holds until touched or closed.
    Deferred {
        within: Option<Duration>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct CommitOptions {
    pub persistence: CommitPersistence,
}

impl Default for CommitOptions {
    fn default() -> Self {
        Self {
            persistence: CommitPersistence::Immediate,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ObservedCommit {
    Stored(Arc<CommitPublication>),
    Memory(Vec<CommitChange>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportKind {
    StorageFailed,
    Extension,
    Host,
    TaskFaulted,
    Listener,
    Timer,
    Telemetry,
}

pub type ConversationCreated = Arc<
    dyn for<'a, 'session> Fn(
            &'a mut Transaction<'session>,
            &'a ConversationRecord,
        ) -> BoxFuture<'a, Result<()>>
        + Send
        + Sync,
>;
pub type MembershipKeys = Arc<dyn Fn(&[StorageWrite]) -> Result<Vec<Key>> + Send + Sync>;
pub type ClosingWrites = Arc<dyn Fn() -> Result<Vec<StorageWrite>> + Send + Sync>;
pub type Reporter = Arc<dyn Fn(ReportKind, &str) + Send + Sync>;

pub struct SessionOptions {
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub document_retention: Duration,
    pub persist_within: Duration,
    pub set_timer: Option<SetTimer>,
    pub read_only: bool,
    pub report: Option<Reporter>,
    pub conversation_created: Option<ConversationCreated>,
    pub membership_keys: Option<MembershipKeys>,
    pub closing_writes: Option<ClosingWrites>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            now: Arc::new(wall_clock),
            document_retention: Duration::from_secs(600),
            persist_within: Duration::from_secs(1),
            set_timer: None,
            read_only: false,
            report: None,
            conversation_created: None,
            membership_keys: None,
            closing_writes: None,
        }
    }
}

#[derive(Clone)]
pub struct Session(Arc<Inner>);

pub fn create_session(storage: Arc<dyn Storage>, options: SessionOptions) -> Session {
    Session::new(storage, options)
}

impl Session {
    pub(crate) fn harness_report(&self, kind: ReportKind, text: &str) {
        self.0.report(kind, text);
    }
    pub(crate) fn timers(&self) -> &Timers {
        &self.0.timers
    }

    pub fn new(storage: Arc<dyn Storage>, options: SessionOptions) -> Self {
        Self(Arc::new_cyclic(|weak: &Weak<Inner>| {
            let report = weak.clone();
            let timers = host_timers(
                options.set_timer.clone(),
                Arc::new(move |error| {
                    if let Some(session) = report.upgrade() {
                        session.report(ReportKind::Timer, &error.to_string());
                    }
                }),
            );
            Inner {
                storage,
                options,
                timers,
                weak: weak.clone(),
                line: AsyncMutex::new(()),
                room: Semaphore::new(256),
                state: Mutex::default(),
                writer: Writer::default(),
                observers: Mutex::default(),
                maintenance: Mutex::default(),
                closing: AtomicBool::default(),
                failed: Mutex::default(),
                closed: watch::channel(None).0,
            }
        }))
    }

    /// Stage one transaction. Its callback may run while unrelated earlier writes reach Storage;
    /// the outcome is delivered only once every earlier admitted commit settled.
    pub async fn commit<T>(
        &self,
        change: impl for<'a, 'tx> AsyncFnOnce(&'a mut Transaction<'tx>) -> Result<T>,
        context: &Context,
    ) -> Result<T> {
        self.commit_with(
            change,
            context,
            TransactionScope::default(),
            CommitOptions::default(),
        )
        .await
    }

    pub async fn commit_with<T>(
        &self,
        change: impl for<'a, 'tx> AsyncFnOnce(&'a mut Transaction<'tx>) -> Result<T>,
        context: &Context,
        scope: TransactionScope,
        options: CommitOptions,
    ) -> Result<T> {
        let session = &self.0;
        session.usable()?;
        context.check()?;
        let _permit = context
            .await_with(session.room.acquire())
            .await?
            .map_err(|_| Error::Closed)?;
        let line = context.await_with(session.line.lock()).await?;
        let earlier = session.writer.gate(&Key::All);
        let prepared: Result<_> = async {
            session.line_open()?;
            context.check()?;
            let mut tx = Transaction::new(session, context, scope);
            let value = change(&mut tx).await?;
            session.healthy()?;
            let mut batch = tx.prepare().await?;
            if session.options.read_only && batch.has_writes() {
                return Err(SessionError::ReadOnly("a commit with writes".into()).into());
            }
            let persistence = match options.persistence {
                CommitPersistence::Deferred { within }
                    if batch.has_writes() && batch.holdable() =>
                {
                    let delay = within.unwrap_or(session.options.persist_within);
                    Persistence::Held {
                        deadline: finite_millis(delay)
                            .map(|delay| session.now().saturating_add(delay)),
                    }
                }
                _ => Persistence::Stored,
            };
            let mut keys = match &session.options.membership_keys {
                Some(get) if batch.has_writes() && matches!(persistence, Persistence::Stored) => {
                    get(&batch.writes)?
                }
                _ => Vec::new(),
            };
            if matches!(persistence, Persistence::Stored) {
                batch.assemble(&mut lock(&session.state).documents);
            }
            if !batch.has_writes() {
                return Ok((value, None));
            }
            keys.extend(batch.keys());
            let outcome =
                session
                    .writer
                    .admit(session.clone(), batch, keys, persistence, context.clone());
            Ok((value, Some(outcome)))
        }
        .await;
        session.schedule_maintenance();
        drop(line);
        if let Some(earlier) = earlier {
            earlier.wait().await;
        }
        let (value, outcome) = prepared?;
        if let Some(outcome) = outcome {
            outcome
                .await
                .map_err(|_| Error::invalid("Session writer dropped a commit outcome"))??;
        } else {
            session.healthy()?;
        }
        Ok(value)
    }

    pub fn observe_commits(&self, observer: CommitObserver) -> Result<Subscription> {
        let mut observers = lock(&self.0.observers);
        self.0.usable()?;
        let id = observers.allocate();
        observers
            .commits
            .insert(id, Arc::new(Listener::new(observer)));
        let weak = self.0.weak.clone();
        Ok(Subscription::new(move || {
            if let Some(session) = weak.upgrade()
                && let Some(listener) = lock(&session.observers).commits.shift_remove(&id)
            {
                listener.deactivate();
            }
        }))
    }

    pub fn follow_commits(
        &self,
        listener: impl Fn(&ObservedCommit, &Context) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Subscription> {
        let mut observers = lock(&self.0.observers);
        self.0.usable()?;
        let id = observers.allocate();
        observers.follows.insert(id, Arc::new(listener));
        let weak = self.0.weak.clone();
        Ok(Subscription::new(move || {
            if let Some(session) = weak.upgrade() {
                lock(&session.observers).follows.shift_remove(&id);
            }
        }))
    }

    pub fn subscribe_close(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> Result<Subscription> {
        let mut observers = lock(&self.0.observers);
        self.0.usable()?;
        let id = observers.allocate();
        observers.close.insert(id, Arc::new(listener));
        let weak = self.0.weak.clone();
        Ok(Subscription::new(move || {
            if let Some(session) = weak.upgrade() {
                lock(&session.observers).close.shift_remove(&id);
            }
        }))
    }

    pub fn failure(&self) -> Option<String> {
        self.0.failure()
    }

    pub async fn close(&self, context: &Context) -> Result<()> {
        self.0.begin_close();
        context.await_with(self.closed()).await?;
        match &*self.0.closed.borrow() {
            Some((_, Some(message))) => Err(StorageError::Backend(message.clone()).into()),
            _ => Ok(()),
        }
    }

    pub async fn closed(&self) -> SessionEnd {
        let mut closed = self.0.closed.subscribe();
        loop {
            if let Some((end, _)) = closed.borrow_and_update().clone() {
                return end;
            }
            if closed.changed().await.is_err() {
                return SessionEnd::Closed;
            }
        }
    }
}

#[derive(Default)]
struct State {
    records: CachedStorage,
    tails: ConversationTails,
    documents: DocumentCache,
}

struct Inner {
    storage: Arc<dyn Storage>,
    options: SessionOptions,
    timers: Timers,
    weak: Weak<Self>,
    writer: Writer,
    line: AsyncMutex<()>,
    room: Semaphore,
    state: Mutex<State>,
    observers: Mutex<Observers>,
    maintenance: Mutex<Maintenance>,
    closing: AtomicBool,
    failed: Mutex<Option<String>>,
    closed: watch::Sender<Option<(SessionEnd, Option<String>)>>,
}

impl Inner {
    fn now(&self) -> u64 {
        catch_unwind(AssertUnwindSafe(|| (self.options.now)())).unwrap_or_else(|_| wall_clock())
    }
    fn failure(&self) -> Option<String> {
        lock(&self.failed).clone()
    }
    fn failed(&self, cause: &str) -> Error {
        SessionError::Failed(Box::new(std::io::Error::other(cause.to_owned()))).into()
    }
    fn healthy(&self) -> Result<()> {
        self.failure()
            .map_or(Ok(()), |cause| Err(self.failed(&cause)))
    }
    // A caller may already be queued when close seals new admission. It can proceed
    // while the backend is still open, but never after the cleanup job ran.
    fn line_open(&self) -> Result<()> {
        self.healthy()?;
        if self.closed.borrow().is_some() {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }

    fn usable(&self) -> Result<()> {
        self.healthy()?;
        if self.closing.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
    fn fail(&self, cause: String) {
        let mut failure = lock(&self.failed);
        if failure.is_some() {
            return;
        }
        *failure = Some(cause.clone());
        drop(failure);
        self.begin_close();
        self.report(ReportKind::StorageFailed, &cause);
    }
    fn report(&self, kind: ReportKind, message: &str) {
        if let Some(report) = &self.options.report {
            let _ = catch_unwind(AssertUnwindSafe(|| report(kind, message)));
        }
    }

    async fn touch(&self, key: &Key) -> Result<()> {
        if let Some(gate) = self.writer.gate(key) {
            gate.wait().await;
        }
        self.healthy()
    }

    async fn drained(&self) {
        if let Some(gate) = self.writer.gate(&Key::All) {
            gate.wait().await;
        }
    }

    fn read_storage<T>(&self, read: impl FnOnce(&dyn Storage) -> StorageResult<T>) -> Result<T> {
        self.storage_call(true, read)
    }

    fn storage_call<T>(
        &self,
        read: bool,
        call: impl FnOnce(&dyn Storage) -> StorageResult<T>,
    ) -> Result<T> {
        self.healthy()?;
        let value = catch_unwind(AssertUnwindSafe(|| call(&*self.storage)))
            .unwrap_or_else(|error| Err(StorageError::Backend(panic_message(&*error))));
        self.healthy()?;
        match value {
            Err(error) => {
                if !(read && matches!(error, StorageError::Request(_))) {
                    self.fail(error.to_string());
                }
                Err(error.into())
            }
            Ok(value) => Ok(value),
        }
    }

    fn mint_id(&self) -> Result<u64> {
        self.storage_call(false, |storage| storage.mint_id())
    }

    fn publish_in_storage(
        &self,
        batch: &mut PreparedBatch,
        persistence: &Persistence,
        seq: Option<Seq>,
        context: &Context,
    ) -> Result<Option<Error>> {
        let mut adopted = None;
        let mut publish = || {
            if adopted.is_none() {
                adopted = Some(
                    catch_unwind(AssertUnwindSafe(|| {
                        self.publish(batch, persistence, seq, context)
                    }))
                    .unwrap_or_else(|error| Err(Error::invalid(panic_message(&*error)))),
                );
            }
        };
        if let Err(error) = catch_unwind(AssertUnwindSafe(|| self.storage.publishing(&mut publish)))
        {
            self.report(ReportKind::Listener, &panic_message(&*error));
        }
        publish();
        adopted.ok_or_else(|| Error::invalid("Storage publication did not run"))?
    }

    fn publish(
        &self,
        batch: &mut PreparedBatch,
        persistence: &Persistence,
        seq: Option<Seq>,
        context: &Context,
    ) -> Result<Option<Error>> {
        if batch.silent {
            return Ok(None);
        }
        let describe = lock(&self.observers).has_publications();
        let deadline = match persistence {
            Persistence::Held { deadline } => *deadline,
            Persistence::Stored => None,
        };
        let now = self.now();
        let adoption = {
            let mut state = lock(&self.state);
            if seq.is_some() {
                state.records.published(&batch.writes);
                state.tails.apply(&batch.writes);
            }
            state.documents.adopt(batch, seq, deadline, now, describe)?
        };
        let mut changes = if describe {
            Vec::with_capacity(batch.writes.len() + adoption.observed.len())
        } else {
            Vec::new()
        };
        for write in std::mem::take(&mut batch.writes) {
            self.telemetry(&write, context);
            if !describe {
                continue;
            }
            match write {
                StorageWrite::Conversation { value } => {
                    changes.push(CommitChange::Conversation { value })
                }
                StorageWrite::Entry { value } => changes.push(CommitChange::Entry { value }),
                StorageWrite::Task { value } => changes.push(CommitChange::Task { value }),
                StorageWrite::Submission { value } => {
                    changes.push(CommitChange::Submission { value })
                }
                _ => {}
            }
        }
        if !describe {
            return Ok(None);
        }
        let tables = changes.len();
        changes.extend(adoption.observed);
        let (documents, followers, hosts) = {
            let observers = lock(&self.observers);
            (
                observers
                    .documents
                    .iter()
                    .map(|(id, doc)| (*id, doc.clone()))
                    .collect::<Vec<_>>(),
                observers
                    .follows
                    .iter()
                    .map(|(id, call)| (*id, call.clone()))
                    .collect::<Vec<_>>(),
                observers
                    .commits
                    .iter()
                    .map(|(id, call)| (*id, call.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        for (id, document) in documents {
            if !document.advance(self, &changes, context) {
                self.detach_document(id);
            }
        }
        let publication = if let Some(seq) = seq {
            let observed = Arc::new(CommitPublication { seq, changes });
            if !hosts.is_empty() {
                let mut combined;
                let stored =
                    if adoption.held_adjustments.is_empty() && adoption.stored_only.is_empty() {
                        &*observed
                    } else {
                        combined = (*observed).clone();
                        for (index, folded) in adoption.held_adjustments {
                            if let Some(CommitChange::Document(DocumentCommitChange::Change {
                                ops,
                                ..
                            })) = combined.changes.get_mut(tables + index)
                            {
                                *ops = folded;
                            }
                        }
                        combined.changes.extend(adoption.stored_only);
                        &combined
                    };
                for (id, host) in hosts {
                    if !host.advance(self, stored, context) {
                        lock(&self.observers).commits.shift_remove(&id);
                    }
                }
            }
            if observed.changes.is_empty() {
                return Ok(None);
            }
            ObservedCommit::Stored(observed)
        } else {
            ObservedCommit::Memory(changes)
        };
        let mut failed = None;
        for (id, listener) in followers {
            if !lock(&self.observers).follows.contains_key(&id) {
                continue;
            }
            match catch_unwind(AssertUnwindSafe(|| listener(&publication, context))) {
                Ok(Err(error)) => {
                    failed.get_or_insert(error);
                }
                Err(error) => {
                    failed.get_or_insert_with(|| Error::invalid(panic_message(&*error)));
                }
                Ok(Ok(())) => {}
            }
        }
        Ok(failed)
    }

    fn telemetry(&self, write: &StorageWrite, context: &Context) {
        let emitted = catch_unwind(AssertUnwindSafe(|| match write {
            StorageWrite::Entry { value } => context.telemetry().add_event(
                "pi.durable.entry",
                [
                    ("entry.id".into(), AttributeValue::Number(value.id as f64)),
                    (
                        "entry.kind".into(),
                        AttributeValue::String(value.kind.clone()),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            StorageWrite::Submission { value }
                if matches!(
                    value.status,
                    SubmissionStatus::Done | SubmissionStatus::Unanswered
                ) =>
            {
                let mut attributes = indexmap::IndexMap::from([
                    (
                        "submission.id".into(),
                        AttributeValue::Number(value.id as f64),
                    ),
                    ("status".into(), AttributeValue::from(value.status.as_str())),
                ]);
                if let Some(answer) = value.answer {
                    attributes.insert(
                        "answer.entry_id".into(),
                        AttributeValue::Number(answer as f64),
                    );
                }
                context
                    .telemetry()
                    .add_event("pi.durable.submission", attributes);
            }
            _ => {}
        }));
        if let Err(error) = emitted {
            self.report(ReportKind::Telemetry, &panic_message(&*error));
        }
    }

    fn detach_document(&self, id: u64) {
        let detached = lock(&self.observers).remove_document(id);
        if let Some(document) = detached {
            document.deactivate();
            let now = self.now();
            lock(&self.state).documents.touch(&document.key, now);
            self.schedule_maintenance();
        }
    }

    fn begin_close(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(session) = self.weak.upgrade() else {
            return;
        };
        let (listeners, documents) = {
            let mut observers = lock(&self.observers);
            (
                std::mem::take(&mut observers.close),
                std::mem::take(&mut observers.documents),
            )
        };
        let failure = self.failure();
        for document in documents.into_values() {
            document.end(self, failure.as_deref());
        }
        for listener in listeners.into_values() {
            if let Err(error) = catch_unwind(AssertUnwindSafe(|| listener())) {
                self.report(ReportKind::Listener, &panic_message(&*error));
            }
        }
        self.cancel_maintenance();
        tokio::spawn(async move {
            session.finish_close().await;
        });
    }

    async fn finish_close(self: Arc<Self>) {
        let _line = self.line.lock().await;
        self.drained().await;
        let context = Context::background();
        let mut error = None;
        if self.failure().is_none() {
            if !self.options.read_only
                && let Some(on_close) = &self.options.closing_writes
            {
                let writes = catch_unwind(AssertUnwindSafe(|| on_close()))
                    .unwrap_or_else(|error| Err(Error::invalid(panic_message(&*error))));
                match writes {
                    Ok(writes) if !writes.is_empty() => {
                        let batch = PreparedBatch {
                            writes,
                            silent: true,
                            ..PreparedBatch::default()
                        };
                        if let Err(failed) = self.flush(batch, &context).await {
                            error = Some(failed.to_string());
                        }
                    }
                    Err(failed) => {
                        self.fail(failed.to_string());
                        error = Some(failed.to_string());
                    }
                    _ => {}
                }
            }
            if self.failure().is_none()
                && let Err(failed) = self.flush_holds(None, &context).await
            {
                error = Some(failed.to_string());
            }
        }
        let commits = {
            let mut observers = lock(&self.observers);
            observers.follows.clear();
            std::mem::take(&mut observers.commits)
        };
        let failure = self.failure();
        for commit in commits.into_values() {
            commit.end(
                &self,
                failure.clone().map_or(WatchEnd::Ended, WatchEnd::Failed),
            );
        }
        *lock(&self.state) = State::default();
        let storage = self.storage.clone();
        let closing = tokio::task::spawn_blocking(move || storage.close()).await;
        let closing = match closing {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(cause) = closing {
            self.fail(cause.clone());
            error = Some(cause);
        }
        self.closed.send_replace(Some((
            self.failure()
                .map_or(SessionEnd::Closed, SessionEnd::Failed),
            error,
        )));
    }
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn finite_millis(duration: Duration) -> Option<u64> {
    u64::try_from(duration.as_millis()).ok()
}
fn wall_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(finite_millis)
        .unwrap_or(0)
}
fn panic_message(value: &(dyn Any + Send)) -> String {
    value
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            value
                .downcast_ref::<&str>()
                .map(|value| (*value).to_owned())
        })
        .unwrap_or_else(|| "callback panicked".into())
}
