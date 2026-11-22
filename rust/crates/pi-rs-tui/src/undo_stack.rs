//! Detached editor state snapshots.

#[derive(Debug)]
pub struct UndoStack<S> {
    snapshots: Vec<S>,
}

impl<S> Default for UndoStack<S> {
    fn default() -> Self {
        Self {
            snapshots: Vec::new(),
        }
    }
}

impl<S> UndoStack<S> {
    /// Store an owned snapshot; callers can move a state instead of cloning it.
    pub fn push(&mut self, snapshot: S) {
        self.snapshots.push(snapshot);
    }

    pub fn pop(&mut self) -> Option<S> {
        self.snapshots.pop()
    }

    pub fn clear(&mut self) {
        self.snapshots.clear();
    }

    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}
