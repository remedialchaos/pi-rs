use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::{ids::*, storage_types::*};

/// Hot records, updated only at publication. An in-flight read cannot overwrite a newer publication.
#[derive(Default)]
pub(crate) struct CachedStorage {
    conversations: Lru<Arc<ConversationRecord>, 4096>,
    live: HashMap<TaskId, Arc<TaskRecord>>,
    ended: Lru<Arc<TaskRecord>, 256>,
    submissions: Lru<Arc<SubmissionRecord>, 256>,
    waits: HashMap<TaskId, (Vec<TaskId>, bool)>,
    awaited: HashMap<TaskId, Awaited>,
    reads: HashMap<u64, ReadVersion>,
}

#[derive(Default)]
struct Awaited {
    count: usize,
    record: Option<Arc<TaskRecord>>,
}

#[derive(Default)]
struct ReadVersion {
    count: usize,
    version: u64,
}

impl CachedStorage {
    pub fn conversation(&mut self, id: ConversationId) -> Option<Arc<ConversationRecord>> {
        self.conversations.get(id).cloned()
    }

    pub fn task(&mut self, id: TaskId) -> Option<Arc<TaskRecord>> {
        self.live
            .get(&id)
            .cloned()
            .or_else(|| {
                self.awaited
                    .get(&id)
                    .and_then(|awaited| awaited.record.clone())
            })
            .or_else(|| self.ended.get(id).cloned())
    }

    pub fn submission(&mut self, id: SubmissionId) -> Option<Arc<SubmissionRecord>> {
        self.submissions.get(id).cloned()
    }

    pub fn start_read(&mut self, id: u64) -> u64 {
        let read = self.reads.entry(id).or_default();
        read.count += 1;
        read.version
    }

    pub fn end_read(&mut self, id: u64, version: u64) -> bool {
        let Some(read) = self.reads.get_mut(&id) else {
            return false;
        };
        let fresh = read.version == version;
        read.count -= 1;
        if read.count == 0 {
            self.reads.remove(&id);
        }
        fresh
    }

    pub fn keep_conversation(&mut self, value: Arc<ConversationRecord>) {
        self.conversations.put(value.id, value);
    }

    pub fn keep_task(&mut self, value: Arc<TaskRecord>) {
        let id = value.id;
        if matches!(value.state, TaskState::Terminal { .. }) {
            self.live.remove(&id);
            if let Some(awaited) = self.awaited.get_mut(&id) {
                awaited.record = Some(value);
            } else {
                self.ended.put(id, value);
            }
        } else {
            self.ended.remove(id);
            self.live.insert(id, value);
        }
    }

    fn wrote(&mut self, id: u64) {
        if let Some(read) = self.reads.get_mut(&id) {
            read.version += 1;
        }
    }

    pub fn published(&mut self, writes: &[StorageWrite]) {
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => {
                    self.wrote(value.id);
                    self.keep_conversation(Arc::new(value.clone()));
                }
                StorageWrite::Task { value } => {
                    self.wrote(value.id);
                    self.follow_wait(value);
                    self.keep_task(Arc::new(value.clone()));
                }
                StorageWrite::Submission { value } => {
                    self.wrote(value.id);
                    self.submissions.put(value.id, Arc::new(value.clone()));
                }
                _ => {}
            }
        }
    }

    fn follow_wait(&mut self, task: &TaskRecord) {
        if let TaskState::Waiting { on, .. } = &task.state {
            if let Some((previous, _)) = self.waits.remove(&task.id) {
                self.release(&previous);
            }
            for &id in on {
                let target = self.awaited.entry(id).or_insert_with(|| Awaited {
                    count: 0,
                    record: self.ended.remove(id),
                });
                target.count += 1;
            }
            self.waits.insert(task.id, (on.clone(), false));
        } else if let Some(wait) = self.waits.get_mut(&task.id) {
            if !wait.1 && !matches!(task.state, TaskState::Terminal { .. }) {
                wait.1 = true;
            } else if let Some((previous, _)) = self.waits.remove(&task.id) {
                self.release(&previous);
            }
        }
    }

    fn release(&mut self, on: &[TaskId]) {
        for &id in on {
            if let Some(target) = self.awaited.get_mut(&id) {
                target.count -= 1;
                if target.count == 0
                    && let Some(target) = self.awaited.remove(&id)
                    && let Some(record) = target.record
                {
                    self.ended.put(id, record);
                }
            }
        }
    }
}

/// Compact bounded LRU with logarithmic eviction and no scan on cache hits.
pub(crate) struct Lru<T, const CAPACITY: usize> {
    latest: u64,
    by_id: HashMap<u64, (u64, T)>,
    by_age: BTreeMap<u64, u64>,
}

impl<T, const CAPACITY: usize> Default for Lru<T, CAPACITY> {
    fn default() -> Self {
        Self {
            latest: 0,
            by_id: HashMap::new(),
            by_age: BTreeMap::new(),
        }
    }
}

impl<T, const CAPACITY: usize> Lru<T, CAPACITY> {
    pub fn peek(&self, id: u64) -> Option<&T> {
        self.by_id.get(&id).map(|(_, value)| value)
    }

    pub fn get(&mut self, id: u64) -> Option<&T> {
        let (age, value) = self.by_id.get_mut(&id)?;
        self.by_age.remove(age);
        self.latest += 1;
        *age = self.latest;
        self.by_age.insert(*age, id);
        Some(value)
    }

    pub fn remove(&mut self, id: u64) -> Option<T> {
        let (age, value) = self.by_id.remove(&id)?;
        self.by_age.remove(&age);
        Some(value)
    }

    pub fn put(&mut self, id: u64, value: T) {
        self.latest += 1;
        if let Some((age, _)) = self.by_id.insert(id, (self.latest, value)) {
            self.by_age.remove(&age);
        }
        self.by_age.insert(self.latest, id);
        if self.by_id.len() > CAPACITY
            && let Some((_, id)) = self.by_age.pop_first()
        {
            self.by_id.remove(&id);
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }
}
