use crate::{
    ConversationId, ConversationRecord, TaskId, TaskRecord, TaskState,
    session::{Result, Transaction},
};
use std::collections::{BTreeMap, HashSet};

#[derive(Default)]
pub(super) struct Ownership {
    pub tasks: BTreeMap<TaskId, TaskRecord>,
    conversations: BTreeMap<ConversationId, ConversationRecord>,
}
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(super) enum Scope {
    Task(TaskId),
    Conversation(ConversationId),
}
impl Ownership {
    pub async fn read(tx: &Transaction<'_>) -> Result<Self> {
        let mut data = Self::default();
        let mut cursor = None;
        loop {
            let page = tx
                .scan_conversations(&Default::default(), 256, cursor.as_ref())
                .await?;
            data.conversations
                .extend(page.items.into_iter().map(|v| (v.id, v)));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        for status in [
            crate::TaskStatus::Pending,
            crate::TaskStatus::Running,
            crate::TaskStatus::Waiting,
            crate::TaskStatus::Sleeping,
            crate::TaskStatus::Completing,
        ] {
            let mut cursor = None;
            loop {
                let page = tx
                    .scan_tasks(
                        &crate::TaskQuery {
                            status: Some(status),
                            ..Default::default()
                        },
                        256,
                        cursor.as_ref(),
                    )
                    .await?;
                data.tasks.extend(page.items.into_iter().map(|v| (v.id, v)));
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(data)
    }
    pub fn overlay(&mut self, tx: &Transaction<'_>) {
        for c in tx.staged_conversations() {
            self.conversations.insert(c.id, c.clone());
        }
        for t in tx.staged_tasks() {
            if matches!(t.state, TaskState::Terminal { .. }) {
                self.tasks.remove(&t.id);
            } else {
                self.tasks.insert(t.id, t.clone());
            }
        }
    }
    pub fn reaches(&self, root: Option<Scope>, from: Scope, background: bool) -> bool {
        let mut seen = HashSet::new();
        let mut here = from;
        while seen.insert(here) {
            if Some(here) == root {
                return true;
            }
            here = match here {
                Scope::Task(id) => {
                    let Some(task) = self.tasks.get(&id) else {
                        return false;
                    };
                    if task.background && !background {
                        return false;
                    }
                    task.owner
                        .map_or(Scope::Conversation(task.conversation_id), Scope::Task)
                }
                Scope::Conversation(id) => {
                    let Some(owner) = self.conversations.get(&id).and_then(|c| c.owner) else {
                        return root.is_none();
                    };
                    Scope::Task(owner.task_id)
                }
            };
        }
        false
    }
    pub fn wait_cycle(&self, waiter: TaskId, start: TaskId) -> Option<Vec<TaskId>> {
        let mut paths = std::collections::HashMap::from([(start, None)]);
        let mut pending = vec![start];
        while let Some(id) = pending.pop() {
            if id == waiter {
                let mut path = vec![id];
                let mut cursor = id;
                while let Some(Some(parent)) = paths.get(&cursor) {
                    path.push(*parent);
                    cursor = *parent;
                }
                path.reverse();
                return Some(path);
            }
            let Some(task) = self.tasks.get(&id) else {
                continue;
            };
            let waiting: &[TaskId] = match &task.state {
                TaskState::Waiting { on, .. } if !task.abort_requested => on,
                _ => &[],
            };
            let owned = self.tasks.keys().copied().filter(|child| {
                *child != id && self.reaches(Some(Scope::Task(id)), Scope::Task(*child), false)
            });
            for child in waiting.iter().copied().chain(owned) {
                if let std::collections::hash_map::Entry::Vacant(entry) = paths.entry(child) {
                    entry.insert(Some(id));
                    pending.push(child);
                }
            }
        }
        None
    }
    pub fn has_children(&self, id: TaskId) -> bool {
        self.tasks.keys().any(|&other| {
            other != id && self.reaches(Some(Scope::Task(id)), Scope::Task(other), false)
        })
    }
}
