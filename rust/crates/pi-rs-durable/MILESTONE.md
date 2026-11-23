# Milestone 2.2a — Durable foundation and session

Milestone 2.2a is the native Durable library; `packages/durable/src/harness/` belongs to 2.2b. Changes are confined to this crate. No package manifests, lockfile, upstream source, sibling crates, or port-map files were modified.

| Gate | Status | Evidence |
| --- | --- | --- |
| Formatting | Pass | `cargo fmt --manifest-path rust/crates/Cargo.toml -p pi-rs-durable -- --check` |
| Strict lint | Pass | Offline `cargo clippy -p pi-rs-durable --all-targets -- -D warnings` |
| Native behavior | Pass | Final offline `cargo test -p pi-rs-durable`: 77/77 contracts in 19.96 s, including 13 cases per storage backend and 29 cases against local `ExecutionEnv`. |
| Mutation proof | Pass (targeted) | 16 expected contract failures and one nontermination after breaking the recursive schema reference guard. That mutant runner was stopped; all 16 edited source files were restored byte-for-byte. |
| Upstream oracle | Pass (focused) | 35 search/truncation, 233 memory/SQLite/JSONL, 87 env/tools, and 33 standalone Chord StoreStorage cases. The upstream env/tools runner skipped 2 platform cases. |
| `rust-anything gate` | Not applicable to this native layout | The bridge gate requires a `rust-anything.json` and replay bridge; none was scaffolded under the permitted paths. The native tests are not a substitute for an exhaustive differential replay. |

Tests added: 77. Tests removed: 0. The starting library scaffold had no tests, so there is no comparable pre-port suite runtime. The 19.96 s is the test runner's time, excluding compilation and linking. The final Cargo invocation took 290.77 s wall time.

Mutation targets covered truncation boundaries, document versions, signal expiration and validation, exact-name ranking, timer fallback, task initialization, streaming BOM and line offsets, descendant cancellation, EXIF, reference traversal, codemode output and durable fingerprints, JSONL recovery, fork cutoff ownership, and publication receipts. The mutations were applied together; this is not a per-test mutation score for all 77 tests.

The upstream Chord Vitest entry points also import the 2.2b harness, which failed package resolution for `@earendil-works/pi-ai`. The standalone run imported the same upstream storage conformance suite and real Chord fixture without the harness.

## Implementation

- `documents`, `entries`, `signals`, `tasks`, `timers`, `truncate`, and `search`: typed registration, version checks and migrations, durable one-shot signal memos, host timer fallback, whole-line UTF-8 budgets, and BM25. Unicode normalization and dictionary data are pinned under `src/unicode/` to keep the Cargo manifest unchanged; licenses are there.
- Dictionary data: [ICU 78.1](https://github.com/unicode-org/icu/tree/release-78.1/icu4c/source/data/brkitr/dictionaries), sorted by UTF-8 bytes for direct lookup. The normalization tables are the used subset of [`unicode-normalization` 0.1.25](https://docs.rs/crate/unicode-normalization/0.1.25/source/src/tables.rs).
- Local `ExecutionEnv`: async file/dir readers, native or polling watches, argv and script execution, cancellation, bounded output, and spillage. Tools add canonical mutation locking, edit diffs, read limits, image detection/fitting, and deferred tool discovery.
- Codemode drives the QuickJS sandbox, real `pi-rs-ai` model registry, and the Session-backed run, journal, and store documents. Nested task execution remains a host callback; the scheduler is not in this milestone.
- Storage shares one record and commit-receipt contract. Native SQLite uses the v2 schema; Chord uses owner tables, document revisions, and the outbox; JSONL recovers confirmed file-backed commit prefixes. Session adds a serialized mutation line, ordered group publication, branch cutoffs, snapshots, document retention, and deferred progress writes.

## Fidelity boundaries

- The brief's four native storage backends are implemented. Browser IndexedDB and Cloudflare-specific SQLite/Photon bindings in the upstream tree are not native Rust backends. Windows and macOS runtime behavior was not exercised here.
- Standard Rust strings and `serde_json::Value` cannot represent lone UTF-16 surrogates. The native storage suite does not claim that part of the upstream lossless indexed-string test. It does exercise embedded NUL, supplementary Unicode, and prototype-like keys.
- The sibling Chord store does not expose live-query publication for custom transaction series, and it rejects explicit document creation at `MAX_SAFE_ID`. Durable publication and table IDs have separate test coverage.
- Image re-encoding is native, not byte-for-byte Photon; image contract tests check geometry, orientation, budgets, and unchanged bytes when no conversion is needed. Exact jsdiff hunk placement for ambiguous repeated lines and exact JavaScript event-loop scheduling are not established.

## 2.2b durable harness

The native Harness runs on Session storage. It opens and forks SQLite-backed conversations, drives model and tool tasks, settles submissions and compacts transcripts.

| Check | Result and scope |
| --- | --- |
| SQLite reopen and fork | Passed. Changing fork inheritance from as-of to current returned /second instead of /first. |
| Model and tool scheduling | The two-request test caught a scheduler deadlock. Independent Tokio dispatch tasks fixed it. Requiring more than one child to process a tool round made the contract fail. |
| Transcript | The two-window result matched the upstream TypeScript module, including the gap after entry 3, the next entry 7 and resume at 9. Adding one to the gap endpoint failed the contract. |
| Automatic compaction | The threshold case made two model requests. Overflow required three. Both wrote one summary between the first and second answers. Relabelling the summary entry failed the test. |
| Tool-round oracle | Both settled done with call-1 and two provider calls. Missing tool produced user, assistant, tool-result, assistant and exactly `<harness>\n[error] Tool unavailable is not available\n</harness>` (isError true). Registered echo inserted system between user and assistant and returned exactly `hello` (isError false). All listed entry kinds use the pi. prefix. |
| Native suite, format and lint | All 81 tests passed serially in 36.11 s, with a wall time of 374.88 s including compilation. Formatting and offline all-target Clippy passed. The 77-test foundation run took 19.96 s under different load. Four tests were added and zero removed. |
| Queued submission after a failed run | With a second submission waiting, keep left it queued, withdraw settled it unanswered with reason `run_failed`, and continue answered it. Changing the withdrawal reason failed the new test. After restoring it, all 82 tests passed serially in 72.60 s. After the scheduler and tool readability edits, the same 82 passed in 37.78 s. One test added, zero removed. Formatting and strict all-target Clippy passed for both revisions. |
| Formal differential replay | The native crate has no leaf bridge or recorded corpus. The checked comparisons above use the upstream transcript and tool-round implementations. |

### Conversation observation and telemetry

Window and task-graph observers report committed changes before the writer returns.
Document watches identify replacements from the ID and version read in the same
snapshot. Three mutations verified eviction splices, the terminal session-close
signal and replacement safety.

The upstream telemetry suite passed 27 cases. The native recorder observed four
write spans, six task phases including a resumed phase, balanced active counts,
and both storage and task failures. The model/tool test observed nested keyed
replay. Changing the storage-failure classification, phase histogram outcome
and replay event name failed the three corresponding contracts. External sink
delivery remains unverified.

The integrated native suite passed all 87 tests in 32.99 s of test execution,
up from 82 tests in 37.78 s on the preceding checkpoint. Five tests were added,
one extended, and zero removed. Offline all-target Clippy passed with warnings
denied. Cancellation, restart cutoffs and hook failure handling remain in progress.
