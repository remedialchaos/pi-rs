//! In-memory owner claims and replay exclusion.
use super::table::invalid;
use crate::Result;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Debug)]
pub struct Claimant {
    id: u64,
    pub label: String,
}
impl Claimant {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            label: label.into(),
        }
    }
}
#[derive(Default)]
struct State {
    holders: HashMap<String, Claimant>,
    locked: bool,
}
#[derive(Clone, Default)]
pub struct Claims(Arc<Mutex<State>>);
impl Claims {
    pub fn held(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .holders
            .is_empty()
    }
    pub fn table(&self, table: &str, claimant: &Claimant) -> Result<()> {
        self.claim(format!("table {}", serde_json::to_string(table)?), claimant)
    }
    pub fn documents(&self, owner: &str, namespace: &str, claimant: &Claimant) -> Result<()> {
        self.claim(
            format!("documents {}", serde_json::to_string(&[owner, namespace])?),
            claimant,
        )
    }
    pub fn assert_table(&self, table: &str, claimant: &Claimant) -> Result<()> {
        self.assert(
            &format!("table {}", serde_json::to_string(table)?),
            claimant,
        )
    }
    fn assert(&self, key: &str, claimant: &Claimant) -> Result<()> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Self::assert_in(&s, key, claimant)
    }
    fn assert_in(s: &State, key: &str, claimant: &Claimant) -> Result<()> {
        if s.locked {
            return Err(invalid(format!(
                "store: {key} cannot be claimed while a batch applies"
            )));
        }
        if let Some(holder) = s.holders.get(key)
            && holder.id != claimant.id
        {
            return Err(invalid(format!(
                "store: {key} is claimed by {}",
                holder.label
            )));
        }
        Ok(())
    }
    fn claim(&self, key: String, claimant: &Claimant) -> Result<()> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Self::assert_in(&s, &key, claimant)?;
        s.holders.insert(key, claimant.clone());
        Ok(())
    }
    pub fn release(&self, claimant: &Claimant) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .holders
            .retain(|_, c| c.id != claimant.id);
    }
    pub(crate) fn lock_replay(&self) -> Result<ReplayLock> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.locked || !s.holders.is_empty() {
            return Err(invalid("store: replay refused while owners hold claims"));
        }
        s.locked = true;
        Ok(ReplayLock(self.clone()))
    }
}
pub(crate) struct ReplayLock(Claims);
impl Drop for ReplayLock {
    fn drop(&mut self) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).locked = false;
    }
}
