use std::{
    collections::{HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use indexmap::IndexMap;
use pi_rs_chord::{
    context::Context,
    delta::Op,
    json::JsonValue,
    watch::{Watch, WatchCall, WatchEnd},
};
use serde_json::Value;

use crate::{documents::address_id, errors::BoxError, ids::DocumentId, storage_types::*};

use super::{Inner, ReportKind, lock};

pub type DocumentWatch = Watch;

type Advance<T> = dyn Fn(&T, &Context) -> std::result::Result<(), BoxError> + Send + Sync;
type End = dyn Fn(WatchEnd) + Send + Sync;

/// Synchronous observer callbacks run after adoption; they must not call back into the mutation line.
pub struct Observer<T> {
    pub advance: Arc<Advance<T>>,
    pub end: Arc<End>,
}

impl<T> Observer<T> {
    pub fn new(
        advance: impl Fn(&T, &Context) -> std::result::Result<(), BoxError> + Send + Sync + 'static,
        end: impl Fn(WatchEnd) + Send + Sync + 'static,
    ) -> Self {
        Self {
            advance: Arc::new(advance),
            end: Arc::new(end),
        }
    }
}

pub type CommitObserver = Observer<CommitPublication>;
pub type DocumentObserver = Observer<WatchCall<Arc<JsonObject>>>;

/// Detached by dropping, or by calling `detach` early. Detachment never sends an end event.
pub struct Subscription {
    detach: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    pub(crate) fn new(detach: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            detach: Some(Box::new(detach)),
        }
    }

    pub fn detach(&mut self) {
        if let Some(detach) = self.detach.take() {
            detach();
        }
    }

    pub(crate) fn keep(mut self) {
        drop(self.detach.take());
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.detach();
    }
}

pub struct Observation<T> {
    pub value: T,
    pub subscription: Subscription,
}

pub(crate) struct Listener<T> {
    active: AtomicBool,
    observer: Observer<T>,
}

impl<T> Listener<T> {
    pub fn new(observer: Observer<T>) -> Self {
        Self {
            active: AtomicBool::new(true),
            observer,
        }
    }
    pub fn deactivate(&self) -> bool {
        self.active.swap(false, Ordering::AcqRel)
    }
    pub fn advance(&self, session: &Inner, value: &T, context: &Context) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let error = match catch_unwind(AssertUnwindSafe(|| (self.observer.advance)(value, context)))
        {
            Ok(Ok(())) => return self.active.load(Ordering::Acquire),
            Ok(Err(error)) => error.to_string(),
            Err(error) => super::panic_message(&*error),
        };
        session.report_in(ReportKind::Listener, &error, Some(context));
        self.end(session, WatchEnd::ListenerError(error));
        false
    }

    pub fn end(&self, session: &Inner, end: WatchEnd) {
        if self.deactivate()
            && let Err(error) = catch_unwind(AssertUnwindSafe(|| (self.observer.end)(end)))
        {
            session.report(ReportKind::Listener, &super::panic_message(&*error));
        }
    }
}

pub(crate) type Follower =
    dyn Fn(&super::ObservedCommit, &Context) -> super::Result<()> + Send + Sync;
pub(crate) type CloseListener = dyn Fn() + Send + Sync;

#[derive(Default)]
pub(crate) struct Observers {
    pub next: u64,
    pub commits: IndexMap<u64, Arc<Listener<CommitPublication>>>,
    pub follows: IndexMap<u64, Arc<Follower>>,
    pub close: IndexMap<u64, Arc<CloseListener>>,
    pub documents: IndexMap<u64, Arc<DocumentFollower>>,
    pins: HashMap<String, usize>,
}

impl Observers {
    pub fn allocate(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    pub fn pins(&self) -> HashSet<String> {
        self.pins.keys().cloned().collect()
    }
    pub fn has_documents(&self) -> bool {
        !self.documents.is_empty()
    }
    pub fn has_publications(&self) -> bool {
        !self.commits.is_empty() || !self.follows.is_empty() || self.has_documents()
    }

    pub fn insert_document(&mut self, id: u64, doc: Arc<DocumentFollower>) {
        *self.pins.entry(doc.key.clone()).or_default() += 1;
        self.documents.insert(id, doc);
    }

    pub fn remove_document(&mut self, id: u64) -> Option<Arc<DocumentFollower>> {
        let doc = self.documents.shift_remove(&id)?;
        if let Some(count) = self.pins.get_mut(&doc.key) {
            *count -= 1;
            if *count == 0 {
                self.pins.remove(&doc.key);
            }
        }
        Some(doc)
    }
}

pub(crate) enum DocumentDestination {
    Sync(Listener<WatchCall<Arc<JsonObject>>>),
    Watch(Watch),
}

struct DocumentVersion {
    id: DocumentId,
    version: u64,
    value: Arc<JsonObject>,
}

pub(crate) struct DocumentFollower {
    address: DocumentAddress,
    pub(super) key: String,
    initial: Mutex<Option<DocumentVersion>>,
    destination: DocumentDestination,
    owner_ended: AtomicBool,
}

impl DocumentFollower {
    pub fn new(
        address: DocumentAddress,
        document: &super::document_cache::LoadedDocument,
        destination: DocumentDestination,
    ) -> Self {
        let key = address_id(&address);
        let initial = Mutex::new(Some(DocumentVersion {
            id: document.record.id,
            version: document.version,
            value: document.value.clone(),
        }));
        Self {
            address,
            key,
            initial,
            destination,
            owner_ended: AtomicBool::new(false),
        }
    }

    fn call(&self, session: &Inner, call: WatchCall<Arc<JsonObject>>, context: &Context) -> bool {
        match &self.destination {
            DocumentDestination::Sync(listener) => listener.advance(session, &call, context),
            DocumentDestination::Watch(watch) => {
                let call = match call {
                    WatchCall::Value {
                        value,
                        retired,
                        ops,
                    } => WatchCall::Value {
                        value: json(&value),
                        retired,
                        ops,
                    },
                    WatchCall::Failed { error } => WatchCall::Failed { error },
                };
                watch.advance(call, context.clone());
                true
            }
        }
    }

    pub fn advance(&self, session: &Inner, changes: &[CommitChange], context: &Context) -> bool {
        if self.owner_ended.load(Ordering::Acquire) {
            return true;
        }
        let mut terminal = false;
        for change in changes {
            let call = match change {
                CommitChange::Task { value } => {
                    terminal |= self.address.scope == (DocumentScope::Task { task_id: value.id })
                        && matches!(value.state, TaskState::Terminal { .. });
                    None
                }
                CommitChange::Document(DocumentCommitChange::Retire { record, .. }) => {
                    let mut life = lock(&self.initial);
                    if life.as_ref().is_some_and(|version| version.id == record.id) {
                        life.take().map(|version| WatchCall::Value {
                            value: version.value,
                            retired: true,
                            ops: Vec::new(),
                        })
                    } else {
                        None
                    }
                }
                CommitChange::Document(DocumentCommitChange::Change {
                    record,
                    version,
                    value,
                    ops,
                    ..
                }) => {
                    if record.kind != self.address.kind
                        || record.key != self.address.key
                        || record.scope != self.address.scope
                    {
                        continue;
                    }
                    let mut life = lock(&self.initial);
                    let replace = life
                        .as_ref()
                        .is_none_or(|live| live.id != record.id || live.version != *version);
                    if !replace && ops.is_empty() {
                        continue;
                    }
                    let value = Arc::new(value.clone());
                    let ops = if replace {
                        vec![Op::Replace(json(&value))]
                    } else {
                        ops.clone()
                    };
                    *life = Some(DocumentVersion {
                        id: record.id,
                        version: *version,
                        value: value.clone(),
                    });
                    Some(WatchCall::Value {
                        value,
                        ops,
                        retired: false,
                    })
                }
                _ => None,
            };
            if let Some(call) = call
                && !self.call(session, call, context)
            {
                return false;
            }
        }
        if !terminal {
            return true;
        }
        self.owner_ended.store(true, Ordering::Release);
        match &self.destination {
            DocumentDestination::Sync(listener) => {
                listener.end(session, WatchEnd::Ended);
                false
            }
            DocumentDestination::Watch(watch) => {
                watch.end_after_delivery();
                // Still end an undelivered terminal watch if the Session closes in the meantime.
                true
            }
        }
    }

    pub fn deactivate(&self) {
        if let DocumentDestination::Sync(listener) = &self.destination {
            listener.deactivate();
        }
    }

    pub fn end(&self, session: &Inner, failure: Option<&str>) {
        let end = failure.map(|cause| WatchEnd::Failed(cause.to_owned()));
        match &self.destination {
            DocumentDestination::Sync(listener) => listener.end(
                session,
                end.unwrap_or_else(|| WatchEnd::Failed("session closed".into())),
            ),
            DocumentDestination::Watch(watch) => {
                watch.end(end.unwrap_or(WatchEnd::Ended));
            }
        }
    }
}

pub(crate) fn detach_document(session: &Weak<Inner>, id: u64) {
    if let Some(session) = session.upgrade() {
        session.detach_document(id);
    }
}

pub(crate) fn json(value: &JsonObject) -> JsonValue {
    JsonValue::from_serde(Value::Object(value.clone()))
}
