//! Deterministic workloads seeded through the same Storage API as a Session.

use pi_rs_chord::{delta::Op, json::JsonValue};
use serde_json::{Value, json};

use crate::ids::{ConversationId, DocumentId, EntryId, ROOT_CONVERSATION_ID, Seq, TaskId};
use crate::storage::*;

use super::commit_writes;

#[derive(Clone, Copy, Debug)]
pub struct StorageBenchmarkScale {
    pub name: &'static str,
    pub entry_count: usize,
    pub task_count: usize,
    pub document_count: usize,
}

pub const STORAGE_MEMORY_SCALES: &[StorageBenchmarkScale] = &[
    StorageBenchmarkScale {
        name: "1k",
        entry_count: 1_000,
        task_count: 200,
        document_count: 200,
    },
    StorageBenchmarkScale {
        name: "10k",
        entry_count: 10_000,
        task_count: 2_000,
        document_count: 2_000,
    },
];
pub const TIMING_SCALE: StorageBenchmarkScale = StorageBenchmarkScale {
    name: "timing",
    entry_count: 1_000,
    task_count: 300,
    document_count: 300,
};
const REPLAY_TAILS: [usize; 4] = [0, 16, 128, 1024];
const HISTORY_SEGMENT_LENGTH: u64 = 128;
const FORK_DEPTH: usize = 8;
const ENTRIES_PER_FORK: usize = 32;
const BATCH_SIZE: usize = 100;

pub const fn storage_benchmark_primary_record_count(scale: StorageBenchmarkScale) -> usize {
    1 + scale.entry_count
        + scale.task_count
        + scale.document_count
        + REPLAY_TAILS.len()
        + 1
        + FORK_DEPTH * (1 + ENTRIES_PER_FORK)
}

#[derive(Clone, Debug)]
pub struct StorageBenchmarkDataset {
    pub first_entry_id: EntryId,
    pub filtered_task_count: usize,
    pub exact_document_id: DocumentId,
    pub exact_document_key: String,
    pub replay_document_ids: [DocumentId; REPLAY_TAILS.len()],
    pub historical_document_id: DocumentId,
    pub ancient_at: Seq,
    pub recent_at: Seq,
    pub deepest_conversation_id: ConversationId,
    pub ancestor_head_entry_id: EntryId,
}

fn task(id: TaskId, index: u64) -> TaskRecord {
    let checkpoint = || json!({"index":index, "payload":"x".repeat(64)});
    TaskRecord {
        trace: None,
        links: None,
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: if index.is_multiple_of(4) {
            "benchmark.filtered"
        } else {
            "benchmark.other"
        }
        .into(),
        version: 1,
        input: json!({"index":index}),
        owner: None,
        background: index.is_multiple_of(5),
        abort_requested: index.is_multiple_of(7),
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        memos: None,
        state: match index % 3 {
            0 => TaskState::Pending {
                checkpoint: checkpoint(),
                wake: None,
            },
            1 => TaskState::Running {
                checkpoint: checkpoint(),
                wake: None,
            },
            _ => TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: json!({"index": index}),
                },
            },
        },
    }
}

fn entry(
    id: EntryId,
    conversation_id: ConversationId,
    kind: &str,
    data: Option<Value>,
    head: Option<EntryId>,
) -> StorageWrite {
    StorageWrite::Entry {
        value: EntryRecord {
            id,
            conversation_id,
            kind: kind.into(),
            model: None,
            data,
            head,
            edits: None,
            by_task_id: None,
        },
    }
}

fn seed_root(storage: &dyn Storage) -> StorageResult<()> {
    commit_writes(
        storage,
        vec![StorageWrite::Conversation {
            value: ConversationRecord {
                id: ROOT_CONVERSATION_ID,
                parent: None,
                owner: None,
            },
        }],
    )?;
    Ok(())
}

fn document(
    id: DocumentId,
    kind: &str,
    key: Option<String>,
    rewinds: bool,
    value: JsonObject,
) -> StorageWrite {
    let (scope, history, fork) = if rewinds {
        (
            DocumentScope::Conversation {
                conversation_id: ROOT_CONVERSATION_ID,
            },
            Some(DocumentHistory::Rewindable),
            Some(DocumentFork::AsOf),
        )
    } else {
        (DocumentScope::Session, None, None)
    };
    StorageWrite::DocumentCreate {
        record: DocumentCreate {
            id,
            kind: kind.into(),
            scope,
            key,
            history,
            fork,
        },
        content: DocumentContent::Base { version: 1, value },
    }
}
fn count_value(count: u64) -> JsonObject {
    [("count".into(), json!(count))].into_iter().collect()
}
fn count_delta(count: u64) -> StorageResult<Op> {
    Op::from_json(&JsonValue::from_serde(json!(["s", ["count"], count])))
        .map_err(|error| StorageError::Backend(error.to_string()))
}

fn batches(
    storage: &dyn Storage,
    count: usize,
    mut create: impl FnMut(usize) -> StorageResult<StorageWrite>,
) -> StorageResult<()> {
    for start in (0..count).step_by(BATCH_SIZE) {
        let writes = (start..(start + BATCH_SIZE).min(count))
            .map(&mut create)
            .collect::<StorageResult<Vec<_>>>()?;
        commit_writes(storage, writes)?;
    }
    Ok(())
}

pub fn seed_storage_benchmark(
    storage: &dyn Storage,
    scale: StorageBenchmarkScale,
) -> StorageResult<StorageBenchmarkDataset> {
    if scale.entry_count == 0 || scale.document_count == 0 {
        return Err(StorageError::Request(
            "Benchmark scale must create entries and documents".into(),
        ));
    }
    seed_root(storage)?;
    let first_entry_id = storage.mint_id()?;
    let entry_payload = "x".repeat(96);
    batches(storage, scale.entry_count, |index| {
        let id = if index == 0 {
            first_entry_id
        } else {
            storage.mint_id()?
        };
        Ok(entry(
            id,
            ROOT_CONVERSATION_ID,
            "benchmark.entry",
            Some(json!({"index":index, "text":format!("entry-{index}-{entry_payload}")})),
            (index == 0).then_some(id),
        ))
    })?;
    batches(storage, scale.task_count, |index| {
        Ok(StorageWrite::Task {
            value: task(storage.mint_id()?, index as u64),
        })
    })?;
    let mut exact_document_id = 0;
    let document_payload = "x".repeat(128);
    batches(storage, scale.document_count, |index| {
        let id = storage.mint_id()?;
        exact_document_id = id;
        let value = [
            ("index".into(), json!(index)),
            ("text".into(), json!(document_payload)),
        ]
        .into_iter()
        .collect();
        Ok(document(
            id,
            "benchmark.family",
            Some(format!("key-{index}")),
            false,
            value,
        ))
    })?;
    let mut replay_document_ids = [0; REPLAY_TAILS.len()];
    let replay_writes = replay_document_ids
        .iter_mut()
        .zip(REPLAY_TAILS)
        .map(|(slot, tail)| {
            *slot = storage.mint_id()?;
            let value = [
                ("count".into(), json!(0)),
                ("text".into(), json!("x".repeat(64))),
            ]
            .into_iter()
            .collect();
            Ok(document(
                *slot,
                "benchmark.replay",
                Some(tail.to_string()),
                true,
                value,
            ))
        })
        .collect::<StorageResult<Vec<_>>>()?;
    commit_writes(storage, replay_writes)?;
    for count in 1..=REPLAY_TAILS[REPLAY_TAILS.len() - 1] {
        let op = count_delta(count as u64)?;
        let writes = replay_document_ids
            .iter()
            .zip(REPLAY_TAILS)
            .filter(|&(_, tail)| count <= tail)
            .map(|(&id, _)| StorageWrite::DocumentChange {
                id,
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![op.clone()],
                },
            })
            .collect();
        commit_writes(storage, writes)?;
    }
    let historical_document_id = storage.mint_id()?;
    let mut ancient_at = commit_writes(
        storage,
        vec![document(
            historical_document_id,
            "benchmark.history",
            None,
            true,
            count_value(0),
        )],
    )?;
    let revise = |count| -> StorageResult<Seq> {
        commit_writes(
            storage,
            vec![StorageWrite::DocumentChange {
                id: historical_document_id,
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![count_delta(count)?],
                },
            }],
        )
    };
    for count in 1..=HISTORY_SEGMENT_LENGTH {
        ancient_at = revise(count)?;
    }
    let mut recent_at = commit_writes(
        storage,
        vec![StorageWrite::DocumentChange {
            id: historical_document_id,
            content: DocumentContent::Base {
                version: 1,
                value: count_value(HISTORY_SEGMENT_LENGTH),
            },
        }],
    )?;
    for count in HISTORY_SEGMENT_LENGTH + 1..=HISTORY_SEGMENT_LENGTH * 2 {
        recent_at = revise(count)?;
    }

    let mut parent = ConversationParent {
        conversation_id: ROOT_CONVERSATION_ID,
        at: first_entry_id,
    };
    for depth in 0..FORK_DEPTH {
        let id = storage.mint_id()?;
        commit_writes(
            storage,
            vec![StorageWrite::Conversation {
                value: ConversationRecord {
                    id,
                    parent: Some(parent),
                    owner: None,
                },
            }],
        )?;
        parent.conversation_id = id;
        batches(storage, ENTRIES_PER_FORK, |index| {
            let entry_id = storage.mint_id()?;
            parent.at = entry_id;
            Ok(entry(
                entry_id,
                id,
                "benchmark.fork",
                Some(json!({"depth":depth,"index":index})),
                None,
            ))
        })?;
    }
    Ok(StorageBenchmarkDataset {
        first_entry_id,
        filtered_task_count: scale.task_count.div_ceil(60).min(50),
        exact_document_id,
        exact_document_key: format!("key-{}", scale.document_count - 1),
        replay_document_ids,
        historical_document_id,
        ancient_at,
        recent_at,
        deepest_conversation_id: parent.conversation_id,
        ancestor_head_entry_id: first_entry_id,
    })
}

#[derive(Clone, Copy)]
pub struct StorageReadBenchmark {
    pub name: &'static str,
    /// `None` means the expected record did not exist.
    pub run: fn(&dyn Storage, &StorageBenchmarkDataset) -> StorageResult<Option<u64>>,
    pub expected: fn(&StorageBenchmarkDataset) -> u64,
}

fn scan_entries(
    storage: &dyn Storage,
    conversation_id: ConversationId,
) -> StorageResult<Option<u64>> {
    Ok(Some(
        storage
            .scan_entries(
                &EntryQuery {
                    conversation_id,
                    ..EntryQuery::default()
                },
                100,
                None,
            )?
            .items
            .len() as u64,
    ))
}
fn count_at(
    storage: &dyn Storage,
    id: DocumentId,
    at: DocumentPoint,
) -> StorageResult<Option<u64>> {
    Ok(storage
        .document(id, at)?
        .and_then(|stored| stored.value.get("count").and_then(Value::as_u64)))
}

pub const STORAGE_READ_BENCHMARKS: &[StorageReadBenchmark] = &[
    StorageReadBenchmark {
        name: "exact entry lookup",
        run: |storage, data| {
            Ok(storage
                .entry(data.first_entry_id)?
                .map(|stored| stored.entry.id))
        },
        expected: |data| data.first_entry_id,
    },
    StorageReadBenchmark {
        name: "entry page scan (100)",
        run: |storage, _| scan_entries(storage, ROOT_CONVERSATION_ID),
        expected: |_| 100,
    },
    StorageReadBenchmark {
        name: "filtered task scan (50)",
        run: |storage, _| {
            Ok(Some(
                storage
                    .scan_tasks(
                        &TaskQuery {
                            kind: Some("benchmark.filtered".into()),
                            status: Some(TaskStatus::Pending),
                            background: Some(true),
                            ..TaskQuery::default()
                        },
                        50,
                        None,
                    )?
                    .items
                    .len() as u64,
            ))
        },
        expected: |data| data.filtered_task_count as u64,
    },
    StorageReadBenchmark {
        name: "exact document address among many",
        run: |storage, data| {
            Ok(storage
                .find_document(
                    &DocumentAddress {
                        kind: "benchmark.family".into(),
                        key: Some(data.exact_document_key.clone()),
                        scope: DocumentScope::Session,
                    },
                    DocumentPoint::Current,
                )?
                .map(|record| record.id))
        },
        expected: |data| data.exact_document_id,
    },
    StorageReadBenchmark {
        name: "document replay tail (0)",
        run: |storage, data| count_at(storage, data.replay_document_ids[0], DocumentPoint::Current),
        expected: |_| 0,
    },
    StorageReadBenchmark {
        name: "document replay tail (16)",
        run: |storage, data| count_at(storage, data.replay_document_ids[1], DocumentPoint::Current),
        expected: |_| 16,
    },
    StorageReadBenchmark {
        name: "document replay tail (128)",
        run: |storage, data| count_at(storage, data.replay_document_ids[2], DocumentPoint::Current),
        expected: |_| 128,
    },
    StorageReadBenchmark {
        name: "document replay tail (1024)",
        run: |storage, data| count_at(storage, data.replay_document_ids[3], DocumentPoint::Current),
        expected: |_| 1024,
    },
    StorageReadBenchmark {
        name: "ancient historical read before newer base",
        run: |storage, data| {
            count_at(
                storage,
                data.historical_document_id,
                DocumentPoint::Seq(data.ancient_at),
            )
        },
        expected: |_| HISTORY_SEGMENT_LENGTH,
    },
    StorageReadBenchmark {
        name: "recent historical read after newer base",
        run: |storage, data| {
            count_at(
                storage,
                data.historical_document_id,
                DocumentPoint::Seq(data.recent_at),
            )
        },
        expected: |_| 2 * HISTORY_SEGMENT_LENGTH,
    },
    StorageReadBenchmark {
        name: "fork-depth history scan (100)",
        run: |storage, data| scan_entries(storage, data.deepest_conversation_id),
        expected: |_| 100,
    },
    StorageReadBenchmark {
        name: "fork-depth head lookup",
        run: |storage, data| {
            Ok(storage
                .find_latest_head_marker(data.deepest_conversation_id, None)?
                .map(|record| record.id))
        },
        expected: |data| data.ancestor_head_entry_id,
    },
];

pub struct StorageWriteBenchmark {
    pub name: &'static str,
    pub expected: usize,
    pub run: fn(&dyn Storage) -> StorageResult<usize>,
}

pub fn seed_storage_write_benchmark(storage: &dyn Storage) -> StorageResult<()> {
    seed_root(storage)?;
    batches(storage, 100, |index| {
        Ok(entry(
            storage.mint_id()?,
            ROOT_CONVERSATION_ID,
            "benchmark.baseline",
            Some(json!({"index":index})),
            None,
        ))
    })
}

pub const STORAGE_WRITE_BENCHMARKS: &[StorageWriteBenchmark] = &[
    StorageWriteBenchmark {
        name: "commit one entry",
        expected: 1,
        run: |storage| {
            commit_writes(
                storage,
                vec![entry(
                    storage.mint_id()?,
                    ROOT_CONVERSATION_ID,
                    "benchmark.write",
                    Some(json!({"text":"x".repeat(128)})),
                    None,
                )],
            )?;
            Ok(1)
        },
    },
    StorageWriteBenchmark {
        name: "commit 100 entries",
        expected: 100,
        run: |storage| {
            let payload = "x".repeat(128);
            batches(storage, 100, |index| {
                Ok(entry(
                    storage.mint_id()?,
                    ROOT_CONVERSATION_ID,
                    "benchmark.write",
                    Some(json!({"index":index,"text":payload})),
                    None,
                ))
            })?;
            Ok(100)
        },
    },
    StorageWriteBenchmark {
        name: "commit mixed entry/task/submission/document",
        expected: 4,
        run: |storage| {
            let entry_id = storage.mint_id()?;
            let task_id = storage.mint_id()?;
            let submission_id = storage.mint_id()?;
            let document_id = storage.mint_id()?;
            commit_writes(
                storage,
                vec![
                    entry(
                        entry_id,
                        ROOT_CONVERSATION_ID,
                        "benchmark.mixed",
                        None,
                        None,
                    ),
                    StorageWrite::Task {
                        value: task(task_id, task_id),
                    },
                    StorageWrite::Submission {
                        value: SubmissionRecord {
                            trace: None,
                            id: submission_id,
                            conversation_id: ROOT_CONVERSATION_ID,
                            request_id: Some(format!("benchmark-{submission_id}")),
                            notify: None,
                            kind: SubmissionKind::Write,
                            status: SubmissionStatus::Done,
                            entry: Some(entry_id),
                            answer: None,
                            reason: None,
                            detail: None,
                        },
                    },
                    document(
                        document_id,
                        "benchmark.mixed",
                        Some(document_id.to_string()),
                        false,
                        [
                            ("entryId".into(), json!(entry_id)),
                            ("taskId".into(), json!(task_id)),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                ],
            )?;
            Ok(4)
        },
    },
];
