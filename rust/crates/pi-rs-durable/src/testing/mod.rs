//! Reusable contract suites and deterministic storage workloads.

pub mod env_conformance;
pub mod storage_benchmark;
pub mod storage_conformance;

use crate::ids::Seq;
use crate::storage::{CommitResult, Storage, StorageError, StorageResult, StorageWrite};

/// Commit one batch through the public storage seam and return its durability receipt.
pub fn commit_writes(
    storage: &(impl Storage + ?Sized),
    writes: Vec<StorageWrite>,
) -> StorageResult<Seq> {
    let mut receipt = None;
    let mut invalid = false;
    let result = storage.commit(&[writes], &mut |index, seq| {
        invalid |= index != 0 || receipt.replace(seq).is_some();
    })?;
    match result {
        CommitResult::Refused { refused, .. } => Err(refused),
        CommitResult::Stored { stored: 1 } if !invalid => receipt
            .ok_or_else(|| StorageError::Backend("storage omitted its durability receipt".into())),
        CommitResult::Stored { .. } => Err(StorageError::Backend(
            "invalid result for a one-batch storage commit".into(),
        )),
    }
}
