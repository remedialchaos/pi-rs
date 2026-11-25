//! Draft lifetime and prepared-revision adoption authority.
use super::*;
use crate::context::lock;
use std::{
    ops::Deref,
    sync::{Arc, Mutex, Weak},
};
#[derive(Clone, Copy, PartialEq)]
enum Status {
    Open,
    Prepared,
    Consumed,
    Aborted,
    Stale,
}
struct Authority {
    value: Revision,
    revision: u64,
    open: Vec<Weak<Mutex<Status>>>,
}
#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Mutex<Authority>>,
}
pub struct Prepared {
    data: PreparedData,
    owner: Weak<Mutex<Authority>>,
    status: Arc<Mutex<Status>>,
}
pub struct PreparedData {
    pub base: Revision,
    pub value: Revision,
    pub ops: Vec<Op>,
    pub base_revision: u64,
}
impl Deref for Prepared {
    type Target = PreparedData;
    fn deref(&self) -> &PreparedData {
        &self.data
    }
}
impl Prepared {
    pub fn abort(&self) {
        let mut status = lock(&self.status);
        if matches!(*status, Status::Open | Status::Prepared) {
            *status = Status::Aborted
        }
    }
}
pub struct Change {
    owner: Weak<Mutex<Authority>>,
    base: Option<Revision>,
    current: Option<Revision>,
    revision: u64,
    status: Arc<Mutex<Status>>,
    journal: super::draft_ops::Journal,
}
impl Tracker {
    pub fn new(value: JsonValue) -> DeltaResult<Self> {
        if value.is_primitive() {
            return Err(shape("tracked state must be an object or array"));
        }
        Self::from_revision(Revision::from_json(value)?)
    }
    pub fn from_revision(value: Revision) -> DeltaResult<Self> {
        if matches!(value.0.as_ref(), revision::Node::Scalar(_)) {
            return Err(shape("tracked state must be an object or array"));
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Authority {
                value,
                revision: 0,
                open: Vec::new(),
            })),
        })
    }
    pub fn value(&self) -> Revision {
        lock(&self.inner).value.clone()
    }
    pub fn revision(&self) -> u64 {
        lock(&self.inner).revision
    }
    pub fn begin_change(&self) -> Change {
        let mut owner = lock(&self.inner);
        owner.open.retain(|v| v.strong_count() > 0);
        let status = Arc::new(Mutex::new(Status::Open));
        owner.open.push(Arc::downgrade(&status));
        Change {
            owner: Arc::downgrade(&self.inner),
            base: Some(owner.value.clone()),
            current: Some(owner.value.clone()),
            revision: owner.revision,
            status,
            journal: super::draft_ops::Journal::default(),
        }
    }
    pub fn prepare_replace(&self, value: JsonValue) -> DeltaResult<Prepared> {
        if value.is_primitive() {
            return Err(shape("tracked state must be an object or array"));
        }
        let mut authority = lock(&self.inner);
        let next = Revision::from_json(value)?;
        let current = if authority.value.equal(&next) {
            authority.value.clone()
        } else {
            next
        };
        let status = Arc::new(Mutex::new(Status::Prepared));
        authority.open.push(Arc::downgrade(&status));
        let ops = if current.ptr_eq(&authority.value) {
            Vec::new()
        } else {
            vec![Op::Replace(current.to_json())]
        };
        Ok(Prepared {
            data: PreparedData {
                base: authority.value.clone(),
                value: current,
                ops,
                base_revision: authority.revision,
            },
            owner: Arc::downgrade(&self.inner),
            status,
        })
    }
    pub fn adopt(&self, prepared: &Prepared) -> DeltaResult<()> {
        let owner = prepared
            .owner
            .upgrade()
            .ok_or_else(|| shape("tracker has been released"))?;
        if !Arc::ptr_eq(&owner, &self.inner) {
            return Err(shape("prepared revision belongs to another tracker"));
        }
        let mut authority = lock(&self.inner);
        let mut status = lock(&prepared.status);
        if *status != Status::Prepared {
            return Err(shape("prepared revision is consumed, aborted, or stale"));
        }
        if authority.revision != prepared.base_revision || !authority.value.ptr_eq(&prepared.base) {
            *status = Status::Stale;
            return Err(shape("prepared revision is stale"));
        }
        let next = authority
            .revision
            .checked_add(1)
            .ok_or_else(|| shape("revision overflow"))?;
        authority.value = prepared.value.clone();
        authority.revision = next;
        *status = Status::Consumed;
        drop(status);
        for entry in authority.open.drain(..) {
            if let Some(other) = entry.upgrade() {
                let mut other = lock(&other);
                if matches!(*other, Status::Open | Status::Prepared) {
                    *other = Status::Stale
                }
            }
        }
        Ok(())
    }
}
impl Change {
    fn check(&self) -> DeltaResult<()> {
        if *lock(&self.status) != Status::Open {
            return Err(shape("draft is settled or stale"));
        }
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| shape("tracker has been released"))?;
        if lock(&owner).revision != self.revision {
            return Err(shape("draft is stale"));
        }
        Ok(())
    }
    pub fn value(&self) -> DeltaResult<&Revision> {
        self.check()?;
        self.current
            .as_ref()
            .ok_or_else(|| shape("draft is settled"))
    }
    pub fn apply(&mut self, op: Op) -> DeltaResult<()> {
        self.check()?;
        if let Op::Delete(path) = &op
            && let Some((_, parents)) = path.split_last()
            && matches!(
                self.value()?.at(parents)?.0.as_ref(),
                revision::Node::Array(_)
            )
        {
            return Err(shape("draft arrays cannot have deleted indices"));
        }
        let before = self.value()?.clone();
        let next = revision::apply_draft(&before, std::slice::from_ref(&op))?;
        if !next.is_container() {
            return Err(shape("tracked state must be an object or array"));
        }
        self.journal.record(&before, &next, &op)?;
        self.current = Some(next);
        Ok(())
    }
    pub fn set(&mut self, path: &[Segment], value: JsonValue) -> DeltaResult<()> {
        self.apply(Op::Set(path.to_vec(), value))
    }
    pub fn delete(&mut self, path: &[Segment]) -> DeltaResult<()> {
        self.apply(Op::Delete(path.to_vec()))
    }
    pub fn append_text(&mut self, path: &[Segment], text: JsonString) -> DeltaResult<()> {
        self.apply(Op::Append(path.to_vec(), text))
    }
    pub fn splice(
        &mut self,
        path: &[Segment],
        index: usize,
        remove: usize,
        items: Vec<JsonValue>,
    ) -> DeltaResult<()> {
        self.apply(Op::Splice(path.to_vec(), index, remove, items))
    }
    pub fn permute(&mut self, path: &[Segment], order: Vec<usize>) -> DeltaResult<()> {
        self.apply(Op::Permute(path.to_vec(), order))
    }
    /// Extending an array fills new entries with null, never sparse holes.
    pub fn resize(&mut self, path: &[Segment], length: usize) -> DeltaResult<()> {
        let JsonValue::Array(a) = self.value()?.at(path)?.to_json() else {
            return Err(path_error(path));
        };
        if length < a.len() {
            self.splice(path, length, a.len() - length, Vec::new())
        } else {
            self.splice(path, a.len(), 0, vec![JsonValue::Null; length - a.len()])
        }
    }
    pub fn prepare(&mut self) -> DeltaResult<Prepared> {
        self.check()?;
        let base = self
            .base
            .as_ref()
            .ok_or_else(|| shape("draft is settled"))?;
        let current = self
            .current
            .as_ref()
            .ok_or_else(|| shape("draft is settled"))?;
        let ops = self.journal.emit(base, current);
        let value = apply_shared(base, &ops)?;
        let base = self.base.take().ok_or_else(|| shape("draft is settled"))?;
        self.current = None;
        self.journal = super::draft_ops::Journal::default();
        *lock(&self.status) = Status::Prepared;
        Ok(Prepared {
            data: PreparedData {
                base,
                value,
                ops,
                base_revision: self.revision,
            },
            owner: self.owner.clone(),
            status: self.status.clone(),
        })
    }
    pub fn abort(&mut self) {
        let mut status = lock(&self.status);
        if matches!(*status, Status::Open | Status::Prepared) {
            *status = Status::Aborted
        }
        self.current = None;
        self.base = None;
        self.journal = super::draft_ops::Journal::default();
    }
}
/// Create, mutate and settle an isolated draft without exposing a mutable alias.
pub fn draft<F>(base: JsonValue, edit: F) -> DeltaResult<Prepared>
where
    F: FnOnce(&mut Change) -> DeltaResult<()>,
{
    let tracker = Tracker::new(base)?;
    let mut change = tracker.begin_change();
    edit(&mut change)?;
    change.prepare()
}
